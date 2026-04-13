use std::{
    env,
    sync::{Arc, OnceLock},
};

use anyhow::{Result, anyhow};
use miden_client::{
    Felt, Word,
    account::{
        Account, AccountBuilder, AccountId, AccountStorageMode, AccountType, component::BasicWallet,
    },
    address::NetworkId,
    asset::Asset,
    auth::{AuthScheme, AuthSecretKey, AuthSingleSig},
    keystore::{FilesystemKeyStore, Keystore},
    note::{NoteRecipient, NoteTag},
    rpc::{Endpoint, GrpcClient, NodeRpcClient},
    store::TransactionFilter,
    transaction::{TransactionRequestBuilder, TransactionStatus},
};
use miden_standards::note::P2idNoteStorage;
use rand::RngCore;
use tracing::{debug, info, warn};

use crate::client::MidenClient;

#[derive(Debug, Clone)]
pub struct MidenAccount {
    id: AccountId,
    account: Option<Account>,
}

static FETCH_RPC: OnceLock<Arc<GrpcClient>> = OnceLock::new();

fn get_fetch_rpc() -> &'static Arc<GrpcClient> {
    FETCH_RPC.get_or_init(|| {
        let miden_endpoint = env::var("MIDEN_NODE_ENDPOINT").unwrap_or("localhost".to_string());
        let miden_endpoint = match miden_endpoint.as_str() {
            "testnet" => Endpoint::testnet(),
            "devnet" => Endpoint::devnet(),
            _ => Endpoint::localhost(),
        };
        Arc::new(GrpcClient::new(&miden_endpoint, 30_000))
    })
}

impl MidenAccount {
    pub fn new(id: AccountId, account: Option<Account>) -> Self {
        Self { id, account }
    }

    /// Deploys a new account and registeres it on the Node with an empty TX
    pub async fn deploy_new(miden_client: &mut MidenClient, keystore_path: &str) -> Result<Self> {
        let mut init_seed = [0_u8; 32];
        miden_client.client_mut().rng().fill_bytes(&mut init_seed);
        let key_pair = AuthSecretKey::new_ecdsa_k256_keccak();
        let builder = AccountBuilder::new(init_seed)
            .account_type(AccountType::RegularAccountUpdatableCode)
            .storage_mode(AccountStorageMode::Public)
            .with_auth_component(AuthSingleSig::new(
                key_pair.public_key().to_commitment(),
                AuthScheme::EcdsaK256Keccak,
            ))
            .with_component(BasicWallet);
        let account = builder.build()?;
        miden_client
            .client_mut()
            .add_account(&account, false)
            .await?;
        let keystore = FilesystemKeyStore::new(keystore_path.into())?;
        keystore.add_key(&key_pair, account.id()).await.map_err(|e| anyhow!("Failed to add key: {e:?}"))?;
        miden_client.client_mut().sync_state().await?;

        let mut acc = Self {
            id: account.id(),
            account: Some(account),
        };
        acc.register_on_node(miden_client).await?;
        info!(
            "Account {} initialized in client & registered on node.",
            acc.id.to_bech32(miden_client.network_id())
        );
        Ok(acc)
    }

    /// Triggers an empty TX on this Account and waits until its finalized in a block so its visible by RPC
    pub async fn register_on_node(&mut self, miden_client: &mut MidenClient) -> Result<()> {
        let note_req = TransactionRequestBuilder::new().build().unwrap();
        let tx_id = miden_client
            .client_mut()
            .submit_new_transaction(*self.id(), note_req)
            .await?;
        loop {
            miden_client.sync_state().await?;
            let txs = miden_client
                .client()
                .get_transactions(TransactionFilter::Ids(vec![tx_id]))
                .await?;
            if let Some(tx) = txs.first() {
                match tx.status {
                    TransactionStatus::Pending => continue,
                    TransactionStatus::Discarded(_) => {
                        return Err(anyhow!(
                            "Registering the account {} failed. Initial Tx was discarded.",
                            self.id.to_bech32(miden_client.network_id())
                        ));
                    }
                    TransactionStatus::Committed {
                        block_number: _,
                        commit_timestamp: _,
                    } => return Ok(()),
                }
            }
        }
    }

    pub async fn refetch_account(&mut self) -> Result<Account> {
        let rpc_client = get_fetch_rpc();
        let acc = rpc_client.get_account_details(self.id).await?;
        let acc = acc.account().ok_or(anyhow!(
            "Missing account data for: {}",
            self.id.to_bech32(Endpoint::localhost().to_network_id())
        ))?;
        self.set_account(acc.clone());
        Ok(acc.clone())
    }

    pub fn id(&self) -> &AccountId {
        &self.id
    }

    pub fn set_account(&mut self, account: Account) {
        self.account = Some(account)
    }

    pub async fn account(&mut self) -> Result<Account> {
        // TODO: find out if this can be avoided, if we can somehow sync just what we need, not downaload it over and over again
        match self.refetch_account().await {
            Ok(acc) => Ok(acc),
            Err(e) => {
                warn!("Error fetching account from RPC: {e:?}");
                self.account.clone().ok_or(anyhow!(
                    "Missing Account on Miden Account for id: {}",
                    self.id.to_hex()
                ))
            }
        }
    }

    pub fn tag(&self) -> NoteTag {
        NoteTag::with_account_target(self.id)
    }

    pub fn zoro_swap_p2id_recipient(&self, swap_serial_num: Word) -> Result<NoteRecipient> {
        // Cincrement first element by 1, like in zoro notes
        let p2id_serial_num: Word = [
            swap_serial_num[0] + Felt::new(1),
            swap_serial_num[1],
            swap_serial_num[2],
            swap_serial_num[3],
        ]
        .into();
        debug!("P2ID beneficiary id: {:?}", self.id);
        debug!("P2ID serial num: {:?}", p2id_serial_num);
        let recipient = P2idNoteStorage::new(self.id).into_recipient(p2id_serial_num);
        debug!("P2ID recipient digest: {:?}", recipient.digest());
        Ok(recipient)
    }

    pub async fn get_balance(&mut self, faucet_id: &AccountId) -> Result<u64> {
        let acc = self.account().await?;
        let balance = acc.vault().get_balance(*faucet_id)?;
        Ok(balance)
    }

    pub async fn print_vault(&mut self, network_id: NetworkId) -> Result<()> {
        let vault = self.account().await?.vault().clone();
        info!("Account {} assets:", self.id.to_bech32(network_id.clone()));
        for asset in vault.assets() {
            match asset {
                Asset::Fungible(asset) => {
                    info!(
                        "Asset {}, balance: {}",
                        asset.faucet_id().to_bech32(network_id.clone()),
                        asset.amount()
                    );
                }
                Asset::NonFungible(asset) => {
                    info!("Asset {}", asset.vault_key().to_string());
                }
            }
        }
        Ok(())
    }
}
