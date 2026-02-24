use std::{collections::HashMap, path::Path, str::FromStr};

use alloy::primitives::{I256, U256};
use anyhow::{Result, anyhow};
use miden_client::{
    Felt, Word,
    account::{
        Account, AccountBuilder, AccountComponent, AccountId, AccountStorageMode, AccountType,
        StorageMap, StorageSlot, StorageSlotName, component::BasicWallet,
    },
    asset::AssetVault,
    auth::{AuthFalcon512Rpo, AuthSecretKey},
    keystore::FilesystemKeyStore,
    rpc::Endpoint,
    transaction::TransactionKernel,
};
use rand::RngCore;

use crate::{
    account::MidenAccount,
    client::{MidenClient, create_library},
    pool_state::{PoolBalances, PoolMetadata, PoolSettings, PoolState},
};

#[derive(Debug, Clone, Copy)]
pub struct LiquidityPoolConfig {
    pub name: &'static str,
    pub symbol: &'static str,
    pub decimals: u8,
    pub faucet_id: AccountId,
    pub oracle_id: &'static str,
}

pub struct ZoroPool {
    miden_account: MidenAccount,
    pool_states: HashMap<AccountId, PoolState>,
    liquidity_pools: Vec<LiquidityPoolConfig>,
}

impl ZoroPool {
    pub fn pool_states(&self) -> &HashMap<AccountId, PoolState> {
        &self.pool_states
    }

    pub async fn new_from_existing_pool(
        pool_account_id: &AccountId,
        liquidity_pools: Vec<LiquidityPoolConfig>,
    ) -> Result<Self> {
        let miden_account = MidenAccount::new(*pool_account_id, None);
        let mut zoro_pool = Self {
            miden_account,
            pool_states: HashMap::with_capacity(liquidity_pools.len()),
            liquidity_pools,
        };
        zoro_pool.update_pool_state_from_chain().await?;
        Ok(zoro_pool)
    }

