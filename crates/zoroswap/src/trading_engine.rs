use crate::{
    amm_state::AmmState,
    common::{instantiate_client, print_transaction_info},
    order::{Order, OrderType},
    pool::{
        PoolBalances, PoolState, get_curve_amount_out, get_deposit_lp_amount_out,
        get_withdraw_asset_amount_out,
    },
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateDetails, OrderUpdateEvent},
};
use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use dashmap::DashMap;
use miden_client::{
    Felt, Serializable, Word,
    account::AccountId,
    address::NetworkId,
    asset::{Asset, FungibleAsset},
    note::{Note, NoteRecipient, NoteTag, NoteType},
    transaction::TransactionRequestBuilder,
};
use miden_objects::{note::NoteDetails, vm::AdviceMap};
use rand::seq::SliceRandom;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error, info, warn};
use zoro_miden_client::{MidenClient, create_p2id_note};

#[derive(Debug, Clone)]
pub(crate) struct ExecutionDetails {
    note: Note,
    order: Order,
    amount_out: u64,
    in_pool_balances: PoolBalances,
    out_pool_balances: PoolBalances,
}

#[derive(Debug, Clone)]
pub(crate) enum OrderExecution {
    Swap(ExecutionDetails),
    Deposit(ExecutionDetails),
    Withdraw(ExecutionDetails),
    FailedOrder(ExecutionDetails),
    PastDeadline(ExecutionDetails),
}

pub(crate) struct MatchingCycle {
    executions: Vec<OrderExecution>,
    new_pool_states: DashMap<AccountId, PoolState>,
}
pub struct TradingEngine {
    state: Arc<AmmState>,
    store_path: String,
    broadcaster: Arc<EventBroadcaster>,
    last_match_time: Arc<Mutex<Instant>>,
    network_id: NetworkId,
    pool_account_id: AccountId,
}

enum NoteExecutionDetails {
    Payout(PayoutDetails),
    ConsumeWithArgs((Note, Word)),
}

struct PayoutDetails {
    pub note: Note,
    pub args: Option<Word>,
    pub advice_map_value: Vec<Felt>,
    pub details: NoteDetails,
    pub tag: NoteTag,
    pub recipient: NoteRecipient,
}

