use crate::{Config, ZoroStorageSettings, common::instantiate_client};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    account::AccountId, asset::FungibleAsset, note::NoteType,
    transaction::TransactionRequestBuilder,
};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use zoro_miden_client::MidenClient;

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
        let mut client = instantiate_client(
            &self.config,
            ZoroStorageSettings::faucet_storage(self.config.store_path.to_string()),
        )
        .await?;
        while let Some(mint_instruction) = self.rx.recv().await {
            let last_mint = self
                .recipients
                .get(&(mint_instruction.account_id, mint_instruction.faucet_id))
                .unwrap_or(&0);
            let can_mint = (Utc::now().timestamp() as u64) - last_mint > 5;
            info!(
                "Minting 10000000 for {} from faucet {}.",
                mint_instruction
                    .account_id
                    .to_bech32(self.config.network_id.clone()),
                mint_instruction
                    .faucet_id
                    .to_bech32(self.config.network_id.clone())
            );
            if can_mint
                && let Err(e) = simple_mint(
                    mint_instruction.account_id,
                    mint_instruction.faucet_id,
                    10000000,
                    &mut client,
                )
                .await
            {
                error!("Error on minting from faucet: {e}")
            }
        }
        Ok(())
    }
}

pub async fn simple_mint(
    account_id: AccountId,
    faucet_id: AccountId,
    amount: u64,
    client: &mut MidenClient,
) -> Result<()> {
    if let Err(e) = client.import_account_by_id(account_id).await {
        warn!("Error importing acccount by id {account_id:?}: {e:?}");
    }
    client.sync_state().await?;
    let fungible_asset = FungibleAsset::new(faucet_id, amount)?;
    info!("Building mint TX.");
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        account_id,
        NoteType::Public,
        client.rng(),
    )?;
    info!("Sending mint TX to the chain.");
    let _tx_id = client
        .submit_new_transaction(faucet_id, transaction_request)
        .await?;
    client.sync_state().await?;
    Ok(())
}
