use crate::{common::instantiate_faucet_client, config::Config};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    Felt,
    account::AccountId,
    assembly::CodeBuilder,
    asset::{Asset, FungibleAsset},
    note::{Note, NoteAttachment, NoteType, create_p2id_note},
    store::{NoteFilter, TransactionFilter},
    sync::StateSync,
    transaction::{TransactionRequest, TransactionRequestBuilder, TransactionScript},
};
use miden_core::crypto::hash::Rpo256;
use std::collections::{BTreeSet, HashMap};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::error;
use zoro_miden_client::MidenClient;

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
        // Create our own client for faucet operations (with retry for DB contention)
        let (mut client, state_sync) =
            instantiate_faucet_client(self.config.clone(), self.config.store_path).await?;
        let limit = 50;
        let amount = 10000000;
        let mut instructions = Vec::with_capacity(limit);
        let masm_file_path = format!("{}/scripts/mint.masm", self.config.masm_path);
        let tx_script = CodeBuilder::new().compile_tx_script(masm_file_path.clone())?;

        loop {
            let n_mints = self.rx.recv_many(&mut instructions, limit).await;
            for pool in self.config.liquidity_pools.iter() {
                Self::sync_state(&mut client, &state_sync, pool.faucet_id).await?;
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
                    if let Err(e) = client.submit_new_transaction(pool.faucet_id, tx_req).await {
                        error!("Error on submiting mint tx: {e}");
                    } else {
                        Self::sync_state(&mut client, &state_sync, pool.faucet_id).await?;
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
            client.rng(),
        )?;
        Ok(note)
    }

    /// Syncs only the faucet account's commitments instead of calling `client.sync_state()`.
    /// A full sync is too slow for fat faucet accounts (can take 30+ minutes).
    /// This is analogous to how the official Miden faucet handles syncing:
    /// https://github.com/0xMiden/miden-faucet/blob/main/crates/faucet/src/lib.rs
    async fn sync_state(
        client: &mut MidenClient,
        state_sync: &StateSync,
        faucet_id: AccountId,
    ) -> Result<()> {
        let accounts = client
            .get_account_header_by_id(faucet_id)
            .await?
            .map(|(header, _)| vec![header])
            .unwrap_or_default();
        let note_tags = BTreeSet::new();
        let input_notes = vec![];
        let expected_output_notes = client.get_output_notes(NoteFilter::Expected).await?;
        let uncommitted_transactions = client
            .get_transactions(TransactionFilter::Uncommitted)
            .await?;

        // Build current partial MMR
        let current_partial_mmr = client.get_current_partial_mmr().await?;

        // Get the sync update from the network
        let state_sync_update = state_sync
            .sync_state(
                current_partial_mmr,
                accounts,
                note_tags,
                input_notes,
                expected_output_notes,
                uncommitted_transactions,
            )
            .await?;

        // Apply received and computed updates to the store
        client.apply_state_sync(state_sync_update).await?;
        Ok(())
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
