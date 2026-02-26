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
    auth::{AuthFalcon512Rpo, AuthSecretKey},
    keystore::FilesystemKeyStore,
    note::{NoteRecipient, NoteTag, build_p2id_recipient},
    rpc::{Endpoint, GrpcClient, NodeRpcClient},
    store::AccountRecordData,
};
use rand::RngCore;
use tracing::{debug, warn};

use crate::client::MidenClient;

pub struct MidenAccount {
    id: AccountId,
    account: Option<Account>,
}

static FETCH_RPC: OnceLock<Arc<GrpcClient>> = OnceLock::new();

fn get_fetch_rpc() -> &'static Arc<GrpcClient> {
    FETCH_RPC.get_or_init(|| {
        // TODO: without env
        let miden_endpoint =
            env::var("MIDEN_NODE_ENDPOINT").expect("Missing MIDEN_NODE_ENDPOINT in .env file.");
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

    pub async fn deploy_new(miden_client: &mut MidenClient, keystore_path: &str) -> Result<Self> {
        let mut init_seed = [0_u8; 32];
        miden_client.client_mut().rng().fill_bytes(&mut init_seed);
        let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(miden_client.client_mut().rng());
        let builder = AccountBuilder::new(init_seed)
            .account_type(AccountType::RegularAccountUpdatableCode)
            .storage_mode(AccountStorageMode::Public)
            .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
            .with_component(BasicWallet);
        let account = builder.build().unwrap();
        miden_client
            .client_mut()
            .add_account(&account, false)
            .await?;
        let keystore = FilesystemKeyStore::new(keystore_path.into())?;
        keystore.add_key(&key_pair).unwrap();
        miden_client.client_mut().sync_state().await?;
        Ok(Self {
            id: account.id(),
            account: Some(account),
        })
    }

    pub async fn refetch_account(&mut self) -> Result<Account> {
        let rpc_client = get_fetch_rpc();
        let new_account = {
            rpc_client
                .get_account_details(self.id)
                .await?
                .account()
                .ok_or(anyhow!("Fetched account does not contain full account."))?
                .clone()
        };
        self.account = Some(new_account.clone());
        Ok(new_account)
    }

    pub fn id(&self) -> &AccountId {
        &self.id
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

    pub async fn full(&self, miden_client: &mut MidenClient) -> Result<AccountId> {
        let acc = miden_client
            .client()
            .get_account(*self.id())
            .await?
            .ok_or(anyhow!(
                "Account {} not found in local store",
                self.id().to_hex()
            ))?;
        let acc = match acc.account_data() {
            AccountRecordData::Full(a) => Ok(a),
            _ => Err(anyhow!(
                "Expected full account data for {}",
                self.id().to_hex()
            )),
        }?;
        Ok(acc.id())
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
        let recipient = build_p2id_recipient(self.id, p2id_serial_num)?;
        debug!("P2ID recipient digest: {:?}", recipient.digest());
        Ok(recipient)
    }

    pub async fn get_balance(&mut self, faucet_id: &AccountId) -> Result<u64> {
        let acc = self.account().await?;
        let balance = acc.vault().get_balance(*faucet_id)?;
        Ok(balance)
    }
}
