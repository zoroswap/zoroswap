use crate::{
    amm_state::AmmState,
    websocket::{EventBroadcaster, OrderUpdateEvent},
};
use anyhow::Result;
use chrono::Utc;
use miden_client::account::AccountId;
use rand::seq::SliceRandom;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{info, warn};
use zoro_miden::{note::TrustedNote, pool::ZoroPool};

pub struct TradingEngine {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
    pool_account_id: AccountId,
}

impl TradingEngine {
    pub async fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Result<Self> {
        let config = state.config();
        let pool_account_id = config.pool_account_id;
        Ok(Self {
            state,
            broadcaster,
            pool_account_id,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        let min_match_interval = Duration::from_millis(100); // Debounce
        let max_match_interval = Duration::from_millis(1000); // Max wait (event-driven)
        let config = self.state.config();
        let mut cycle = 0_u128;
        let mut zoro_pool = ZoroPool::new_from_existing_pool(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_dir,
            &self.pool_account_id,
            config.liquidity_pools,
        )
        .await?;
        info!("Init pool states in AmmState");
        self.state.set_pool_states(zoro_pool.pool_states().clone());
        info!(
            "Starting event-driven trading engine (min: {:?}, max: {:?})",
            min_match_interval, max_match_interval
        );

        // Subscribe to events that should trigger matching
        let mut interval = tokio::time::interval(max_match_interval);
        loop {
            interval.tick().await;
            let start = Instant::now();

            if self.prices_are_stale().await {
                continue;
            }
            let mut orders = self.state.flush_open_orders();
            if orders.is_empty() {
                continue;
            }

            // MEV protection: randomize order processing sequence
            orders.shuffle(&mut rand::rng());

            info!(cycle = cycle, "Trading engine cycle.");
            for order in orders.iter() {
                order.print_info(config.network_id.clone());
            }

            // match & execute on the zoro pool
            let notes: Vec<TrustedNote> = orders
                .iter()
                .filter_map(|o| self.state.pluck_note(&o.id).ok())
                .collect();
            let prices = self
                .state
                .oracle_prices()
                .clone()
                .into_iter()
                .collect::<HashMap<_, _>>();
            if !notes.is_empty()
                && let Ok(results) = zoro_pool.execute_notes(notes, prices).await
            {
                for (note_id, result) in &results {
                    let order_id = self.state.get_order_id(note_id).unwrap_or_default();
                    let _ = self.broadcaster.broadcast_order_update(OrderUpdateEvent {
                        order_id,
                        note_id: note_id.to_hex(),
                        status: (*result).into(),
                        timestamp: Utc::now().timestamp_millis() as u64,
                    });
                }
            }
            info!(cycle=cycle, time_elapsed =? start.elapsed(), "Trading engine cycle ends.");
            cycle += 1;
        }
    }

    async fn prices_are_stale(&mut self) -> bool {
        // Check if oracle prices for high-liquidity tokens are fresh.
        const MAX_PRICE_AGE_SECS: u64 = 4;
        const CANARY_TOKENS: &[&str] = &["ETH", "BTC"];
        if let Some(stale_age) = self
            .state
            .oldest_stale_price(MAX_PRICE_AGE_SECS, CANARY_TOKENS)
        {
            warn!(
                "Skipping execution cycle: oracle price for {CANARY_TOKENS:?} is {}s old (max {}s)",
                stale_age, MAX_PRICE_AGE_SECS
            );
            true
        } else {
            false
        }
    }
}
