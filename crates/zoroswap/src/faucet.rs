use crate::{common::instantiate_client, config::Config};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    account::AccountId,
    asset::FungibleAsset,
    note::NoteType,
    transaction::TransactionRequestBuilder,
};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};

pub struct FaucetMintInstruction {
    pub account_id: AccountId,
    pub faucet_id: AccountId,
}

pub struct GuardedFaucet {
    rx: mpsc::Receiver<FaucetMintInstruction>,
    recipients: HashMap<(AccountId, AccountId), u64>, // (user_id, faucet_id), timestamp
    config: Config,
}

impl GuardedFaucet {
    pub fn new(config: Config) -> (Self, mpsc::Sender<FaucetMintInstruction>) {
        let (tx, rx) = mpsc::channel(100);
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
        // Create our own client for faucet operations
        let mut client = instantiate_client(&self.config, self.config.store_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create faucet client: {e}"))?;

        while let Some(mint_instruction) = self.rx.recv().await {
            let last_mint = self
                .recipients
                .get(&(mint_instruction.account_id, mint_instruction.faucet_id))
                .unwrap_or(&0);
            let can_mint = (Utc::now().timestamp() as u64) - last_mint > 5;
            trace!(
                "Faucet request for {} from faucet {}",
                mint_instruction.account_id.to_hex(),
                mint_instruction.faucet_id.to_hex()
            );
            if can_mint {
                // Import the recipient account first
                if let Err(e) = client.import_account_by_id(mint_instruction.account_id).await {
                    warn!("Note: account import returned: {e}");
                }

                debug!(
                    "Minting 10000000 for {} from faucet {}",
                    mint_instruction.account_id.to_hex(),
                    mint_instruction.faucet_id.to_hex()
                );

                match Self::mint_asset(
                    &mut client,
                    mint_instruction.faucet_id,
                    mint_instruction.account_id,
                    10000000,
                )
                .await
                {
                    Ok(_tx_id) => {
                        // Update timestamp after successful mint to enforce rate limiting
                        self.recipients.insert(
                            (mint_instruction.account_id, mint_instruction.faucet_id),
                            Utc::now().timestamp() as u64,
                        );
                    }
                    Err(e) => {
                        error!("Error on minting from faucet: {e}");
                    }
                }
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
        client: &mut zoro_miden_client::MidenClient,
        faucet_id: AccountId,
        recipient_id: AccountId,
        amount: u64,
    ) -> Result<String> {
        client.sync_state().await?;

        let fungible_asset = FungibleAsset::new(faucet_id, amount)?;

        debug!(
            "Building mint TX for recipient {} from faucet {}",
            recipient_id.to_hex(),
            faucet_id.to_hex()
        );

        let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            recipient_id,
            NoteType::Public,
            client.rng(),
        )?;

        let tx_id = client
            .submit_new_transaction(faucet_id, transaction_request)
            .await
            .map_err(|e| {
                error!(
                    "Failed to submit mint transaction. Faucet: {}, Recipient: {}, Error: {:?}",
                    faucet_id.to_hex(),
                    recipient_id.to_hex(),
                    e
                );
                e
            })?;

        info!("Mint transaction submitted. TxID: {:?}", tx_id);
        client.sync_state().await?;

        Ok(format!("{:?}", tx_id))
    }
}
