use crate::config::Config;
use anyhow::Result;
use chrono::Utc;
use miden_client::account::AccountId;
use std::collections::HashMap;
use tokio::sync::mpsc::{Receiver, Sender};
use tracing::{debug, error, info};
use zoro_miden::client::MidenClient;

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
        let mut clients: HashMap<AccountId, MidenClient> =
            HashMap::with_capacity(self.config.liquidity_pools.len());
        for liq_pool in self.config.liquidity_pools.iter() {
            let client = MidenClient::new(
                self.config.miden_endpoint.clone(),
                self.config.keystore_path,
                self.config.store_path,
                Some(liq_pool.faucet_id),
            )
            .await?;
            clients.insert(liq_pool.faucet_id, client);
        }

        while let Some(mint_instruction) = self.rx.recv().await {
            let last_mint = self
                .recipients
                .get(&(mint_instruction.account_id, mint_instruction.faucet_id))
                .unwrap_or(&0);
            let can_mint = (Utc::now().timestamp() as u64) - last_mint > 1;
            debug!(
                "Faucet request for {} from faucet {}",
                mint_instruction.account_id.to_hex(),
                mint_instruction.faucet_id.to_hex()
            );
            if can_mint {
                let amount = 10000000;
                let miden_client = clients.iter_mut().find_map(|(faucet_id, client)| {
                    if (*faucet_id).eq(&mint_instruction.faucet_id) {
                        Some(client)
                    } else {
                        None
                    }
                });
                if let Some(miden_client) = miden_client {
                    match miden_client
                        .mint_asset(
                            mint_instruction.faucet_id,
                            mint_instruction.account_id,
                            amount,
                        )
                        .await
                    {
                        Ok(tx_id) => {
                            info!(
                                amount = amount,
                                faucet = %mint_instruction.faucet_id.to_hex(),
                                recipient = %mint_instruction.account_id.to_hex(),
                                tx_id = ?tx_id,
                                "Minted tokens"
                            );
                            // Update timestamp after successful mint to enforce rate limiting
                            self.recipients.insert(
                                (mint_instruction.account_id, mint_instruction.faucet_id),
                                Utc::now().timestamp() as u64,
                            );
                            let _ = miden_client.sync_state().await;
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
}
