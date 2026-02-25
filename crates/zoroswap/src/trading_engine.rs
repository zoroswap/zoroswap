use crate::{
    amm_state::AmmState,
    websocket::{EventBroadcaster, OrderUpdateEvent},
};
use anyhow::Result;
use chrono::Utc;
use miden_client::account::AccountId;
use rand::seq::SliceRandom;
use std::{collections::HashMap, sync::Arc, time::Duration};
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
        let mut zoro_pool = ZoroPool::new_from_existing_pool(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_path,
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
            if self.prices_are_stale().await {
                continue;
            }
            let mut orders = self.state.flush_open_orders();
            // MEV protection: randomize order processing sequence
            orders.shuffle(&mut rand::rng());
            // match & execute on the zoro pool
            let notes: Vec<TrustedNote> = orders
                .iter()
                .filter_map(|o| self.state.get_note(&o.id).ok())
                .collect();
            let prices = self
                .state
                .oracle_prices()
                .clone()
                .into_iter()
                .collect::<HashMap<_, _>>();
            if let Ok(results) = zoro_pool.execute_notes(notes, prices).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, websocket::EventBroadcaster};
    use alloy::primitives::U256;
    use chrono::Utc;
    use miden_client::{
        account::{AccountStorageMode, AccountType},
        address::NetworkId,
        asset::FungibleAsset,
        note::{
            NoteAssets, NoteInputs, NoteMetadata, NoteRecipient, NoteScript, NoteTag, NoteType,
        },
    };
    use miden_protocol::{FieldElement, account::AccountIdVersion};
    use zoro_miden::{
        pool::LiquidityPoolConfig,
        pool_state::{PoolBalances, PoolState},
        price::PriceData,
    };

    struct TestContext {
        state: Arc<AmmState>,
        broadcaster: Arc<EventBroadcaster>,
        faucet_a_id: AccountId,
        faucet_b_id: AccountId,
        pool_account_id: AccountId,
        user_account_id: AccountId,
    }

    impl TestContext {
        async fn new() -> Self {
            Self::with_decimals(8, 8).await
        }

        async fn with_decimals(decimals_a: u8, decimals_b: u8) -> Self {
            let faucet_a_id = AccountId::dummy(
                [1; 15],
                AccountIdVersion::Version0,
                AccountType::FungibleFaucet,
                AccountStorageMode::Public,
            );
            let faucet_b_id = AccountId::dummy(
                [2; 15],
                AccountIdVersion::Version0,
                AccountType::FungibleFaucet,
                AccountStorageMode::Public,
            );
            let pool_account_id = AccountId::dummy(
                [3; 15],
                AccountIdVersion::Version0,
                AccountType::RegularAccountUpdatableCode,
                AccountStorageMode::Public,
            );
            let user_account_id = AccountId::dummy(
                [4; 15],
                AccountIdVersion::Version0,
                AccountType::RegularAccountUpdatableCode,
                AccountStorageMode::Public,
            );

            let config = Config {
                pool_account_id,
                liquidity_pools: vec![
                    LiquidityPoolConfig {
                        name: "Wrapped Ether",
                        symbol: "ETH",
                        decimals: decimals_a,
                        faucet_id: faucet_a_id,
                        oracle_id: "ETH",
                    },
                    LiquidityPoolConfig {
                        name: "Wrapped Bitcoin",
                        symbol: "BTC",
                        decimals: decimals_b,
                        faucet_id: faucet_b_id,
                        oracle_id: "BTC",
                    },
                ],
                oracle_sse: "http://localhost:8080",
                oracle_https: "http://localhost:8080",
                miden_endpoint: miden_client::rpc::Endpoint::localhost(),
                server_url: "http://localhost:3000",
                amm_tick_interval: 1000,
                network_id: NetworkId::Testnet,
                masm_path: "./masm",
                keystore_path: "./keystore",
                store_path: "./testing_store.sqlite3",
            };

            let broadcaster = Arc::new(EventBroadcaster::new());
            let state = Arc::new(AmmState::new(config).await);

            let mut pool_a = PoolState::default();
            let mut pool_b = PoolState::default();
            pool_a.update_state(
                PoolBalances {
                    reserve: U256::from(1_000_000_00000000u64),
                    reserve_with_slippage: U256::from(1_000_000_00000000u64),
                    total_liabilities: U256::from(1_000_000_00000000u64),
                },
                1_000_000_00000000u64,
            );
            pool_b.update_state(
                PoolBalances {
                    reserve: U256::from(1_000_000_00000000u64),
                    reserve_with_slippage: U256::from(1_000_000_00000000u64),
                    total_liabilities: U256::from(1_000_000_00000000u64),
                },
                1_000_000_00000000u64,
            );
            state.liquidity_pools().insert(faucet_a_id, pool_a);
            state.liquidity_pools().insert(faucet_b_id, pool_b);

            Self {
                state,
                broadcaster,
                faucet_a_id,
                faucet_b_id,
                pool_account_id,
                user_account_id,
            }
        }

        fn create_swap_note(&self, amount_in: u64, min_amount_out: u64) -> Note {
            let asset_in = FungibleAsset::new(self.faucet_a_id, amount_in).unwrap();
            let deadline = Utc::now() + chrono::Duration::minutes(5);

            let mut inputs: Vec<Felt> = vec![Felt::ZERO; 12];
            inputs[0] = Felt::new(min_amount_out);
            let faucet_b_felts: [Felt; 2] = self.faucet_b_id.into();
            inputs[2] = faucet_b_felts[1];
            inputs[3] = faucet_b_felts[0];
            inputs[4] = Felt::new(deadline.timestamp_millis() as u64);
            let user_felts: [Felt; 2] = self.user_account_id.into();
            inputs[10] = user_felts[1];
            inputs[11] = user_felts[0];

            let note_inputs = NoteInputs::new(inputs).unwrap();
            let note_tag = NoteTag::with_account_target(self.pool_account_id);
            let metadata = NoteMetadata::new(self.user_account_id, NoteType::Public, note_tag);
            let assets = NoteAssets::new(vec![asset_in.into()]).unwrap();
            let serial_num: Word = [Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)].into();
            let recipient = NoteRecipient::new(serial_num, NoteScript::mock(), note_inputs);
            Note::new(assets, metadata, recipient)
        }

        fn set_oracle_prices(&self, timestamp: u64, price: u64) {
            self.state
                .oracle_prices()
                .insert(self.faucet_a_id, PriceData::new(timestamp, price));
            self.state
                .oracle_prices()
                .insert(self.faucet_b_id, PriceData::new(timestamp, price));
        }

        async fn create_engine(&self) -> Result<TradingEngine> {
            TradingEngine::new(self.state.clone(), self.broadcaster.clone()).await
        }
    }

    #[tokio::test]
    async fn test_matching_skipped_with_stale_prices() -> Result<()> {
        let ctx = TestContext::new().await;

        // Add a swap order
        let note = ctx.create_swap_note(1000, 1);
        ctx.state.add_order(note).expect("Should add order");
        assert_eq!(ctx.state.get_open_orders().len(), 1);

        // Set STALE oracle prices (10 seconds old)
        let stale_time = Utc::now().timestamp() as u64 - 10;
        ctx.set_oracle_prices(stale_time, 100_000_000);

        // Verify stale prices are detected
        const MAX_PRICE_AGE_SECS: u64 = 4;
        const CANARY_TOKENS: &[&str] = &["ETH", "BTC"];
        assert!(
            ctx.state
                .oldest_stale_price(MAX_PRICE_AGE_SECS, CANARY_TOKENS)
                .is_some(),
            "Prices should be detected as stale"
        );
        assert_eq!(
            ctx.state.get_open_orders().len(),
            1,
            "Orders should remain when prices are stale"
        );

        // Set FRESH oracle prices
        let fresh_time = Utc::now().timestamp() as u64;
        ctx.set_oracle_prices(fresh_time, 100_000_000);

        assert!(
            ctx.state
                .oldest_stale_price(MAX_PRICE_AGE_SECS, CANARY_TOKENS)
                .is_none(),
            "Prices should be fresh"
        );

        // Run matching cycle - should process the order
        let engine = ctx.create_engine().await?;
        let executions = engine
            .run_matching_cycle()
            .expect("Matching should succeed");

        assert_eq!(
            executions.executions.len(),
            1,
            "Should have processed 1 order"
        );
        assert_eq!(
            ctx.state.get_open_orders().len(),
            0,
            "Open orders should be empty after matching"
        );
        Ok(())
    }

    /// Test that `get_deposit_lp_amount_out` returns a `PoolState` with correctly updated
    /// `lp_total_supply`, enabling consecutive deposits to use accurate LP supply values.
    #[test]
    fn test_get_deposit_lp_amount_out_returns_updated_lp_total_supply() -> Result<()> {
        // Given: a pool with initial LP supply and reserves
        let initial_lp_supply: u64 = 1_000_000;
        let initial_reserve = U256::from(10_000_000u64);
        let deposit_amount = U256::from(1_000_000u64);

        let mut pool = PoolState::default();
        pool.update_state(
            PoolBalances {
                reserve: initial_reserve,
                reserve_with_slippage: initial_reserve,
                total_liabilities: initial_reserve,
            },
            initial_lp_supply,
        );

        // When: calculating LP amount out for a deposit
        let (lp_out, new_lp_supply, _) = pool.get_deposit_lp_amount_out(deposit_amount)?;

        // Then: the returned pool state has lp_total_supply = initial + minted
        assert!(lp_out > U256::ZERO, "must mint LP tokens");
        assert_eq!(
            new_lp_supply,
            U256::from(initial_lp_supply)
                .saturating_add(lp_out)
                .saturating_to::<u64>(),
            "returned pool state must have lp_total_supply = initial + minted"
        );
        Ok(())
    }

    /// Test that consecutive deposits correctly accumulate `lp_total_supply`.
    #[test]
    fn test_consecutive_deposits_accumulate_lp_total_supply() -> Result<()> {
        // Given: a pool with initial LP supply
        let initial_lp_supply: u64 = 1_000_000;
        let initial_reserve = U256::from(10_000_000u64);
        let deposit_amount = U256::from(1_000_000u64);

        let mut pool = PoolState::default();
        pool.update_state(
            PoolBalances {
                reserve: initial_reserve,
                reserve_with_slippage: initial_reserve,
                total_liabilities: initial_reserve,
            },
            initial_lp_supply,
        );

        // When: two consecutive deposits are processed
        let (lp_out1, lp_after_first, _) = pool.get_deposit_lp_amount_out(deposit_amount)?;
        let (lp_out2, lp_after_second, _) = pool.get_deposit_lp_amount_out(deposit_amount)?;

        // Then: the final `lp_total_supply` reflects both deposits
        let expected_final_supply = U256::from(initial_lp_supply)
            .saturating_add(lp_out1)
            .saturating_add(lp_out2)
            .saturating_to::<u64>();

        assert_eq!(
            lp_after_second, expected_final_supply,
            "final lp_total_supply must equal initial + first deposit + second deposit"
        );
        assert!(
            lp_after_second > lp_after_first,
            "second deposit must increase lp_total_supply beyond first deposit"
        );
        Ok(())
    }
}
