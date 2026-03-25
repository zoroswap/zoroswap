use crate::config::Config;
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    Felt,
    account::AccountId,
    asset::{Asset, FungibleAsset},
    crypto::Rpo256,
    note::{Note, NoteAttachment, NoteType, create_p2id_note},
    transaction::{TransactionRequest, TransactionRequestBuilder, TransactionScript},
};
use std::collections::HashMap;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{error, info, warn};
use zoro_miden::{client::MidenClient, faucet::compile_mint_script};

#[derive(Copy, Clone)]
pub struct FaucetMintInstruction {
    pub account_id: AccountId,
    pub faucet_id: AccountId,
}

pub struct GuardedFaucet {
    rx: Receiver<FaucetMintInstruction>,
    recipients: HashMap<(AccountId, AccountId), u64>, // (user_id, faucet_id), timestamp
    config: Config,
}

impl GuardedFaucet {
    pub fn new(config: Config) -> (Self, Sender<FaucetMintInstruction>) {
        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let recipients = HashMap::new();
        (
            Self {
                rx,
                recipients,
                config,
            },
            tx,
        )
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut client = MidenClient::new(
            self.config.miden_endpoint.clone(),
            self.config.keystore_path,
            self.config.store_dir,
        )
        .await?;
        let limit = 50;
        let amount = 10000000;
        let mut instructions = Vec::with_capacity(limit);
        let tx_script = compile_mint_script()?;
        for pool in self.config.liquidity_pools.iter() {
            client.import_account(&pool.faucet_id).await?;
            info!("Faucet {} imported.", pool.name);
        }

        loop {
            let n_mints = self.rx.recv_many(&mut instructions, limit).await;
            for pool in self.config.liquidity_pools.iter() {
                client.partial_sync_state(&pool.faucet_id).await?;
                let mut notes = Vec::with_capacity(n_mints);
                let instructions_for_faucet: Vec<FaucetMintInstruction> = instructions
                    .iter()
                    .filter_map(|i| {
                        if i.faucet_id.eq(&pool.faucet_id) {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .collect();
                for mint_instruction in instructions_for_faucet.iter() {
                    let last_mint = self
                        .recipients
                        .get(&(mint_instruction.account_id, mint_instruction.faucet_id))
                        .unwrap_or(&0);
                    let can_mint = (Utc::now().timestamp() as u64) - last_mint > 120;
                    if can_mint
                        && let Err(e) = client
                            .partial_sync_state(&mint_instruction.account_id)
                            .await
                    {
                        warn!(error = ?e,"Error partially syncing acc into faucet");
                    } else if can_mint
                        && let Ok(note) = GuardedFaucet::create_p2id_from_instruction(
                            *mint_instruction,
                            amount,
                            &mut client,
                        )
                        .await
                    {
                        notes.push(note)
                    }
                }
                if let Ok(tx_req) = Self::create_transaction(&notes, tx_script.clone()) {
                    if let Err(e) = client
                        .client_mut()
                        .submit_new_transaction(pool.faucet_id, tx_req)
                        .await
                    {
                        error!(error = ?e, "Error on submiting mint tx");
                    } else {
                        client.partial_sync_state(&pool.faucet_id).await?;
                    }
                }
            }
        }
    }

    async fn create_p2id_from_instruction(
        mint_instruction: FaucetMintInstruction,
        amount: u64,
        client: &mut MidenClient,
    ) -> Result<Note> {
        let asset = FungibleAsset::new(mint_instruction.faucet_id, amount)?;
        let note = create_p2id_note(
            mint_instruction.faucet_id,
            mint_instruction.account_id,
            vec![Asset::Fungible(asset)],
            NoteType::Public,
            NoteAttachment::default(),
            client.client_mut().rng(),
        )?;
        Ok(note)
    }

    fn create_transaction(notes: &[Note], script: TransactionScript) -> Result<TransactionRequest> {
        let expected_output_recipients = notes.iter().map(Note::recipient).cloned().collect();
        let n = notes.len() as u64;
        let mut note_data = vec![Felt::new(n)];
        for note in notes {
            let amount = note
                .assets()
                .iter()
                .next()
                .unwrap()
                .unwrap_fungible()
                .amount();
            note_data.extend(note.recipient().digest().iter());
            note_data.push(Felt::from(note.metadata().note_type()));
            note_data.push(Felt::from(note.metadata().tag()));
            note_data.push(Felt::new(amount));
        }
        let note_data_commitment = Rpo256::hash_elements(&note_data);
        let advice_map = [(note_data_commitment, note_data)];
        let tx_req = TransactionRequestBuilder::new()
            .custom_script(script.clone())
            .extend_advice_map(advice_map)
            .expected_output_recipients(expected_output_recipients)
            .script_arg(note_data_commitment)
            .build()?;
        Ok(tx_req)
    }
}
