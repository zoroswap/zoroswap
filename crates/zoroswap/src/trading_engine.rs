use crate::{
    amm_state::AmmState,
    common::{instantiate_client, print_transaction_info},
    order::Order,
    pool::{PoolState, get_curve_amount_out},
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateDetails, OrderUpdateEvent},
};
use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use chrono::Utc;
use dashmap::DashMap;
use miden_client::{
    Felt, Word,
    account::AccountId,
    asset::{Asset, FungibleAsset},
    note::{Note, NoteType},
    transaction::TransactionRequestBuilder,
};
use miden_objects::note::NoteDetails;
use rand::seq::SliceRandom;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::error::RecvError;
use tracing::{error, info, warn};
use zoro_miden_client::{MidenClient, create_p2id_note};

#[derive(Debug, Clone)]
struct ExecutionDetails {
    note: Note,
    order: Order,
    amount_out: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum OrderExecution {
    Swap(ExecutionDetails),
    FailedSwap(ExecutionDetails),
    PastDeadline(ExecutionDetails),
}

pub struct TradingEngine {
    state: Arc<AmmState>,
    store_path: String,
    broadcaster: Arc<EventBroadcaster>,
    last_match_time: Arc<Mutex<Instant>>,
}

impl TradingEngine {
    pub fn new(store_path: &str, state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self {
            store_path: store_path.to_string(),
            state,
            broadcaster,
            last_match_time: Arc::new(Mutex::new(Instant::now())),
        }
    }

    pub async fn start(&mut self) {
        let min_match_interval = Duration::from_millis(100); // Debounce
        let max_match_interval = Duration::from_millis(1000); // Max wait (event-driven)

        // Create client with retry for DB contention
        let mut client = None;
        for attempt in 1..=5 {
            match instantiate_client(self.state.config(), &self.store_path).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(e) => {
                    if attempt < 5 {
                        warn!(
                            "Trading engine client creation attempt {}/5 failed: {e}, retrying...",
                            attempt
                        );
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    } else {
                        panic!(
                            "Failed to instantiate client in trading engine after 5 attempts: {e}"
                        );
                    }
                }
            }
        }
        let mut client = client.unwrap();

        info!(
            "Starting event-driven trading engine (min: {:?}, max: {:?})",
            min_match_interval, max_match_interval
        );

        // Subscribe to events that should trigger matching
        let mut order_rx = self.broadcaster.subscribe_order_updates();
        let mut price_rx = self.broadcaster.subscribe_oracle_prices();

