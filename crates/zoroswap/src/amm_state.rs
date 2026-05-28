use crate::{
    config::Config,
    oracle_sse::PriceMetadata,
    order::Order,
    websocket::{EventBroadcaster, messages::PoolStateEvent},
};
use anyhow::{Result, anyhow};
use chrono::Utc;
use dashmap::DashMap;
use miden_client::{Felt, account::AccountId, asset::FungibleAsset, note::NoteId};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::error;
use uuid::Uuid;
use zoro_miden::{
    asset_utils::asset_to_word,
    note::{NoteInstructions, TrustedNote},
    pool_state::PoolState,
    price::PriceData,
};

pub struct AmmState {
    open_orders: DashMap<Uuid, Order>,
    closed_orders: DashMap<Uuid, Order>,
    positions: DashMap<Uuid, TrustedNote>,
    notes: DashMap<Uuid, TrustedNote>,
    note_ids: DashMap<Uuid, String>,
    liquidity_pools: DashMap<AccountId, PoolState>,
    oracle_prices: DashMap<AccountId, PriceData>,
    config: Arc<Config>,
    broadcaster: Arc<EventBroadcaster>,
    valid_faucets: HashSet<AccountId>,
}

impl AmmState {
    pub async fn new(config: Config, broadcaster: Arc<EventBroadcaster>) -> Self {
        let valid_faucets: HashSet<AccountId> =
            config.liquidity_pools.iter().map(|p| p.faucet_id).collect();
        Self {
            open_orders: DashMap::new(),
            closed_orders: DashMap::new(),
            notes: DashMap::new(),
            note_ids: DashMap::new(),
            positions: DashMap::new(),
            liquidity_pools: DashMap::new(),
            config: Arc::new(config),
            oracle_prices: DashMap::new(),
            broadcaster,
            valid_faucets,
        }
    }

    pub fn add_position_order(
        &self,
        position_id: Uuid,
        asset_in: String,
        asset_out: String,
        amount_in: u64,
        amount_out: u64,
    ) -> Result<(String, Uuid, Order)> {
        let note = self
            .positions
            .get(&position_id)
            .ok_or(anyhow!("No note found for position {}", position_id))?
            .clone();
        let (_, asset_in) = AccountId::from_bech32(&asset_in)?;
        let asset_in = FungibleAsset::new(asset_in, amount_in)?;
        let asset_in = asset_to_word(asset_in);
        let (_, asset_out) = AccountId::from_bech32(&asset_out)?;
        let asset_out = FungibleAsset::new(asset_out, amount_out)?;
        let asset_out = asset_to_word(asset_out);
        let position_details = vec![
            asset_in[0],
            asset_in[1],
            asset_in[2],
            asset_in[3],
            asset_out[0],
            asset_out[1],
            asset_out[2],
            asset_out[3],
        ];
        self.add_order(note, Some(position_details))
    }

    pub fn add_order(
        &self,
        note: TrustedNote,
        additional_details: Option<Vec<Felt>>,
    ) -> Result<(String, Uuid, Order)> {
        let hex = note.note().id().to_hex();
        let order = Order::from_trusted_note(note.clone(), additional_details)?;
        let order_id = order.id;
        let instructions: NoteInstructions = note.clone().try_into()?;
        if !instructions.involves_faucets(&self.valid_faucets) {
            return Err(anyhow!("Wrong faucet ids for order."));
        }
        self.note_ids.insert(order_id, hex.clone());
        self.notes.insert(order_id, note);
        self.open_orders.insert(order_id, order.clone());
        Ok((hex, order_id, order.clone()))
    }

    pub fn add_position(&self, note: TrustedNote) -> Result<Uuid> {
        let new_id = Uuid::new_v4();
        self.positions.insert(new_id, note);
        Ok(new_id)
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
        self.open_orders.iter().map(|i| i.value().clone()).collect()
    }

    pub fn get_closed_orders(&self) -> Vec<Order> {
        self.closed_orders
            .iter()
            .map(|i| i.value().clone())
            .collect()
    }

    pub fn flush_open_orders(&self) -> Vec<Order> {
        let orders = self.get_open_orders().clone();
        for order in orders.iter() {
            self.closed_orders.insert(order.id, order.clone());
        }
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
    pub fn pluck_note(&self, id: &Uuid) -> Result<TrustedNote> {
        self.notes
            .remove(id)
            .map(|(_, n)| n)
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