impl TradingEngine {
    pub fn new(store_path: &str, state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        let config = state.config();
        let pool_account_id = config.pool_account_id;
        let network_id = config.miden_endpoint.to_network_id();
        Self {
            store_path: store_path.to_string(),
            state,
            broadcaster,
            last_match_time: Arc::new(Mutex::new(Instant::now())),
            network_id,
            pool_account_id,
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
        let mut last_match = { self.last_match_time.lock().unwrap() };
        if last_match.elapsed() < min_interval {
            return; // Skip this match cycle
        }
        *last_match = Instant::now();
        drop(last_match);

        // Check if oracle prices for high-liquidity tokens are fresh.
        // We only check ETH and BTC here: these tokens have high liquidity
        // and are traded often, so we should always have fresh prices.
        //
        // We'll add more tokens in the future, those might have lower liquidity
        // and slower price updates, so we can't enforce the same short staleness
        // threshold for them.
        const MAX_PRICE_AGE_SECS: u64 = 4;
        const CANARY_TOKENS: &[&str] = &["ETH", "BTC"];
        if let Some(stale_age) = self
            .state
            .oldest_stale_price(MAX_PRICE_AGE_SECS, CANARY_TOKENS)
        {
            warn!(
                "Skipping matching cycle: oracle price for {CANARY_TOKENS:?} is {}s old (max {}s)",
                stale_age, MAX_PRICE_AGE_SECS
            );
            return;
        }

        // Run existing matching logic
        match self.run_matching_cycle() {
            Ok(matching_cycle) => {
                let executions = matching_cycle.executions;
                if !executions.is_empty() {
                    // Broadcast order status updates before execution
                    for execution in &executions {
                        let (order_id, status) = match execution {
                            OrderExecution::Swap(details) => {
                                (details.order.id, OrderStatus::Matching)
                            }
                            OrderExecution::Deposit(details) => {
                                (details.order.id, OrderStatus::Matching)
                            }
                            OrderExecution::Withdraw(details) => {
                                (details.order.id, OrderStatus::Matching)
                            }
                            OrderExecution::FailedOrder(details) => {
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
                                | OrderExecution::FailedOrder(d)
                                | OrderExecution::Deposit(d)
                                | OrderExecution::Withdraw(d)
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
                    match self.execute_orders(executions.clone(), client).await {
                        Ok(_) => {
                            for (faucet_id, pool_state) in matching_cycle.new_pool_states {
                                self.state.update_pool_state(&faucet_id, pool_state);
                            }

                            // Broadcast order status for executed orders
                            for execution in &executions {
                                match &execution {
                                    &OrderExecution::Swap(details)
                                    | &OrderExecution::Deposit(details)
                                    | &OrderExecution::Withdraw(details) => {
                                        let note_id = self
                                            .state
                                            .get_note_id(&details.order.id)
                                            .unwrap_or_default();
                                        let _ = self.broadcaster.broadcast_order_update(
                                            OrderUpdateEvent {
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
                                            },
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => {
                            error!("Failed to execute orders: {e:?}");
                        }
                    }
                }
            }
            Err(e) => {
                error!("Failed to run matching cycle: {e:?}")
            }
        }
    }

    pub(crate) fn run_matching_cycle(&self) -> Result<MatchingCycle> {
        let pools = self.state.liquidity_pools().clone();
        let mut orders = self.state.flush_open_orders();

        // MEV protection: randomize order processing sequence
        orders.shuffle(&mut rand::rng());

        let mut order_executions = Vec::new();
        let now = Utc::now();
        for order in orders {
            let ((base_pool_state, base_pool_decimals), (quote_pool_state, quote_pool_decimals)) =
                self.get_liq_pools_for_order(&pools, &order)?;
            debug!(
                order = ?order,
                base_pool_state = ?base_pool_state,
                quote_pool_state = ?quote_pool_state,
                "Processing order"
            );

            if order.deadline < now {
                let (_, note) = self.state.pluck_note(&order.id)?;
                warn!(
                    "Order {:?} is past deadline (by {})",
                    order.order_type,
                    now - order.deadline
                );
                order_executions.push(OrderExecution::PastDeadline(ExecutionDetails {
                    note,
                    order,
                    amount_out: order.asset_in.amount(),
                    in_pool_balances: base_pool_state.balances,
                    out_pool_balances: quote_pool_state.balances,
                }));
                continue;
            }
            let price = {
                self.state.oracle_price_for_pair(
                    order.asset_in.faucet_id(),
                    order.asset_out.faucet_id(),
                )?
            };

            match order.order_type {
                OrderType::Deposit => {
                    let (amount_out, new_pool_state) = get_deposit_lp_amount_out(
                        &base_pool_state,
                        U256::from(order.asset_in.amount()),
                        U256::from(base_pool_state.lp_total_supply),
                        U256::from(base_pool_decimals),
                    );
                    let amount_out = amount_out.to::<u64>();
                    if amount_out > 0 && amount_out >= order.asset_out.amount() {
                        let (_, note) = self.state.pluck_note(&order.id)?;
                        pools
                            .get_mut(&order.asset_in.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_state(new_pool_state.balances, new_pool_state.lp_total_supply);
                        order_executions.push(OrderExecution::Deposit(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: new_pool_state.balances,
                            out_pool_balances: quote_pool_state.balances,
                        }));
                    } else {
                        warn!("Deposit unsuccessful.");
                        if amount_out == 0 {
                            info!("LP amount out calculated to be 0.")
                        } else if amount_out < order.asset_out.amount() {
                            info!(
                                "User would get {} lp but it wanted at least {}.",
                                amount_out,
                                order.asset_out.amount()
                            );
                        }
                        let (_, note) = self.state.pluck_note(&order.id)?;
                        order_executions.push(OrderExecution::FailedOrder(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: base_pool_state.balances,
                            out_pool_balances: quote_pool_state.balances,
                        }));
                    }
                }
                OrderType::Withdraw => {
                    let (amount_out, new_pool_state) = get_withdraw_asset_amount_out(
                        &base_pool_state,
                        U256::from(order.asset_in.amount()),
                        U256::from(base_pool_state.lp_total_supply),
                        U256::from(base_pool_decimals),
                    );
                    let amount_out = amount_out.to::<u64>();
                    if amount_out > 0 && amount_out >= order.asset_out.amount() {
                        let (_, note) = self.state.pluck_note(&order.id)?;
                        pools
                            .get_mut(&order.asset_out.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_state(new_pool_state.balances, new_pool_state.lp_total_supply);
                        order_executions.push(OrderExecution::Withdraw(ExecutionDetails {
                            note,
                            order,
                            amount_out,
                            in_pool_balances: new_pool_state.balances,
                            out_pool_balances: quote_pool_state.balances,
                        }));
                    } else {
                        warn!("Withdraw unsuccessful.");
                        if amount_out == 0 {
                            info!("LP amount out calculated to be 0.")
                        } else if amount_out < order.asset_out.amount() {
                            info!(
                                "User would get {} but it wanted at least {}.",
                                amount_out,
                                order.asset_out.amount()
                            );
                        }
                        self.state.pluck_note(&order.id)?;
                    }
                }
                OrderType::Swap => {
                    // TODO: check for
                    //       ERR_MAX_COVERAGE_RATIO_EXCEEDED +
                    //       ERR_RESERVE_WITH_SLIPPAGE_EXCEEDS_ASSET_BALANCE
                    info!(
                        amount_in = order.asset_in.amount(),
                        faucet_in = %order.asset_in.faucet_id().to_bech32(self.network_id.clone()),
                        min_amount_out = order.asset_out.amount(),
                        faucet_out = %order.asset_out.faucet_id().to_bech32(self.network_id.clone()),
                        price = %price,
                        "Processing swap order"
                    );
                    let amount_in = order.asset_in.amount();
                    let curve_result = get_curve_amount_out(
                        &base_pool_state,
                        &quote_pool_state,
                        U256::from(base_pool_decimals),
                        U256::from(quote_pool_decimals),
                        U256::from(order.asset_in.amount()),
                        price,
                    );
                    let (amount_out, new_base_pool_balance, new_quote_pool_balance) =
                        match curve_result {
                            Ok(result) => result,
                            Err(e) => {
                                warn!("Swap calculation failed: {e}");
                                let pool_in = pools
                                    .get(&order.asset_in.faucet_id())
                                    .ok_or(anyhow!("Missing pool in state"))?;

                                let pool_out = pools
                                    .get_mut(&order.asset_out.faucet_id())
                                    .ok_or(anyhow!("Missing pool in state"))?;
                                let (_, note) = self.state.pluck_note(&order.id)?;
                                order_executions.push(OrderExecution::FailedOrder(
                                    ExecutionDetails {
                                        in_pool_balances: pool_in.balances,
                                        out_pool_balances: pool_out.balances,
                                        note,
                                        order,
                                        amount_out: order.asset_in.amount(),
                                    },
                                ));
                                continue;
                            }
                        };
                    let amount_out = amount_out.to::<u64>();
                    if amount_out > 0 && amount_out >= order.asset_out.amount() {
                        // Swap successful - create execution order for swap
                        info!(
                            creator = %order.creator_id.to_hex(),
                            amount_in = amount_in,
                            amount_out = amount_out,
                            faucet_in = %order.asset_in.faucet_id().to_hex(),
                            faucet_out = %order.asset_out.faucet_id().to_hex(),
                            "Swap successful"
                        );
                        pools
                            .get_mut(&order.asset_in.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_balances(new_base_pool_balance);
                        pools
                            .get_mut(&order.asset_out.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_balances(new_quote_pool_balance);
                        let (_, note) = self.state.pluck_note(&order.id)?;
                        order_executions.push(OrderExecution::Swap(ExecutionDetails {
                            note,
                            order,
                            amount_out,
                            in_pool_balances: new_base_pool_balance,
                            out_pool_balances: new_quote_pool_balance,
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
                        order_executions.push(OrderExecution::FailedOrder(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: base_pool_state.balances,
                            out_pool_balances: quote_pool_state.balances,
                        }));
                    }
                }
            }
        }
        Ok(MatchingCycle {
            executions: order_executions,
            new_pool_states: pools,
        })
    }

    async fn execute_orders(
        &mut self,
        executions: Vec<OrderExecution>,
        client: &mut MidenClient,
    ) -> Result<()> {
        client.sync_state().await?;
        let pool_account_id = self.state.config().pool_account_id;
        let network_id = self.state.config().network_id;
        let mut input_notes = Vec::new();
        let mut expected_future_notes = Vec::new();
        let mut expected_output_recipients = Vec::new();
        let mut advice_map = AdviceMap::default();

        for execution in executions {
            let note_execution_details = match execution {
                OrderExecution::Swap(execution_details) => {
                    NoteExecutionDetails::Payout(self.prepare_payout(execution_details, false)?)
                }
                OrderExecution::FailedOrder(execution_details) => {
                    NoteExecutionDetails::Payout(self.prepare_payout(execution_details, true)?)
                }
                OrderExecution::PastDeadline(execution_details) => {
                    NoteExecutionDetails::Payout(self.prepare_payout(execution_details, true)?)
                }
                OrderExecution::Deposit(execution_details) => {
                    NoteExecutionDetails::ConsumeWithArgs((
                        execution_details.note,
                        Word::from(&[
                            Felt::new(execution_details.amount_out),
                            Felt::new(
                                execution_details
                                    .in_pool_balances
                                    .reserve_with_slippage
                                    .to::<u64>(),
                            ),
                            Felt::new(execution_details.in_pool_balances.reserve.to::<u64>()),
                            Felt::new(
                                execution_details
                                    .in_pool_balances
                                    .total_liabilities
                                    .to::<u64>(),
                            ),
                        ]),
                    ))
                }
                OrderExecution::Withdraw(execution_details) => {
                    let mut details = self.prepare_payout(execution_details.clone(), false)?;
                    details.args = Some(Word::from(&[
                        Felt::new(execution_details.amount_out),
                        Felt::new(
                            execution_details
                                .in_pool_balances
                                .reserve_with_slippage
                                .to::<u64>(),
                        ),
                        Felt::new(execution_details.in_pool_balances.reserve.to::<u64>()),
                        Felt::new(
                            execution_details
                                .in_pool_balances
                                .total_liabilities
                                .to::<u64>(),
                        ),
                    ]));
                    debug!("Withdraw payout details.args: {:?}", details.args);
                    NoteExecutionDetails::Payout(details)
                }
            };

            match note_execution_details {
                NoteExecutionDetails::Payout(payout) => {
                    debug!("Payout args: {:?}", payout.args);
                    let advice_key = payout.note.serial_num();
                    expected_future_notes.push((payout.details, payout.tag));
                    expected_output_recipients.push(payout.recipient);
                    input_notes.push((payout.note, payout.args));
                    advice_map.insert(advice_key, payout.advice_map_value);
                }
                NoteExecutionDetails::ConsumeWithArgs((note, args)) => {
                    input_notes.push((note, Some(args)));
                }
            }
        }

        for note in &expected_future_notes {
            info!("Expected future note P2ID id: {:?}", note.0.id());
            for asset in note.0.assets().iter_fungible() {
                info!(
                    "Expected future note P2ID asset (amount: {}) with faucet_id ({:?}), prefix|suffix: {:?} | {:?}",
                    asset.amount(),
                    asset.faucet_id().to_bech32(network_id.clone()),
                    asset.faucet_id().prefix().as_felt(),
                    asset.faucet_id().suffix()
                );
            }
        }

        debug!("Input notes count: {:?}", input_notes.len());

        let consume_req = TransactionRequestBuilder::new()
            .extend_advice_map(advice_map)
            .unauthenticated_input_notes(input_notes.clone())
            .expected_future_notes(expected_future_notes)
            .expected_output_recipients(expected_output_recipients.clone())
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build batch transaction request: {}", e))?;

        info!("All Recipients: {:?}", expected_output_recipients.len());
        for recipient in expected_output_recipients.clone() {
            info!("Recipient digest: {:?}", recipient.digest());
        }

        let tx = client
            .execute_transaction(pool_account_id, consume_req)
            .await
            .map_err(|e| {
                error!(
                    error = ?e,
                    pool_id = %pool_account_id.to_hex(),
                    input_notes = input_notes.len(),
                    expected_recipients = expected_output_recipients.len(),
                    "Failed to execute transaction"
                );
                // TODO: if this fails, return funds and propagate failure
                anyhow::anyhow!("Failed to execute transaction: {:?}", e)
            })?;

        let proven_tx = client.prove_transaction(&tx).await?;

        // Serialize the proof to whatever file
        let proof_bytes = proven_tx.to_bytes();
        tokio::fs::write("proof.bin", proof_bytes).await?;

        info!("Proof written to proof.bin");

        // let tx_id = client
        //     .submit_new_transaction(pool_account_id, consume_req)
        //     .await
        //     .map_err(|e| {
        //         error!(
        //             error = ?e,
        //             pool_id = %pool_account_id.to_hex(),
        //             input_notes = input_notes.len(),
        //             expected_recipients = expected_output_recipients.len(),
        //             "Failed to submit batch transaction"
        //         );
        //         // TODO: if this fails, return funds and propagate failure
        //         anyhow::anyhow!("Failed to create and submit batch transaction: {:?}", e)
        //     })?;

        // Submitted the transaction (exactly like the test)
        // info!("Submitted TX: {:?}", tx_id);

        // client.sync_state().await?;

        // print_transaction_info(&tx_id);

        Ok(())
    }

    fn prepare_payout(
        &self,
        execution_details: ExecutionDetails,
        return_asset_in: bool,
    ) -> Result<PayoutDetails> {
        let asset_out = if return_asset_in {
            execution_details.order.asset_in
        } else {
            FungibleAsset::new(
                execution_details.order.asset_out.faucet_id(),
                execution_details.amount_out,
            )?
        };
        let user_account_id = execution_details.order.creator_id;
        let serial_num = execution_details.note.serial_num();
        let note = execution_details.note;
        let asset_out_faucet_id = asset_out.faucet_id().to_bech32(self.network_id.clone());
        let asset_out = Asset::Fungible(asset_out);
        debug!("prepare_payout asset_out: {:?}", asset_out);
        let p2id_serial_num = [
            serial_num[0],
            serial_num[1],
            serial_num[2],
            Felt::new(serial_num[3].as_int() + 1),
        ];

        info!(
            "Calculated {asset_out:?} asset_out from faucet: {}, for recipient {} {} {} with serial number {:?}",
            asset_out_faucet_id,
            user_account_id.to_bech32(self.network_id.clone()),
            user_account_id.prefix().as_felt(),
            user_account_id.suffix(),
            p2id_serial_num
        );

        let p2id = create_p2id_note(
            self.pool_account_id,
            user_account_id,
            vec![asset_out],
            NoteType::Public,
            Felt::new(0),
            p2id_serial_num.into(),
        )?;

        let in_pool_balances = execution_details.in_pool_balances;
        let out_pool_balances = execution_details.out_pool_balances;
        debug!(
            "-----------------------------------In pool balances: {:?}",
            in_pool_balances
        );
        debug!(
            "-----------------------------------Out pool balances: {:?}",
            out_pool_balances
        );

        let advice_map_value = vec![
            Felt::new(in_pool_balances.total_liabilities.to::<u64>()),
            Felt::new(in_pool_balances.reserve.to::<u64>()),
            Felt::new(in_pool_balances.reserve_with_slippage.to::<u64>()),
            Felt::new(asset_out.unwrap_fungible().amount()),
            Felt::new(out_pool_balances.total_liabilities.to::<u64>()),
            Felt::new(out_pool_balances.reserve.to::<u64>()),
            Felt::new(out_pool_balances.reserve_with_slippage.to::<u64>()),
            Felt::new(0),
        ];

        Ok(PayoutDetails {
            note,
            args: None,
            advice_map_value,
            details: NoteDetails::from(p2id.clone()),
            tag: p2id.metadata().tag(),
            recipient: p2id.recipient().clone(),
        })
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

        let base_pool_decimals = self
            .state
            .config()
            .get_asset_decimals_by_faucet_id(&asset_in_id)?;
        let quote_pool_decimals = self
            .state
            .config()
            .get_asset_decimals_by_faucet_id(&asset_out_id)?;
        Ok((
            (*base_pool_state, base_pool_decimals),
            (*quote_pool_state, quote_pool_decimals),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, LiquidityPoolConfig},
        oracle_sse::PriceData,
        pool::{PoolBalances, PoolState, get_deposit_lp_amount_out},
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
    use miden_objects::{FieldElement, account::AccountIdVersion};
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
            let state = Arc::new(AmmState::new(config, broadcaster.clone()).await);

            let mut pool_a = PoolState::new(pool_account_id, faucet_a_id);
            let mut pool_b = PoolState::new(pool_account_id, faucet_b_id);
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
            TradingEngine::new(
                "testing_store.sqlite3",
                self.state.clone(),
                self.broadcaster.clone(),
            )
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
            order_type: OrderType::Swap,
            p2id_tag: 0,
            creator_id: ctx.pool_account_id,
        };

        let engine = ctx.create_engine();
        let result = engine.get_liq_pools_for_order(&ctx.state.liquidity_pools(), &order);

        assert!(result.is_ok(), "Unable to get liquidity pools");
        let ((_base_pool, base_decimals), (_quote_pool, quote_decimals)) = result.unwrap();

        assert_eq!(
            base_decimals, 6,
            "Base pool (asset_in) must have 6 decimals"
        );
        assert_eq!(
            quote_decimals, 12,
            "Quote pool (asset_out) must have 12 decimals"
        );
    }

    #[tokio::test]
    async fn test_matching_skipped_with_stale_prices() {
        let ctx = TestContext::new().await;

        // Add a swap order
        let note = ctx.create_swap_note(1000, 1);
        ctx.state
            .add_order(note, OrderType::Swap)
            .expect("Should add order");
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
        let engine = ctx.create_engine();
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
    }

    /// Test that `get_deposit_lp_amount_out` returns a `PoolState` with correctly updated
    /// `lp_total_supply`, enabling consecutive deposits to use accurate LP supply values.
    #[test]
    fn test_get_deposit_lp_amount_out_returns_updated_lp_total_supply() {
        // Given: a pool with initial LP supply and reserves
        let initial_lp_supply: u64 = 1_000_000;
        let initial_reserve = U256::from(10_000_000u64);
        let asset_decimals = U256::from(6u64);
        let deposit_amount = U256::from(1_000_000u64);

        let mut pool = PoolState::default();
        pool.balances = PoolBalances {
            reserve: initial_reserve,
            reserve_with_slippage: initial_reserve,
            total_liabilities: initial_reserve,
        };
        pool.lp_total_supply = initial_lp_supply;

        // When: calculating LP amount out for a deposit
        let (lp_out, new_pool_state) = get_deposit_lp_amount_out(
            &pool,
            deposit_amount,
            U256::from(pool.lp_total_supply),
            asset_decimals,
        );

        // Then: the returned pool state has lp_total_supply = initial + minted
        assert!(lp_out > U256::ZERO, "must mint LP tokens");
        assert_eq!(
            new_pool_state.lp_total_supply,
            U256::from(initial_lp_supply)
                .saturating_add(lp_out)
                .saturating_to::<u64>(),
            "returned pool state must have lp_total_supply = initial + minted"
        );
    }

    /// Test that consecutive deposits correctly accumulate `lp_total_supply`.
    #[test]
    fn test_consecutive_deposits_accumulate_lp_total_supply() {
        // Given: a pool with initial LP supply
        let initial_lp_supply: u64 = 1_000_000;
        let initial_reserve = U256::from(10_000_000u64);
        let asset_decimals = U256::from(6u64);
        let deposit_amount = U256::from(1_000_000u64);

        let mut pool = PoolState::default();
        pool.balances = PoolBalances {
            reserve: initial_reserve,
            reserve_with_slippage: initial_reserve,
            total_liabilities: initial_reserve,
        };
        pool.lp_total_supply = initial_lp_supply;

        // When: two consecutive deposits are processed
        let (lp_out_1, pool_after_first) = get_deposit_lp_amount_out(
            &pool,
            deposit_amount,
            U256::from(pool.lp_total_supply),
            asset_decimals,
        );

        let (lp_out_2, pool_after_second) = get_deposit_lp_amount_out(
            &pool_after_first,
            deposit_amount,
            U256::from(pool_after_first.lp_total_supply),
            asset_decimals,
        );

        // Then: the final `lp_total_supply` reflects both deposits
        let expected_final_supply = U256::from(initial_lp_supply)
            .saturating_add(lp_out_1)
            .saturating_add(lp_out_2)
            .saturating_to::<u64>();

        assert_eq!(
            pool_after_second.lp_total_supply, expected_final_supply,
            "final lp_total_supply must equal initial + first deposit + second deposit"
        );
        assert!(
            pool_after_second.lp_total_supply > pool_after_first.lp_total_supply,
            "second deposit must increase lp_total_supply beyond first deposit"
        );
    }
}
