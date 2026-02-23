use crate::{
    amm_state::AmmState,
    order::{Order, OrderType},
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateDetails, OrderUpdateEvent},
};
use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::{
    Felt, Word,
    account::AccountId,
    address::NetworkId,
    asset::{Asset, FungibleAsset},
    note::{Note, NoteRecipient, NoteTag, NoteType},
    transaction::TransactionRequestBuilder,
};
use miden_protocol::{note::NoteDetails, vm::AdviceMap};
use rand::seq::SliceRandom;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};
use zoro_miden::{
    client::MidenClient,
    curve::get_curve_amount_out,
    note::TrustedNote,
    pool::ZoroPool,
    pool_state::{PoolBalances, PoolState},
};

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

impl OrderExecution {
    fn details(&self) -> &ExecutionDetails {
        match self {
            OrderExecution::Swap(d)
            | OrderExecution::Deposit(d)
            | OrderExecution::Withdraw(d)
            | OrderExecution::FailedOrder(d)
            | OrderExecution::PastDeadline(d) => d,
        }
    }
}

pub(crate) struct MatchingCycle {
    executions: Vec<OrderExecution>,
    new_pool_states: HashMap<AccountId, PoolState>,
}
pub struct TradingEngine {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
    last_match_time: Arc<Mutex<Instant>>,
    network_id: NetworkId,
    pool_account_id: AccountId,
    zoro_pool: ZoroPool,
}

enum NoteExecutionDetails {
    Payout(PayoutDetails),
    ConsumeWithArgs((Note, Word)),
}

struct PayoutDetails {
    pub note: Note,
    pub args: Option<Word>,
    pub advice_map_value: Option<Vec<Felt>>,
    pub details: NoteDetails,
    pub tag: NoteTag,
    pub recipient: NoteRecipient,
}

