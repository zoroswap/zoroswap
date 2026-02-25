use crate::{
    config::Config,
    oracle_sse::PriceMetadata,
    order::Order,
    websocket::{EventBroadcaster, messages::PoolStateEvent},
};
use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use chrono::Utc;
use dashmap::DashMap;
use miden_client::{
    account::AccountId,
    note::{Note, NoteId},
};
use std::{collections::HashMap, sync::Arc};
use tracing::{error, info};
use uuid::Uuid;
use zoro_miden::{note::TrustedNote, pool_state::PoolState, price::PriceData};

pub struct AmmState {
    open_orders: DashMap<Uuid, Order>,
    closed_orders: DashMap<Uuid, Order>,
    notes: DashMap<Uuid, TrustedNote>,
    note_ids: DashMap<Uuid, String>,
    liquidity_pools: DashMap<AccountId, PoolState>,
    oracle_prices: DashMap<AccountId, PriceData>,
    config: Arc<Config>,
    broadcaster: Arc<EventBroadcaster>,
}

impl AmmState {
    pub async fn new(config: Config, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self {
            open_orders: DashMap::new(),
            closed_orders: DashMap::new(),
            notes: DashMap::new(),
            note_ids: DashMap::new(),
            liquidity_pools: DashMap::new(),
            config: Arc::new(config),
            oracle_prices: DashMap::new(),
            broadcaster,
        }
    }

    pub fn add_order(&self, note: Note) -> Result<(String, Uuid, Order)> {
        let hex = note.id().to_hex();
        let note = TrustedNote::from_note(note.clone())?;
        let order = Order::from_trusted_note(note.clone())?;
        let order_id = order.id;
        self.note_ids.insert(order_id, hex.clone());
        self.notes.insert(order_id, note);
        self.open_orders.insert(order_id, order);
        Ok((hex, order_id, order))
    }

    pub fn get_order_id(&self, note_id: &NoteId) -> Option<Uuid> {
        // Look up from stored note_ids map (notes might be consumed during execution)
        let note_id_hex = note_id.to_hex();
        self.note_ids.iter().find_map(|i| {
            if i.value().eq(&note_id_hex) {
                Some(*i.key())
            } else {
                None
            }
        })
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
                    error!(
                        "Failed to map oracle id '{}' to faucet id: {e:?}",
                        price_update.id
                    )
                }
            }
        }
    }

    pub fn set_pool_states(&self, new_pool_state: HashMap<AccountId, PoolState>) {
        let timestamp = Utc::now().timestamp_millis() as u64;
        for (faucet_id, new_pool_state) in new_pool_state.iter() {
            let _ = self.broadcaster.broadcast_pool_state(PoolStateEvent {
                faucet_id: faucet_id.to_bech32(self.config.network_id.clone()),
                balances: *new_pool_state.balances(),
                timestamp,
            });
            self.liquidity_pools.insert(*faucet_id, *new_pool_state);
        }
    }

    pub fn config(&self) -> Config {
        (*self.config).clone()
    }

    pub fn liquidity_pools(&self) -> &DashMap<AccountId, PoolState> {
        &self.liquidity_pools
    }

    pub fn oracle_prices(&self) -> &DashMap<AccountId, PriceData> {
        &self.oracle_prices
    }

    /// Clones a note without removing it from state.
    pub fn get_note(&self, id: &Uuid) -> Result<TrustedNote> {
        self.notes
            .get(id)
            .map(|n| n.clone())
            .ok_or(anyhow!("No note found for id {id} in state."))
    }

    /// Check if oracle prices for specified tokens are fresh (within max_age_secs).
    /// Only checks tokens matching the given `oracle_ids`.
    ///
    /// Returns `None` if prices are fresh. Otherwise, `Some(age_secs)`
    /// of the oldest stale price.
    pub fn oldest_stale_price(&self, max_age_secs: u64, oracle_ids: &[&str]) -> Option<u64> {
        let now = Utc::now().timestamp() as u64;
        let mut oldest_age: Option<u64> = None;

        let faucet_ids: Vec<AccountId> = oracle_ids
            .iter()
            .filter_map(|oracle_id| self.config.oracle_id_to_faucet_id(oracle_id).ok())
            .collect();

        for faucet_id in faucet_ids {
            if let Some(price_data) = self.oracle_prices.get(&faucet_id) {
                let age = now.saturating_sub(price_data.timestamp);
                if age > max_age_secs {
                    oldest_age = Some(oldest_age.map_or(age, |old| old.max(age)));
                }
            }
        }

        oldest_age
    }
}