        let mut interval = tokio::time::interval(max_match_interval);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    // Periodic matching (every 1s max)
                    self.run_matching_if_ready(min_match_interval, &mut client).await;
                }
                result = order_rx.recv() => {
                    match result {
                        Ok(order_event) => {
                            if order_event.status == OrderStatus::Pending {
                                info!("New order received, triggering match cycle");
                                self.run_matching_if_ready(min_match_interval, &mut client).await;
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            warn!("Trading engine lagged, skipped {} order updates", skipped);
                        }
                        Err(_) => break,
                    }
                }
                result = price_rx.recv() => {
                    match result {
                        Ok(_price_event) => {
                            // Price update: trigger match
                            self.run_matching_if_ready(min_match_interval, &mut client).await;
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            warn!("Trading engine lagged, skipped {} price updates", skipped);
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }

    async fn run_matching_if_ready(&mut self, min_interval: Duration, client: &mut MidenClient) {
        // Debouncing: prevent excessive matching
        let mut last_match = self.last_match_time.lock().unwrap();
        if last_match.elapsed() < min_interval {
            return; // Skip this match cycle
        }
        *last_match = Instant::now();
        drop(last_match);

        // Check if oracle prices are fresh (max 2 seconds old)
        const MAX_PRICE_AGE_SECS: u64 = 2;
        if let Some(stale_age) = self.state.oldest_stale_price(MAX_PRICE_AGE_SECS) {
            warn!(
                "Skipping matching cycle: oracle price is {}s old (max {}s)",
                stale_age, MAX_PRICE_AGE_SECS
            );
            return;
        }

        // Run existing matching logic
        match self.run_matching_cycle() {
            Ok(orders_to_execute) => {
                if !orders_to_execute.is_empty() {
                    // Broadcast order status updates before execution
                    for execution in &orders_to_execute {
                        let (order_id, status) = match execution {
                            OrderExecution::Swap(details) => {
                                (details.order.id, OrderStatus::Matching)
                            }
                            OrderExecution::FailedSwap(details) => {
                                (details.order.id, OrderStatus::Failed)
                            }
                            OrderExecution::PastDeadline(details) => {
                                (details.order.id, OrderStatus::Expired)
                            }
                        };

                        if status != OrderStatus::Matching {
                            // Broadcast final status for non-swap orders
                            let details = match execution {
                                OrderExecution::Swap(d)
                                | OrderExecution::FailedSwap(d)
                                | OrderExecution::PastDeadline(d) => d,
                            };
                            let note_id = self.state.get_note_id(&order_id).unwrap_or_default();
                            let _ = self.broadcaster.broadcast_order_update(OrderUpdateEvent {
                                order_id,
                                note_id,
                                status,
                                details: OrderUpdateDetails {
                                    amount_in: details.order.asset_in.amount(),
                                    amount_out: Some(details.amount_out),
                                    asset_in_faucet: details.order.asset_in.faucet_id().to_hex(),
                                    asset_out_faucet: details.order.asset_out.faucet_id().to_hex(),
                                },
                                timestamp: Utc::now().timestamp_millis() as u64,
                            });
                        }
                    }

                    // Execute swaps and broadcast success
                    match self.run_executions(orders_to_execute.clone(), client).await {
                        Ok(_) => {
                            // Broadcast Executed status for successful swaps
                            for execution in &orders_to_execute {
                                if let OrderExecution::Swap(details) = execution {
                                    let note_id = self
                                        .state
                                        .get_note_id(&details.order.id)
                                        .unwrap_or_default();
                                    let _ =
                                        self.broadcaster.broadcast_order_update(OrderUpdateEvent {
                                            order_id: details.order.id,
                                            note_id,
                                            status: OrderStatus::Executed,
                                            details: OrderUpdateDetails {
                                                amount_in: details.order.asset_in.amount(),
                                                amount_out: Some(details.amount_out),
                                                asset_in_faucet: details
                                                    .order
                                                    .asset_in
                                                    .faucet_id()
                                                    .to_hex(),
                                                asset_out_faucet: details
                                                    .order
                                                    .asset_out
                                                    .faucet_id()
                                                    .to_hex(),
                                            },
                                            timestamp: Utc::now().timestamp_millis() as u64,
                                        });
                                }
                            }
                        }
                        Err(e) => {
                            error!("{e}");
                        }
                    }
                }
            }
            Err(e) => {
                error!("{e}")
            }
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn run_matching_cycle(&self) -> Result<Vec<OrderExecution>> {
        let pools = self.state.liquidity_pools().clone();
        let mut orders = self.state.flush_open_orders();

        // MEV protection: randomize order processing sequence
        orders.shuffle(&mut rand::rng());

        let mut order_executions = Vec::new();
        let now = Utc::now();
        for order in orders {
            // TODO: check for
            //       ERR_MAX_COVERAGE_RATIO_EXCEEDED +
            //       ERR_RESERVE_WITH_SLIPPAGE_EXCEEDS_ASSET_BALANCE

            // Check if order is past deadline
            if order.deadline < now {
                let (_, note) = self.state.pluck_note(&order.id)?;
                warn!("Swap past deadline (by {})", now - order.deadline);
                order_executions.push(OrderExecution::PastDeadline(ExecutionDetails {
                    note,
                    order,
                    amount_out: order.asset_in.amount(),
                }));
                continue;
            }
            let ((base_pool_state, base_pool_decimals), (quote_pool_state, quote_pool_decimals)) =
                self.get_liq_pools_for_order(&pools, &order)?;
            let price = {
                self.state.oracle_price_for_pair(
                    order.asset_in.faucet_id(),
                    order.asset_out.faucet_id(),
                )?
            };
            info!(
                "SWAP: decimals base: {}, quote: {}, amount_in: {:?}, price: {:?}",
                base_pool_decimals,
                quote_pool_decimals,
                order.asset_in.amount(),
                price
            );
            let (amount_out, new_base_pool_balance, new_quote_pool_balance) = get_curve_amount_out(
                &base_pool_state,
                &quote_pool_state,
                U256::from(base_pool_decimals),
                U256::from(quote_pool_decimals),
                U256::from(order.asset_in.amount()),
                price,
            )?;
            let amount_out = amount_out.to::<u64>();
            if amount_out > 0 && amount_out >= order.asset_out.amount() {
                // Swap successful - create execution order for swap
                info!("Swap successful!");
                pools
                    .get_mut(&order.asset_in.faucet_id())
                    .ok_or(anyhow!("Missing pool in state"))?
                    .update_state(new_base_pool_balance);
                pools
                    .get_mut(&order.asset_out.faucet_id())
                    .ok_or(anyhow!("Missing pool in state"))?
                    .update_state(new_quote_pool_balance);
                let (_, note) = self.state.pluck_note(&order.id)?;
                order_executions.push(OrderExecution::Swap(ExecutionDetails {
                    note,
                    order,
                    amount_out,
                }));
            } else {
                warn!("Swap unsuccessful.");
                if amount_out == 0 {
                    info!("Amount out calculated to be 0.")
                } else if amount_out < order.asset_out.amount() {
                    info!(
                        "User would get {} but it wanted at least {}.",
                        amount_out,
                        order.asset_out.amount()
                    );
                }
                let (_, note) = self.state.pluck_note(&order.id)?;
                order_executions.push(OrderExecution::FailedSwap(ExecutionDetails {
                    note,
                    order,
                    amount_out: order.asset_in.amount(),
                }));
            }
        }
        Ok(order_executions)
    }

    async fn run_executions(
        &mut self,
        executions: Vec<OrderExecution>,
        client: &mut MidenClient,
    ) -> Result<()> {
        client.sync_state().await?;
        let pool_account_id = self.state.config().pool_account_id;
        let network_id = self.state.config().miden_endpoint.to_network_id();
        let mut input_notes = Vec::new();
        let mut expected_future_notes = Vec::new();
        let mut expected_output_recipients = Vec::new();
        for execution in executions {
            let (asset_out, user_account_id, serial_num, note) = match execution {
                OrderExecution::Swap(execution_details) => (
                    FungibleAsset::new(
                        execution_details.order.asset_out.faucet_id(),
                        execution_details.amount_out,
                    )?,
                    execution_details.order.creator_id,
                    execution_details.note.serial_num(),
                    execution_details.note,
                ),
                OrderExecution::FailedSwap(execution_details) => (
                    execution_details.order.asset_in,
                    execution_details.order.creator_id,
                    execution_details.note.serial_num(),
                    execution_details.note,
                ),
                OrderExecution::PastDeadline(execution_details) => (
                    execution_details.order.asset_in,
                    execution_details.order.creator_id,
                    execution_details.note.serial_num(),
                    execution_details.note,
                ),
            };
            let asset_out = Asset::Fungible(asset_out);
            let p2id_serial_num = [
                serial_num[0],
                serial_num[1],
                serial_num[2],
                Felt::new(serial_num[3].as_int() + 1),
            ];
            info!(
                "Calculated {asset_out:?} asset_out for recipient {} with serial number {:?}",
                user_account_id.to_bech32(network_id.clone()),
                p2id_serial_num
            );

            let p2id = create_p2id_note(
                pool_account_id,
                user_account_id,
                vec![asset_out],
                NoteType::Public,
                Felt::new(0),
                p2id_serial_num.into(),
            )?;
            input_notes.push((
                note.clone(),
                Some(Word::new([
                    Felt::new(asset_out.unwrap_fungible().amount()),
                    Felt::new(0),
                    Felt::new(0),
                    Felt::new(0),
                ])),
            ));
            expected_future_notes.push((NoteDetails::from(p2id.clone()), p2id.metadata().tag()));
            expected_output_recipients.push(p2id.recipient().clone());
        }

        for note in &expected_future_notes {
            info!("Expected future note P2ID id: {:?}", note.0.id());
            for asset in note.0.assets().iter_fungible() {
                info!(
                    "Expected future note P2ID asset with faucet_id: {}: {:?}",
                    asset.faucet_id().to_bech32(network_id.clone()),
                    asset.amount()
                );
            }
        }

        let consume_req = TransactionRequestBuilder::new()
            .unauthenticated_input_notes(input_notes.clone())
            .expected_future_notes(expected_future_notes)
            .expected_output_recipients(expected_output_recipients.clone())
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build batch transaction request: {}", e))?;

        info!("All Recipients: {:?}", expected_output_recipients);
        for recipient in expected_output_recipients.clone() {
            info!("Recipient digest: {:?}", recipient.digest());
        }

        let tx_id = client
            .submit_new_transaction(pool_account_id, consume_req)
            .await
            .map_err(|e| {
                error!("🔍 Detailed batch transaction creation error: {:?}", e);
                error!("🔍 Pool ID: {}", pool_account_id.to_hex());
                error!("🔍 Number of input notes: {}", input_notes.len());
                error!(
                    "🔍 Number of expected output recipients: {}",
                    expected_output_recipients.len()
                );
                anyhow::anyhow!("Failed to create and submit batch transaction: {:?}", e)
            })?;
        // Submitted the transaction (exactly like the test)
        info!("Submitted TX: {:?}", tx_id);

        client.sync_state().await?;

        print_transaction_info(&tx_id);

        Ok(())
    }

    fn get_liq_pools_for_order(
        &self,
        liq_pool_states: &DashMap<AccountId, PoolState>,
        order: &Order,
    ) -> Result<((PoolState, u8), (PoolState, u8))> {
        let asset_in_id = order.asset_in.faucet_id();
        let asset_out_id = order.asset_out.faucet_id();
        let base_pool_state = liq_pool_states.get(&asset_in_id).ok_or(anyhow!(
            "Liquidity pool for faucet ID {} doesnt exist in state.",
            asset_in_id
        ))?;
        let quote_pool_state = liq_pool_states.get(&asset_out_id).ok_or(anyhow!(
            "Liquidity pool for faucet ID {} doesnt exist in state.",
            asset_out_id
        ))?;
        let base_pool_faucet = self
            .state
            .faucet_metadata()
            .get(&asset_in_id)
            .ok_or(anyhow!(
                "Faucet metadata for faucet ID {} doesnt exist in state.",
                asset_in_id
            ))?;
        let quote_pool_faucet = self
            .state
            .faucet_metadata()
            .get(&asset_out_id)
            .ok_or(anyhow!(
                "Faucet metadata for faucet ID {} doesnt exist in state.",
                asset_out_id
            ))?;
        Ok((
            (*base_pool_state, base_pool_faucet.decimals()),
            (*quote_pool_state, quote_pool_faucet.decimals()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, LiquidityPoolConfig},
        oracle_sse::PriceData,
        pool::{PoolBalances, PoolState},
        websocket::EventBroadcaster,
    };
    use chrono::Utc;
    use miden_client::{
        account::{AccountStorageMode, AccountType},
        address::NetworkId,
        asset::{FungibleAsset, TokenSymbol},
        note::{
            NoteAssets, NoteExecutionHint, NoteInputs, NoteMetadata, NoteRecipient, NoteScript,
            NoteTag, NoteType,
        },
    };
    use miden_lib::account::faucets::BasicFungibleFaucet;
    use miden_objects::{account::AccountIdVersion, FieldElement};
    use uuid::Uuid;

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

            let symbol_a = TokenSymbol::new("TKA").unwrap();
            let symbol_b = TokenSymbol::new("TKB").unwrap();
            let faucet_a =
                BasicFungibleFaucet::new(symbol_a, decimals_a, Felt::new(1_000_000_000)).unwrap();
            let faucet_b =
                BasicFungibleFaucet::new(symbol_b, decimals_b, Felt::new(1_000_000_000)).unwrap();

            let config = Config {
                pool_account_id,
                liquidity_pools: vec![
                    LiquidityPoolConfig {
                        name: "TokenA",
                        symbol: "TKA",
                        decimals: decimals_a,
                        faucet_id: faucet_a_id,
                        oracle_id: "oracle_a",
                    },
                    LiquidityPoolConfig {
                        name: "TokenB",
                        symbol: "TKB",
                        decimals: decimals_b,
                        faucet_id: faucet_b_id,
                        oracle_id: "oracle_b",
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
            let state = Arc::new(AmmState::new(config, broadcaster.clone()).await);

            state.faucet_metadata().insert(faucet_a_id, faucet_a);
            state.faucet_metadata().insert(faucet_b_id, faucet_b);

            let mut pool_a = PoolState::new(pool_account_id, faucet_a_id);
            let mut pool_b = PoolState::new(pool_account_id, faucet_b_id);
            pool_a.update_state(PoolBalances {
                reserve: U256::from(1_000_000_00000000u64),
                reserve_with_slippage: U256::from(1_000_000_00000000u64),
                total_liabilities: U256::from(1_000_000_00000000u64),
            });
            pool_b.update_state(PoolBalances {
                reserve: U256::from(1_000_000_00000000u64),
                reserve_with_slippage: U256::from(1_000_000_00000000u64),
                total_liabilities: U256::from(1_000_000_00000000u64),
            });
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
            let note_tag = NoteTag::from_account_id(self.pool_account_id);
            let metadata = NoteMetadata::new(
                self.user_account_id,
                NoteType::Public,
                note_tag,
                NoteExecutionHint::always(),
                Felt::ZERO,
            )
            .unwrap();
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

        fn create_engine(&self) -> TradingEngine {
            TradingEngine::new("testing_store.sqlite3", self.state.clone(), self.broadcaster.clone())
        }
    }

    #[tokio::test]
    async fn test_get_liq_pools_for_order_uses_correct_asset_ids() {
        let ctx = TestContext::with_decimals(6, 12).await;

        let asset_in = FungibleAsset::new(ctx.faucet_a_id, 100).unwrap();
        let asset_out = FungibleAsset::new(ctx.faucet_b_id, 200).unwrap();
        let order = Order {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline: Utc::now(),
            is_limit_order: false,
            asset_in,
            asset_out,
            p2id_tag: 0,
            creator_id: ctx.pool_account_id,
        };

        let engine = ctx.create_engine();
        let result = engine.get_liq_pools_for_order(&ctx.state.liquidity_pools(), &order);

        assert!(result.is_ok(), "Unable to get liquidity pools");
        let ((_base_pool, base_decimals), (_quote_pool, quote_decimals)) = result.unwrap();

        assert_eq!(base_decimals, 6, "Base pool (asset_in) must have 6 decimals");
        assert_eq!(quote_decimals, 12, "Quote pool (asset_out) must have 12 decimals");
    }

    #[tokio::test]
    async fn test_matching_skipped_with_stale_prices() {
        let ctx = TestContext::new().await;

        // Add a swap order
        let note = ctx.create_swap_note(1000, 1);
        ctx.state.add_order(note).expect("Should add order");
        assert_eq!(ctx.state.get_open_orders().len(), 1);

        // Set STALE oracle prices (10 seconds old)
        let stale_time = Utc::now().timestamp() as u64 - 10;
        ctx.set_oracle_prices(stale_time, 100_000_000);

        // Verify stale prices are detected
        const MAX_PRICE_AGE_SECS: u64 = 2;
        assert!(
            ctx.state.oldest_stale_price(MAX_PRICE_AGE_SECS).is_some(),
            "Prices should be detected as stale"
        );
        assert_eq!(ctx.state.get_open_orders().len(), 1, "Orders should remain when prices are stale");

        // Set FRESH oracle prices
        let fresh_time = Utc::now().timestamp() as u64;
        ctx.set_oracle_prices(fresh_time, 100_000_000);

        assert!(
            ctx.state.oldest_stale_price(MAX_PRICE_AGE_SECS).is_none(),
            "Prices should be fresh"
        );

        // Run matching cycle - should process the order
        let engine = ctx.create_engine();
        let executions = engine.run_matching_cycle().expect("Matching should succeed");

        assert_eq!(executions.len(), 1, "Should have processed 1 order");
        assert_eq!(ctx.state.get_open_orders().len(), 0, "Open orders should be empty after matching");
    }
}
