use crate::{common::instantiate_faucet_client, config::Config};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    account::AccountId, asset::FungibleAsset, note::NoteType,
    transaction::TransactionRequestBuilder,
};
use std::collections::HashMap;
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
        // Create our own client for faucet operations
        let mut client =
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

                // Sync state after minting
                client.sync_state().await?;
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
}
