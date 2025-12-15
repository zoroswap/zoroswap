use crate::{
    ZoroStorageSettings,
    amm_state::AmmState,
    common::{instantiate_client, print_transaction_info},
    order::Order,
    pool::{PoolState, get_curve_amount_out},
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
use std::{sync::Arc, thread::sleep, time::Duration};
use tracing::{error, info, warn};
use zoro_miden_client::{MidenClient, create_p2id_note};

#[derive(Debug)]
struct ExecutionDetails {
    note: Note,
    order: Order,
    amount_out: u64,
}

#[derive(Debug)]
enum OrderExecution {
    Swap(ExecutionDetails),
    FailedSwap(ExecutionDetails),
    PastDeadline(ExecutionDetails),
}

pub struct TradingEngine {
    state: Arc<AmmState>,
    store_path: String,
}

impl TradingEngine {
    pub fn new(store_path: &str, state: Arc<AmmState>) -> Self {
        Self {
            store_path: store_path.to_string(),
            state,
        }
    }

    pub async fn start(&mut self) {
        let tick_interval = self.state.config().amm_tick_interval;
        let mut client = instantiate_client(
            self.state.config(),
            ZoroStorageSettings::trading_storage(self.store_path.to_string()),
        )
        .await
        .unwrap_or_else(|err| panic!("Failed to instantiate client in trading engine: {err:?}"));
        info!("Starting trading engine with {tick_interval} ms interval.");
        loop {
            match self.run_matching_cycle() {
                Ok(orders_to_execute) => {
                    if !orders_to_execute.is_empty()
                        && let Err(e) = self.run_executions(orders_to_execute, &mut client).await
                    {
                        error!("{e}");
                    }
                }
                Err(e) => {
                    error!("{e}")
                }
            };
            sleep(Duration::from_millis(tick_interval));
        }
    }

    fn run_matching_cycle(&self) -> Result<Vec<OrderExecution>> {
        let pools = self.state.liquidity_pools().clone();
        let orders = self.state.flush_open_orders();
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
        pool::PoolState,
    };
    use chrono::Utc;
    use miden_client::{
        address::NetworkId,
        asset::{FungibleAsset, TokenSymbol},
    };
    use miden_lib::account::faucets::BasicFungibleFaucet;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_get_liq_pools_for_order_uses_correct_asset_ids() {
        // Create two different account IDs for the faucets using dummy accounts
        use miden_client::account::{AccountStorageMode, AccountType};
        use miden_objects::account::AccountIdVersion;
        let faucet_a_id = AccountId::dummy(
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            AccountIdVersion::Version0,
            AccountType::FungibleFaucet,
            AccountStorageMode::Public,
        );
        let faucet_b_id = AccountId::dummy(
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            AccountIdVersion::Version0,
            AccountType::FungibleFaucet,
            AccountStorageMode::Public,
        );
        let pool_account_id = AccountId::dummy(
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3],
            AccountIdVersion::Version0,
            AccountType::RegularAccountImmutableCode,
            AccountStorageMode::Public,
        );

        // Create faucets with different decimals
        let symbol_a = TokenSymbol::new("TKA").unwrap();
        let symbol_b = TokenSymbol::new("TKB").unwrap();
        let faucet_a = BasicFungibleFaucet::new(symbol_a, 6, Felt::new(1_000_000)).unwrap();
        let faucet_b = BasicFungibleFaucet::new(symbol_b, 12, Felt::new(1_000_000)).unwrap();

        // Create config with these faucets
        let config = Config {
            pool_account_id,
            liquidity_pools: vec![
                LiquidityPoolConfig {
                    name: "TokenA",
                    symbol: "TKA",
                    decimals: 6,
                    faucet_id: faucet_a_id,
                    oracle_id: "oracle_a",
                },
                LiquidityPoolConfig {
                    name: "TokenB",
                    symbol: "TKB",
                    decimals: 12,
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

        // Create AMM state and populate it
        let state = Arc::new(AmmState::new(config).await);
        state.faucet_metadata().insert(faucet_a_id, faucet_a);
        state.faucet_metadata().insert(faucet_b_id, faucet_b);

        // Create pool states
        let pool_a_state = PoolState::new(pool_account_id, faucet_a_id);
        let pool_b_state = PoolState::new(pool_account_id, faucet_b_id);
        state.liquidity_pools().insert(faucet_a_id, pool_a_state);
        state.liquidity_pools().insert(faucet_b_id, pool_b_state);

        // Create an order that swaps from faucet_a to faucet_b
        let asset_in = FungibleAsset::new(faucet_a_id, 100).unwrap();
        let asset_out = FungibleAsset::new(faucet_b_id, 200).unwrap();
        let order = Order {
            id: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline: Utc::now(),
            is_limit_order: false,
            asset_in,
            asset_out,
            p2id_tag: 0,
            creator_id: pool_account_id,
        };

        // Create trading engine, call method
        let engine = TradingEngine::new("testing_store.sqlite3", state.clone());
        let result = engine.get_liq_pools_for_order(&state.liquidity_pools(), &order);

        // Verify result
        assert!(result.is_ok(), "Unable to get liquidity pools");
        let ((_base_pool, base_decimals), (_quote_pool, quote_decimals)) = result.unwrap();

        assert_eq!(
            base_decimals, 6,
            "Base pool (asset_in) must have 6 decimals"
        );
        assert_eq!(
            quote_decimals, 12,
            "Quote pool (asset_out) must have 12 decimals, not {}.",
            quote_decimals
        );
    }
}