    pub async fn new_deployment(
        masm_path: &str,
        liquidity_pools: Vec<LiquidityPoolConfig>,
        miden_client: &mut MidenClient,
        endpoint: Endpoint,
        keystore: FilesystemKeyStore,
    ) -> Result<Self> {
        let pool_reader_path = format!("{}/accounts/zoropool.masm", masm_path);
        let pool_reader_path = Path::new(&pool_reader_path);
        let pool_code = std::fs::read_to_string(pool_reader_path)
            .unwrap_or_else(|err| panic!("unable to read from {pool_reader_path:?}: {err}"));

        let assembler = TransactionKernel::assembler();

        let mut assets_mapping = StorageMap::new();
        let mut curves_mapping = StorageMap::new();
        let mut fees_mapping = StorageMap::new();

        let pool_settings = PoolSettings {
            beta: I256::from_str("10000000000000000")?,
            c: I256::from_str("16000000000000000000")?,
            swap_fee: U256::from(200),
            backstop_fee: U256::from(300),
            protocol_fee: U256::from(0),
        };

        for (i, pool) in liquidity_pools.iter().enumerate() {
            let fees: Word = [
                Felt::new(pool_settings.swap_fee.to::<u64>()),
                Felt::new(pool_settings.backstop_fee.to::<u64>()),
                Felt::new(pool_settings.protocol_fee.to::<u64>()),
                Felt::new(0), // 0
            ]
            .into();
            let curve: Word = [
                Felt::new(pool_settings.beta.to_string().parse::<u64>()?),
                Felt::new(pool_settings.c.to_string().parse::<u64>()?),
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
        }

        let n = |name: &str| StorageSlotName::new(name).expect("valid slot name");

        let fees_mapping = StorageSlot::with_map(n("zoroswap::fees"), fees_mapping);
        let pool_states_mapping = StorageSlot::with_empty_map(n("zoroswap::pool_state"));
        let user_deposits_mapping = StorageSlot::with_empty_map(n("zoroswap::user_deposits"));

        // Compile the account code into a Library, then create AccountComponent
        let pool_library = create_library(assembler.clone(), "zoroswap::zoropool", &pool_code)
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
        miden_client.client_mut().rng().fill_bytes(&mut init_seed);

        let key_pair = AuthSecretKey::new_falcon512_rpo_with_rng(miden_client.client_mut().rng());

        // Build the new `Account` with the component
        let pool_contract = AccountBuilder::new(init_seed)
            .account_type(AccountType::RegularAccountUpdatableCode)
            .storage_mode(AccountStorageMode::Public)
            .with_component(pool_component.clone())
            .with_auth_component(AuthFalcon512Rpo::new(key_pair.public_key().to_commitment()))
            .with_component(BasicWallet)
            .build()?;

        println!(
            "pool contract commitment hash: {:?}",
            pool_contract.commitment().to_hex()
        );
        println!(
            "contract id: {:?}",
            pool_contract.id().to_bech32(endpoint.to_network_id())
        );

        keystore.add_key(&key_pair)?;
        miden_client
            .client_mut()
            .add_account(&pool_contract.clone(), false)
            .await?;
        miden_client.sync_state().await?;

        let mut pool_states = HashMap::with_capacity(liquidity_pools.len());
        for pool in liquidity_pools.iter() {
            pool_states.insert(
                pool.faucet_id,
                PoolState::new(pool_settings, PoolBalances::default(), 0, pool.into()),
            );
        }

        Ok(Self {
            miden_account: MidenAccount::new(pool_contract.id(), Some(pool_contract)),
            pool_states,
            liquidity_pools,
        })
    }

    pub async fn update_pool_state_from_chain(&mut self) -> Result<()> {
        let acc = self.miden_account.account().await?;
        for pool in self.liquidity_pools.iter() {
            let (settings, balances, lp_total_supply) =
                Self::extract_liqudity_pool_state_from_account(&acc, pool.faucet_id).await?;
            let pool_state = PoolState::new(
                settings,
                balances,
                lp_total_supply,
                PoolMetadata::from(pool),
            );
            self.pool_states.insert(pool.faucet_id, pool_state);
        }
        Ok(())
    }

    async fn extract_liqudity_pool_state_from_account(
        account: &Account,
        faucet_id: AccountId,
    ) -> Result<(PoolSettings, PoolBalances, u64)> {
        let storage = account.storage();
        let balances_slot = StorageSlotName::new("zoroswap::pool_state").expect("valid slot name");
        let curve_slot = StorageSlotName::new("zoroswap::pool_curve").expect("valid slot name");
        let fees_slot = StorageSlotName::new("zoroswap::fees").expect("valid slot name");
        let lp_supply_slot =
            StorageSlotName::new("zoroswap::user_deposits").expect("valid slot name");
        let asset_address: Word = [
            Felt::new(0),
            Felt::new(0),
            faucet_id.suffix(),
            faucet_id.prefix().as_felt(),
        ]
        .into();

        let pool_balances_raw = storage.get_map_item(&balances_slot, asset_address)?;
        let pool_curve = storage.get_map_item(&curve_slot, asset_address)?;
        let pool_fees = storage.get_map_item(&fees_slot, asset_address)?;

        let balances = PoolBalances {
            reserve_with_slippage: U256::from(pool_balances_raw[1].as_int()),
            reserve: U256::from(pool_balances_raw[2].as_int()),
            total_liabilities: U256::from(pool_balances_raw[3].as_int()),
        };
        let settings = PoolSettings {
            beta: I256::from_str(&pool_curve[0].as_int().to_string())?,
            c: I256::from_str(&pool_curve[1].as_int().to_string())?,
            swap_fee: U256::from(pool_fees[0].as_int()),
            backstop_fee: U256::from(pool_fees[1].as_int()),
            protocol_fee: U256::from(pool_fees[2].as_int()),
        };
        let total_supply_raw = storage.get_map_item(&lp_supply_slot, asset_address)?;
        let lp_total_supply = total_supply_raw[0].as_int();

        Ok((settings, balances, lp_total_supply))
    }

    pub async fn vault(&mut self) -> Result<AssetVault> {
        Ok(self.miden_account_mut().account().await?.vault().clone())
    }

    pub fn miden_account(&self) -> &MidenAccount {
        &self.miden_account
    }
    pub fn miden_account_mut(&mut self) -> &mut MidenAccount {
        &mut self.miden_account
    }

    pub fn print_pool_states(&self) {
        for pool in self.liquidity_pools.iter() {
            if let Some(pool_state) = self.pool_states.get(&pool.faucet_id) {
                pool_state.print_pool_state();
            }
        }
    }
}
