use crate::{
    config::Config,
    oracle_sse::{PriceData, PriceMetadata},
    order::Order,
    pool::{PoolBalances, PoolState},
};
use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use dashmap::DashMap;
use miden_client::{account::AccountId, note::Note};
use miden_lib::account::faucets::BasicFungibleFaucet;
use std::sync::Arc;
use tracing::{error, info};
use uuid::Uuid;
use zoro_miden_client::{MidenClient, instantiate_simple_client};

pub struct AmmState {
    open_orders: DashMap<Uuid, Order>,
    closed_orders: DashMap<Uuid, Order>,
    notes: DashMap<Uuid, Note>,
    liquidity_pools: DashMap<AccountId, PoolState>,
    oracle_prices: DashMap<AccountId, PriceData>,
    faucet_metadata: DashMap<AccountId, BasicFungibleFaucet>,
    config: Arc<Config>,
}

impl AmmState {
    pub async fn new(config: Config) -> Self {
        Self {
            open_orders: DashMap::new(),
            closed_orders: DashMap::new(),
            notes: DashMap::new(),
            liquidity_pools: DashMap::new(),
            config: Arc::new(config),
            oracle_prices: DashMap::new(),
            faucet_metadata: DashMap::new(),
        }
    }

    pub async fn init_liquidity_pool_states(&self, client: &mut MidenClient) -> Result<()> {
        info!("Starting initiation of liq pool states.");
        for (index, liq_pool_config) in self.config.liquidity_pools.iter().enumerate() {
            let mut new_liq_pool_state =
                PoolState::new(self.config.pool_account_id, liq_pool_config.faucet_id);
            {
                new_liq_pool_state
                    .sync_from_chain(client, index as u8)
                    .await?;
            }
            self.liquidity_pools
                .insert(liq_pool_config.faucet_id, new_liq_pool_state);
        }
        info!("Successfully initiated / synced pool states from the chain.");
        Ok(())
    }

    pub async fn init_faucet_metadata(&self) -> Result<()> {
        let mut miden_client =
            instantiate_simple_client(self.config.keystore_path, &self.config.miden_endpoint)
                .await?;
        for liq_pool in self.config.liquidity_pools.iter() {
            let _ = miden_client
                .import_account_by_id(liq_pool.faucet_id)
                .await?;
            if let Some(acc) = miden_client.get_account(liq_pool.faucet_id).await? {
                let faucet: BasicFungibleFaucet = BasicFungibleFaucet::try_from(acc.account())?;
                self.faucet_metadata.insert(liq_pool.faucet_id, faucet);
            }
        }
        Ok(())
    }

    pub fn add_order(&self, note: Note) -> Result<String> {
        let note_id = note.id().to_hex();
        let order = Order::from_note(&note)?;
        let order_id = order.id;
        self.notes.insert(order_id, note);
        self.open_orders.insert(order_id, order);
        Ok(note_id)
    }

    pub fn get_open_orders(&self) -> Vec<Order> {
        self.open_orders.iter().map(|i| *i.value()).collect()
    }

    pub fn get_closed_orders(&self) -> Vec<Order> {
        self.closed_orders.iter().map(|i| *i.value()).collect()
    }

    pub fn flush_open_orders(&self) -> Vec<Order> {
        let orders = self.get_open_orders();
        self.open_orders.clear();
        orders
    }

    pub fn update_oracle_prices(&self, updates: Vec<PriceMetadata>) {
        for price_update in updates {
            match self.config.oracle_id_to_faucet_id(&price_update.id) {
                Ok(faucet_id) => {
                    self.oracle_prices.insert(
                        faucet_id,
                        PriceData::new(price_update.price.publish_time, price_update.price.price),
                    );
                }
                Err(e) => {
                    error!("{e}")
                }
            }
        }
    }

    pub fn update_pool_state(&self, pool_account_id: &AccountId, new_pool_balances: PoolBalances) {
        if let Some(mut liq_pool) = self.liquidity_pools.get_mut(pool_account_id) {
            liq_pool.update_state(new_pool_balances);
        };
    }

    pub fn config(&self) -> Config {
        (*self.config).clone()
    }

    pub fn liquidity_pools(&self) -> &DashMap<AccountId, PoolState> {
        &self.liquidity_pools
    }

    pub fn faucet_metadata(&self) -> &DashMap<AccountId, BasicFungibleFaucet> {
        &self.faucet_metadata
    }

    pub fn pluck_note(&self, id: &Uuid) -> Result<(Uuid, Note)> {
        self.notes
            .remove(id)
            .ok_or(anyhow!("No note found for id {id} in state."))
    }

    pub fn oracle_price_for_pair(
        &self,
        faucet_in: AccountId,
        faucet_out: AccountId,
    ) -> Result<U256> {
        let price_in = self
            .oracle_prices
            .get(&faucet_in)
            .ok_or(anyhow!("No oracle price found for faucet id: {faucet_in}"))?;
        let price_out = self
            .oracle_prices
            .get(&faucet_out)
            .ok_or(anyhow!("No oracle price found for faucet id: {faucet_out}"))?;
        let price = price_in.price as f64 / price_out.price as f64;
        let price = U256::from(price * 1e12);
        info!(
            "Price for swap: {price:?}. Price in: {}, Price out: {}",
            price_in.price, price_out.price
        );
        Ok(price)
    }
}