impl TradingEngine {
    pub async fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Result<Self> {
        let config = state.config();
        let pool_account_id = config.pool_account_id;
        let network_id = config.miden_endpoint.to_network_id();
        let zoro_pool =
            ZoroPool::new_from_existing_pool(&pool_account_id, config.liquidity_pools).await?;
        Ok(Self {
            state,
            broadcaster,
            last_match_time: Arc::new(Mutex::new(Instant::now())),
            network_id,
            pool_account_id,
            zoro_pool,
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        let min_match_interval = Duration::from_millis(100); // Debounce
        let max_match_interval = Duration::from_millis(1000); // Max wait (event-driven)
        let config = self.state.config();
        let mut client = MidenClient::new(
            config.miden_endpoint,
            config.keystore_path,
            config.store_path,
            None,
        )
        .await?;
        info!("Init pool states in AmmState");
        self.zoro_pool.miden_account_mut().refetch_account().await?;
        self.state
            .set_pool_states(self.zoro_pool.pool_states().clone());
        info!(
            "Starting event-driven trading engine (min: {:?}, max: {:?})",
            min_match_interval, max_match_interval
        );

        // Subscribe to events that should trigger matching
        let mut interval = tokio::time::interval(max_match_interval);
        loop {
            interval.tick().await;
            self.run_matching_if_ready(min_match_interval, &mut client)
                .await;
        }
    }

    async fn run_matching_if_ready(&mut self, min_interval: Duration, client: &mut MidenClient) {
        let last_match = { *self.last_match_time.lock().unwrap() };
        if last_match.elapsed() < min_interval {
            return; // Skip this match cycle
        }

        // Check if oracle prices for high-liquidity tokens are fresh.
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
                        let status = match execution {
                            OrderExecution::Swap(_)
                            | OrderExecution::Deposit(_)
                            | OrderExecution::Withdraw(_) => OrderStatus::Matching,
                            OrderExecution::FailedOrder(_) => OrderStatus::Failed,
                            OrderExecution::PastDeadline(_) => OrderStatus::Expired,
                        };

                        if status != OrderStatus::Matching {
                            let details = execution.details();
                            let order_id = details.order.id;
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

                    // TODO: Put this down on success after Poisoned note problem is resolved
                    self.state.flush_open_orders();

                    // Execute swaps and broadcast success
                    match self.execute_orders(executions.clone(), client).await {
                        Ok(_) => {
                            for execution in &executions {
                                let _ = self.state.pluck_note(&execution.details().order.id);
                            }

                            for (faucet_id, pool_state) in matching_cycle.new_pool_states {
                                self.state.update_pool_state(&faucet_id, pool_state);
                            }

                            // Broadcast order status for executed orders
                            for execution in &executions {
                                if matches!(
                                    execution,
                                    OrderExecution::Swap(_)
                                        | OrderExecution::Deposit(_)
                                        | OrderExecution::Withdraw(_)
                                ) {
                                    let details = execution.details();
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
        let mut pool_states = self.zoro_pool.pool_states().clone();
        let mut orders = self.state.get_open_orders();

        // MEV protection: randomize order processing sequence
        orders.shuffle(&mut rand::rng());

        let mut order_executions = Vec::new();
        let now = Utc::now();
        for order in orders {
            let ((base_pool_state, base_pool_decimals), (quote_pool_state, quote_pool_decimals)) =
                self.get_liq_pools_for_order(&pool_states, &order)?;
            debug!(
                order = ?order,
                base_pool_state = ?base_pool_state,
                quote_pool_state = ?quote_pool_state,
                "Processing order"
            );

            if order.deadline < now {
                let note = self.state.get_note(&order.id)?;
                warn!(
                    "Order {:?} is past deadline (by {})",
                    order.order_type,
                    now - order.deadline
                );
                order_executions.push(OrderExecution::PastDeadline(ExecutionDetails {
                    note,
                    order,
                    amount_out: order.asset_in.amount(),
                    in_pool_balances: *base_pool_state.balances(),
                    out_pool_balances: *quote_pool_state.balances(),
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
                    let (amount_out, new_lp_total_supply, new_pool_balances) = base_pool_state
                        .get_deposit_lp_amount_out(U256::from(order.asset_in.amount()))?;
                    let amount_out = amount_out.to::<u64>();
                    if amount_out > 0 && amount_out >= order.asset_out.amount() {
                        let note = self.state.get_note(&order.id)?;
                        pool_states
                            .get_mut(&order.asset_in.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_state(new_pool_balances, new_lp_total_supply);
                        order_executions.push(OrderExecution::Deposit(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: new_pool_balances,
                            out_pool_balances: *quote_pool_state.balances(),
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
                        let note = self.state.get_note(&order.id)?;
                        order_executions.push(OrderExecution::FailedOrder(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: *base_pool_state.balances(),
                            out_pool_balances: *quote_pool_state.balances(),
                        }));
                    }
                }
                OrderType::Withdraw => {
                    let (amount_out, new_lp_total_supply, new_pool_balances) = base_pool_state
                        .get_withdraw_asset_amount_out(U256::from(order.asset_in.amount()))?;

                    let amount_out = amount_out.to::<u64>();
                    if amount_out > 0 && amount_out >= order.asset_out.amount() {
                        let note = self.state.get_note(&order.id)?;
                        pool_states
                            .get_mut(&order.asset_out.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_state(new_pool_balances, new_lp_total_supply);
                        order_executions.push(OrderExecution::Withdraw(ExecutionDetails {
                            note,
                            order,
                            amount_out,
                            in_pool_balances: new_pool_balances,
                            out_pool_balances: *quote_pool_state.balances(),
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
                        let note = self.state.get_note(&order.id)?;
                        order_executions.push(OrderExecution::FailedOrder(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: *base_pool_state.balances(),
                            out_pool_balances: *quote_pool_state.balances(),
                        }));
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
                                let pool_in = pool_states
                                    .get(&order.asset_in.faucet_id())
                                    .ok_or(anyhow!("Missing pool in state"))?
                                    .clone();

                                let pool_out = pool_states
                                    .get_mut(&order.asset_out.faucet_id())
                                    .ok_or(anyhow!("Missing pool in state"))?;
                                let note = self.state.get_note(&order.id)?;
                                order_executions.push(OrderExecution::FailedOrder(
                                    ExecutionDetails {
                                        in_pool_balances: *pool_in.balances(),
                                        out_pool_balances: *pool_out.balances(),
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
                            beneficiary = %order.beneficiary_id.to_hex(),
                            amount_in = amount_in,
                            amount_out = amount_out,
                            faucet_in = %order.asset_in.faucet_id().to_hex(),
                            faucet_out = %order.asset_out.faucet_id().to_hex(),
                            "Swap successful"
                        );
                        pool_states
                            .get_mut(&order.asset_in.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_balances(new_base_pool_balance);
                        pool_states
                            .get_mut(&order.asset_out.faucet_id())
                            .ok_or(anyhow!("Missing pool in state"))?
                            .update_balances(new_quote_pool_balance);
                        let note = self.state.get_note(&order.id)?;
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
                        let note = self.state.get_note(&order.id)?;
                        order_executions.push(OrderExecution::FailedOrder(ExecutionDetails {
                            note,
                            order,
                            amount_out: order.asset_in.amount(),
                            in_pool_balances: *base_pool_state.balances(),
                            out_pool_balances: *quote_pool_state.balances(),
                        }));
                    }
                }
            }
        }
        Ok(MatchingCycle {
            executions: order_executions,
            new_pool_states: pool_states,
        })
    }

    async fn execute_orders(
        &mut self,
        executions: Vec<OrderExecution>,
        miden_client: &mut MidenClient,
    ) -> Result<()> {
        miden_client.sync_state().await?;
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
                    NoteExecutionDetails::Payout(self.prepare_payout(execution_details, false)?)
                }
            };

            match note_execution_details {
                NoteExecutionDetails::Payout(payout) => {
                    debug!("Payout args: {:?}", payout.args);
                    let advice_key = payout.note.serial_num();
                    expected_future_notes.push((payout.details, payout.tag));
                    expected_output_recipients.push(payout.recipient);
                    input_notes.push((payout.note, payout.args));
                    if let Some(advice_map_value) = payout.advice_map_value {
                        advice_map.insert(advice_key, advice_map_value);
                    }
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

        info!("Input notes count: {:?}", input_notes.len());
        info!("All Recipients: {:?}", expected_output_recipients.len());
        for recipient in expected_output_recipients.clone() {
            info!("Recipient digest: {:?}", recipient.digest());
        }
        info!("All Future notes: {:?}", expected_future_notes.len());

        let consume_req = TransactionRequestBuilder::new()
            .extend_advice_map(advice_map)
            .input_notes(input_notes.clone())
            .expected_future_notes(expected_future_notes)
            .expected_output_recipients(expected_output_recipients.clone())
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build batch transaction request: {}", e))?;

        let tx_id = miden_client
            .client_mut()
            .submit_new_transaction(pool_account_id, consume_req)
            .await
            .map_err(|e| {
                error!(
                    error = ?e,
                    pool_id = %pool_account_id.to_hex(),
                    input_notes = input_notes.len(),
                    expected_recipients = expected_output_recipients.len(),
                    "Failed to submit batch transaction"
                );
                anyhow::anyhow!("Failed to create and submit batch transaction: {:?}", e)
            })?;
        // Submitted the transaction (exactly like the test)
        info!("Submitted TX: {:?}", tx_id);

        miden_client.sync_state().await?;

        MidenClient::print_transaction_info(&tx_id);

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
        let user_account_id = if return_asset_in {
            execution_details.order.creator_id
        } else {
            execution_details.order.beneficiary_id
        };
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

        let p2id_note = TrustedNote::new_p2id(
            self.pool_account_id,
            user_account_id,
            vec![asset_out],
            NoteType::Public,
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

        let (args, advice_map_value): (Option<Word>, Option<Vec<Felt>>) =
            if execution_details.order.order_type.eq(&OrderType::Swap) {
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
                (None, Some(advice_map_value))
            } else if execution_details.order.order_type.eq(&OrderType::Deposit)
                || execution_details.order.order_type.eq(&OrderType::Withdraw)
            {
                // set amount_out to zero so it triggers returning asset in MASM
                let amount_out = if return_asset_in {
                    0
                } else {
                    execution_details.amount_out
                };

                let args = Some(Word::from(&[
                    Felt::new(amount_out),
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
                (args, None)
            } else {
                (None, None)
            };

        Ok(PayoutDetails {
            note,
            args,
            advice_map_value,
            details: NoteDetails::from(p2id_note.note().clone()),
            tag: p2id_note.note().metadata().tag(),
            recipient: p2id_note.note().recipient().clone(),
        })
    }

    fn get_liq_pools_for_order(
        &self,
        liq_pool_states: &HashMap<AccountId, PoolState>,
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
    use crate::{config::Config, oracle_sse::PriceData, websocket::EventBroadcaster};
    use anyhow::Result;
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
    use zoro_miden::pool::LiquidityPoolConfig;

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
            let state = Arc::new(AmmState::new(config, broadcaster.clone()).await);

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
