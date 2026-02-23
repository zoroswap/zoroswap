use anyhow::{Context, Result, anyhow};
use miden_assembly::Assembler;
use miden_client::account::component::BasicWallet;
use miden_client::account::{
    AccountBuilder, AccountComponent, AccountStorageMode, AccountType, StorageMap, StorageSlot,
    StorageSlotName,
};
use miden_client::auth::{AuthFalcon512Rpo, AuthSecretKey};
use miden_client::store::{AccountRecordData, TransactionFilter};
use miden_client::transaction::TransactionKernel;
use miden_client::{
    Felt, Word,
    account::{Account, AccountId},
    asset::FungibleAsset,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use rand::RngCore;
use std::path::Path;
use std::{collections::HashMap, str::FromStr};
use url::Url;
use zoro_miden::client::MidenClient;
use zoro_miden::pool::ZoroPool;
use zoro_miden::{MidenClient, create_basic_account, instantiate_simple_client, wait_for_note};
use zoroswap::{
    Config, PoolBalances, config::LiquidityPoolConfig, fetch_pool_state_from_chain,
    fetch_vault_for_account_from_chain, instantiate_client, oracle_sse::PriceMetadata,
};

pub struct MidenAccount {
    id: AccountId,
}

impl MidenAccount {
    pub fn id(&self) -> &AccountId {
        &self.id
    }

    pub async fn full(&self, client: &mut MidenClient) -> Result<Account> {
        let acc = client.get_account(*self.id()).await?.ok_or(anyhow!(
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
        Ok(acc.clone())
    }
}

pub struct UserAccount {
    account: MidenAccount,
}

impl UserAccount {
    pub async fn new(client: &mut MidenClient, keystore: FilesystemKeyStore) -> Result<Self> {
        let (account, _) = create_basic_account(client, keystore.clone()).await?;
        Ok(Self {
            account: MidenAccount { id: account.id() },
        })
    }
    pub async fn get_balance(
        &self,
        client: &mut MidenClient,
        faucet_id: &AccountId,
    ) -> Result<u64> {
        let acc = self.account.full(client).await?;
        let balance = acc.vault().get_balance(*faucet_id)?;
        Ok(balance)
    }
}

pub struct Faucet {
    account: MidenAccount,
    client: MidenClient,
}

pub struct DeployedPool {
    account: MidenAccount,
}

impl DeployedPool {
    pub async fn new(store_path: &str) -> Result<Self> {
        let config = Config::from_config_file(
            "../../config.toml",
            "../../masm",
            "../../keystore",
            store_path,
        )?;

        let keystore: FilesystemKeyStore = FilesystemKeyStore::new(config.keystore_path.into())?;
        let mut client =
            instantiate_simple_client(&config.keystore_path, &config.miden_endpoint).await?;
        let sync_summary = client.sync_state().await?;
        let pool_reader_path = format!("{}/accounts/zoropool.masm", config.masm_path);
        let pool_reader_path = Path::new(&pool_reader_path);
        let pool_code = std::fs::read_to_string(pool_reader_path)
            .unwrap_or_else(|err| panic!("unable to read from {pool_reader_path:?}: {err}"));

        let assembler: Assembler = TransactionKernel::assembler();

        let mut assets_mapping = StorageMap::new();
        let mut curves_mapping = StorageMap::new();
        let mut fees_mapping = StorageMap::new();

        for (i, pool) in config.liquidity_pools.iter().enumerate() {
            let fees: Word = [
                Felt::new(200), // swap_fee
                Felt::new(300), // backstop_fee
                Felt::new(0),   // protocol_fee
                Felt::new(0),   // 0
            ]
            .into();
            let curve: Word = [
                Felt::new(10000000000000000),        // beta
                Felt::new(16000000000000000000_u64), // c
                Felt::new(0),
                Felt::new(0),
            ]
            .into();
            let asset_index = [
                Felt::new(i as u64),
                Felt::new(0),
                Felt::new(0),
                Felt::new(0),
            ];
            let asset_id = [
                Felt::new(0),
                Felt::new(0),
                pool.faucet_id.suffix(),
                pool.faucet_id.prefix().as_felt(),
            ];
            assets_mapping
                .insert(asset_index.into(), asset_id.into())
                .unwrap_or_else(|err| panic!("Failed to insert asset into mapping: {err:?}"));
            fees_mapping
                .insert(asset_id.into(), fees)
                .unwrap_or_else(|err| panic!("Failed to insert fees into mapping: {err:?}"));
            curves_mapping
                .insert(asset_id.into(), curve)
                .unwrap_or_else(|err| panic!("Failed to insert curve into mapping: {err:?}"));
            let n = |name: &str| StorageSlotName::new(name).expect("valid slot name");

            let fees_mapping = StorageSlot::with_map(n("zoroswap::fees"), fees_mapping);
            let pool_states_mapping = StorageSlot::with_empty_map(n("zoroswap::pool_state"));
            let user_deposits_mapping = StorageSlot::with_empty_map(n("zoroswap::user_deposits"));

            // Compile the account code into a Library, then create AccountComponent
            let pool_library =
                zoro_miden::create_library(assembler.clone(), "zoroswap::zoropool", &pool_code)
                    .map_err(|e| anyhow!("Failed to create pool library: {e}"))?;
            let pool_component = AccountComponent::new(
                pool_library,
                vec![
                    StorageSlot::with_empty_value(n("zoroswap::slot0")),
                    StorageSlot::with_empty_value(n("zoroswap::slot1")),
                    StorageSlot::with_map(n("zoroswap::assets"), assets_mapping),
                    pool_states_mapping,
                    user_deposits_mapping,
                    StorageSlot::with_map(n("zoroswap::pool_curve"), curves_mapping),
                    fees_mapping,
                    StorageSlot::with_empty_value(n("zoroswap::pool_balances")),
                    StorageSlot::with_empty_value(n("zoroswap::lp_supply")),
                    StorageSlot::with_empty_value(n("zoroswap::pool_fees")),
                    StorageSlot::with_empty_value(n("zoroswap::slot10")),
                    StorageSlot::with_empty_value(n("zoroswap::slot11")),
                    StorageSlot::with_empty_value(n("zoroswap::slot12")),
                ],
            )?
            .with_supports_all_types();

            // Init seed for the pool contract
            let mut init_seed = [0_u8; 32];
            client.rng().fill_bytes(&mut init_seed);

            let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(client.rng());

            // Build the new `Account` with the component
            let pool_contract = AccountBuilder::new(init_seed)
                .account_type(AccountType::RegularAccountUpdatableCode)
                .storage_mode(AccountStorageMode::Public)
                .with_component(pool_component.clone())
                .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
                .with_component(BasicWallet) // is this actually needed? probably not
                .build()?;

            println!(
                "pool contract commitment hash: {:?}",
                pool_contract.commitment().to_hex()
            );
            println!(
                "contract id: {:?}",
                pool_contract
                    .id()
                    .to_bech32(config.miden_endpoint.to_network_id())
            );

            keystore.add_key(&key_pair)?;
            client.add_account(&pool_contract.clone(), false).await?;
            client.sync_state().await?;
        }

        Ok(Self {
            account: pool_contract.id(),
        })
    }

    pub async fn get_lp_balance(
        &self,
        client: &mut MidenClient,
        account_id: &AccountId,
        faucet_id: &AccountId,
    ) -> Result<u64> {
        let asset_address: Word = [
            account_id.suffix(),
            account_id.prefix().as_felt(),
            faucet_id.suffix(),
            faucet_id.prefix().as_felt(),
        ]
        .into();
        let lp_supply_slot = StorageSlotName::new("zoroswap::user_deposits")?;
        let lp_balance = self
            .account
            .full(client)
            .await?
            .storage()
            .get_map_item(&lp_supply_slot, asset_address)?;
        Ok(lp_balance[0].as_int())
    }
}

/// Common state for E2E tests.
pub struct E2ETestSetup {
    pub config: Config,
    pub client: MidenClient,
    pub keystore: FilesystemKeyStore,
    pub zoro_pool: ZoroPool,
}

impl E2ETestSetup {
    pub async fn new(store_path: &str) -> Result<Self> {
        dotenv::dotenv().ok();

        let config = Config::from_config_file(
            "../../config.toml",
            "../../masm",
            "../../keystore",
            store_path,
        )?;

        assert!(
            config.liquidity_pools.len() > 1,
            "Less than 2 liquidity pools configured"
        );

        let mut client = MidenClient::new(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_path,
            None,
        )
        .await?;
        let keystore = FilesystemKeyStore::new(config.keystore_path.into())?;
        client.sync_state().await?;
        let pools = config.liquidity_pools.clone();
        let zoro_pool =
            ZoroPool::new_from_existing_pool(&config.pool_account_id, config.liquidity_pools);
        Ok(E2ETestSetup {
            config,
            client,
            keystore,
            zoro_pool,
        })
    }
}

/// POST a serialized note to `{server_url}/{endpoint}/submit`.
pub async fn send_to_server(server_url: &str, note: String, endpoint: &str) -> Result<()> {
    let url = Url::from_str(format!("{server_url}/{endpoint}/submit").as_str())?;
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        .body(serde_json::json!({ "note_data": note }).to_string())
        .header("Content-Type", "application/json")
        .send()
        .await?;

    println!("Server response: {:?}", res.text().await?);
    Ok(())
}

/// Look up a single price by oracle ID from fetched price metadata.
pub fn extract_oracle_price(
    prices: &[PriceMetadata],
    oracle_id: &str,
    symbol: &str,
) -> Result<u64> {
    Ok(prices
        .iter()
        .find(|p| p.id.eq(oracle_id))
        .ok_or(anyhow!(
            "No price for {} ({}) on price oracle.",
            symbol,
            oracle_id
        ))?
        .price
        .price)
}

/// Build the 12-element input vector expected by the ZOROSWAP note script.
pub fn build_zoroswap_inputs(
    requested_asset_word: Word,
    deadline: u64,
    p2id_tag: NoteTag,
    beneficiary_id: AccountId,
    sender_id: AccountId,
) -> Vec<Felt> {
    vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(deadline),
        p2id_tag.into(),
        Felt::new(0), // padding (unused by note script)
        Felt::new(0), // padding (unused by note script)
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        sender_id.suffix(),
        sender_id.prefix().into(),
    ]
}

/// Fetch current pool states and assert they differ from the provided old values.
pub async fn assert_pool_states_changed(
    client: &mut MidenClient,
    pool_account_id: AccountId,
    pool0_faucet_id: AccountId,
    pool1_faucet_id: AccountId,
    old_balances_0: &PoolBalances,
    old_balances_1: &PoolBalances,
    old_vault: &HashMap<AccountId, u64>,
) -> Result<()> {
    let (new_balances_pool_0, _) =
        fetch_pool_state_from_chain(client, pool_account_id, pool0_faucet_id).await?;
    let (new_balances_pool_1, _) =
        fetch_pool_state_from_chain(client, pool_account_id, pool1_faucet_id).await?;
    let new_vault = fetch_vault_for_account_from_chain(client, pool_account_id).await?;

    println!("previous balances for liq pool 0: {old_balances_0:?}");
    println!("previous balances for liq pool 1: {old_balances_1:?}");
    println!("new balances for liq pool 0: {new_balances_pool_0:?}");
    println!("new balances for liq pool 1: {new_balances_pool_1:?}");
    println!("pool vault: {old_vault:?}");

    assert!(
        *old_balances_0 != new_balances_pool_0,
        "Balances for pool 0 havent changed"
    );
    assert!(
        *old_balances_1 != new_balances_pool_1,
        "Balances for pool 1 havent changed"
    );
    assert!(new_vault != *old_vault, "Vault hasn't changed");

    Ok(())
}
