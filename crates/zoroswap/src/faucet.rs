use crate::{common::instantiate_faucet_client, config::Config};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    account::AccountId,
    asset::FungibleAsset,
    note::NoteType,
    store::{NoteFilter, TransactionFilter},
    sync::StateSync,
    transaction::TransactionRequestBuilder,
};
use std::collections::{BTreeSet, HashMap};
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info, trace, warn};
use zoro_miden_client::MidenClient;

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

        while let Some(mint_instruction) = self.rx.recv().await {
            let last_mint = self
                .recipients
                .get(&(mint_instruction.account_id, mint_instruction.faucet_id))
                .unwrap_or(&0);
            let can_mint = (Utc::now().timestamp() as u64) - last_mint > 1;
            trace!(
                "Faucet request for {} from faucet {}",
                mint_instruction.account_id.to_hex(),
                mint_instruction.faucet_id.to_hex()
            );
            if can_mint {
                // Sync BEFORE minting to ensure we have latest faucet state
                if let Err(e) =
                    Self::sync_state(&mut client, &state_sync, mint_instruction.faucet_id).await
                {
                    error!(
                        faucet = %mint_instruction.faucet_id.to_hex(),
                        error = ?e,
                        "Failed to sync before mint"
                    );
                    continue;
                }

                // Import the recipient account first
                if let Err(e) = client
                    .import_account_by_id(mint_instruction.account_id)
                    .await
                {
                    warn!("Note: account import returned: {e:?}");
                }
                let amount = 10000000;

                debug!(
                    "Minting {amount} for {} from faucet {}",
                    mint_instruction.account_id.to_hex(),
                    mint_instruction.faucet_id.to_hex()
                );

                match Self::mint_asset(
                    &mut client,
                    mint_instruction.faucet_id,
                    mint_instruction.account_id,
                    amount,
                )
                .await
                {
                    Ok(tx_id) => {
                        // Update timestamp after successful mint to enforce rate limiting
                        info!(
                            amount = amount,
                            faucet = %mint_instruction.faucet_id.to_hex(),
                            recipient = %mint_instruction.account_id.to_hex(),
                            tx_id = ?tx_id,
                            "Minted tokens"
                        );
                        self.recipients.insert(
                            (mint_instruction.account_id, mint_instruction.faucet_id),
                            Utc::now().timestamp() as u64,
                        );
                    }
                    Err(e) => {
                        error!(
                            faucet = %mint_instruction.faucet_id.to_hex(),
                            recipient = %mint_instruction.account_id.to_hex(),
                            error = ?e,
                            "Failed to mint asset"
                        );
                    }
                }

                // sync commitments
                Self::sync_state(&mut client, &state_sync, mint_instruction.faucet_id).await?;
            } else {
                debug!(
                    "Rate limited: {} from faucet {}",
                    mint_instruction.account_id.to_hex(),
                    mint_instruction.faucet_id.to_hex()
                );
            }
        }
        Ok(())
    }

    async fn mint_asset(
        client: &mut MidenClient,
        faucet_id: AccountId,
        recipient_id: AccountId,
        amount: u64,
    ) -> Result<String> {
        let fungible_asset = FungibleAsset::new(faucet_id, amount)?;
        let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            recipient_id,
            NoteType::Public,
            client.rng(),
        )?;
        let tx_id = client
            .submit_new_transaction(faucet_id, transaction_request)
            .await?;
        Ok(format!("{:?}", tx_id))
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
}
