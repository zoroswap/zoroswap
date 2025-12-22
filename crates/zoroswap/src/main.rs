mod amm_state;
mod common;
mod config;
mod faucet;
mod note_serialization;
mod notes_listener;
mod oracle_sse;
mod order;
mod pool;
mod server;
mod trading_engine;
mod websocket;

use amm_state::AmmState;
use clap::Parser;
use common::{enable_wal_mode, instantiate_client};
use config::Config;
use dotenv::dotenv;
use faucet::{FaucetMintInstruction, GuardedFaucet};
use notes_listener::NotesListener;
use oracle_sse::OracleSSEClient;
use server::{AppState, create_router};
use std::{sync::Arc, thread};
use tokio::{runtime::Builder, sync::mpsc::Sender};
use tracing::{error, info};
use trading_engine::TradingEngine;
use websocket::{ConnectionManager, EventBroadcaster};
use zoro_miden_client::delete_client_store;

#[derive(Parser, Debug)]
#[command(name = "zoro-server")]
#[command(about = "Zoro DEX server", long_about = None)]
struct Args {
    /// Path to the config file
    #[arg(short, long, default_value = "./config.toml")]
    config: String,

    /// Path to the MASM files directory
    #[arg(short, long, default_value = "./crates/zoroswap/masm")]
    masm_path: String,

    /// Path to the keystore directory
    #[arg(short, long, default_value = "./keystore")]
    keystore_path: String,

    /// Path to the SQLite store file
    #[arg(short, long, default_value = "./store.sqlite3")]
    store_path: String,
}

fn main() {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter("info,zoro=debug,miden-client=info")
        .init();
    dotenv().ok();
    let runtime = tokio::runtime::Runtime::new()
        .unwrap_or_else(|err| panic!("Failed to create tokio runtime: {err:?}"));
    match runtime.block_on(async {
        info!("[INIT] Parsing config");
        info!("Deleting old sqlite3 store");
        delete_client_store(&args.store_path).await;

        // Enable WAL mode for better concurrent database access
        // This must be done before any clients are created
        if let Err(e) = enable_wal_mode(&args.store_path) {
            // Non-fatal: WAL mode is an optimization, not a requirement
            tracing::warn!("Failed to enable WAL mode: {e}");
        }

        let config = Config::from_config_file(
            &args.config,
            &args.masm_path,
            &args.keystore_path,
            &args.store_path,
        ).map_err(|e| e.to_string())?;
        let mut init_client = instantiate_client(&config, config.store_path)
            .await
            .unwrap_or_else(|err| panic!("Failed to instantiate init client: {err:?}"));
        info!(
            "[INFO] Pool information\n\n\tpool_id: {}\n\tpool0 faucet_id: {}\n\tpool1 faucet_id: {}\n",
            config.pool_account_id.to_bech32(config.network_id.clone()),
            config.liquidity_pools[0].faucet_id.to_bech32(config.network_id.clone()),
            config.liquidity_pools[1].faucet_id.to_bech32(config.network_id.clone()),
        );
        // Initialize WebSocket infrastructure
        info!("[INIT] Initializing WebSocket infrastructure");
        let event_broadcaster = Arc::new(EventBroadcaster::new());
        let connection_manager = Arc::new(ConnectionManager::new());

        // Create and initialize AMM state
        let mut amm_state = AmmState::new(config).await;
        amm_state.set_broadcaster(event_broadcaster.clone());
        let amm_state = Arc::new(amm_state);

        info!("[INIT] Initializing faucet metadata");
        amm_state
            .init_faucet_metadata(&mut init_client)
            .await
            .map_err(|e| e.to_string())?;
        info!("[INIT] Initializing liquidity pool states");
        amm_state
            .init_liquidity_pool_states(&mut init_client)
            .await
            .map_err(|e| e.to_string())?;

        // Initialize components with broadcaster
        let trading_engine = TradingEngine::new(&args.store_path, amm_state.clone(), event_broadcaster.clone());
        let oracle_client = OracleSSEClient::new(amm_state.clone(), event_broadcaster.clone());

        info!("[INIT] Initializing oracle prices");
        oracle_client
            .init_prices_in_state()
            .await
            .map_err(|e| e.to_string())?;

        let (guarded_faucet, faucet_tx) = GuardedFaucet::new(amm_state.config().clone());
        let notes_listener = NotesListener::new(amm_state.clone(), event_broadcaster.clone());
        Ok::<
            (
                OracleSSEClient,
                TradingEngine,
                Arc<AmmState>,
                GuardedFaucet,
                Sender<FaucetMintInstruction>,
                NotesListener,
                Arc<ConnectionManager>,
                Arc<EventBroadcaster>,
            ),
            String,
        >((
            oracle_client,
            trading_engine,
            amm_state,
            guarded_faucet,
            faucet_tx,
            notes_listener,
            connection_manager,
            event_broadcaster,
        ))
    }) {
        Ok((
            oracle_client,
            mut trading_engine,
            amm_state,
            mut guarded_faucet,
            faucet_tx,
            mut notes_listener,
            connection_manager,
            event_broadcaster,
        )) => {
            thread::scope(|s| {
                s.spawn(move || {
                    let rt = Builder::new_current_thread().enable_all().build()
                        .unwrap_or_else(|err| panic!("Failed building runtime for trading engine: {err:?}"));
                    rt.block_on(async {
                        info!("[RUN] Starting trading engine");
                        trading_engine.start().await;
                    });
                });

                s.spawn(move || {
                    let rt = Builder::new_current_thread().enable_all().build()
                        .unwrap_or_else(|err| panic!("Failed building runtime for guarded faucet: {err:?}"));
                    rt.block_on(async {
                        info!("[RUN] Starting guarded faucet");
                        if let Err(e) = guarded_faucet.start().await {
                            error!("Critical error on faucet: {e:?}. Exiting with status 1.");
                            std::process::exit(1);
                        }
                    });
                });

                s.spawn(move || {
                    let rt = Builder::new_current_thread().enable_all().build()
                        .unwrap_or_else(|err| panic!("Failed building runtime for notes listener: {err:?}"));
                    rt.block_on(async {
                        info!("[RUN] Starting notes listener");
                        notes_listener.start().await
                    });
                });

                run_main_tokio((oracle_client, amm_state, faucet_tx, connection_manager, event_broadcaster));
            });
        }
        Err(e) => {
            println!("[INIT] Critical error: {e:?}");
        }
    };
}

/// Spawn bridge tasks that forward events from EventBroadcaster to WebSocket clients
fn spawn_event_bridge_tasks(
    event_broadcaster: Arc<EventBroadcaster>,
    connection_manager: Arc<ConnectionManager>,
) {
    use tokio::sync::broadcast::error::RecvError;
    use tracing::{debug, error, warn};
    use websocket::{ServerMessage, SubscriptionChannel};

    // Bridge task for order updates
    {
        let broadcaster = event_broadcaster.clone();
        let conn_mgr = connection_manager.clone();
        tokio::spawn(async move {
            debug!("Order updates WebSocket bridge task started");
            let mut rx = broadcaster.subscribe_order_updates();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        debug!("Received order update event in bridge: order_id={}, note_id={}, status={:?}", event.order_id, event.note_id, event.status);
                        let message = ServerMessage::OrderUpdate {
                            order_id: event.order_id.to_string(),
                            note_id: event.note_id.clone(),
                            status: event.status,
                            timestamp: event.timestamp,
                            details: event.details.clone(),
                        };

                        // Broadcast to "all orders" channel
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::OrderUpdates { order_id: None },
                            message.clone(),
                        );

                        // Also broadcast to specific order channel for clients subscribed to this order
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::OrderUpdates {
                                order_id: Some(event.order_id.to_string())
                            },
                            message,
                        );
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!("Order updates bridge lagged, skipped {} events", skipped);
                    }
                    Err(RecvError::Closed) => {
                        error!("Order updates broadcast channel closed");
                        break;
                    }
                }
            }
        });
    }

    // Bridge task for oracle price updates
    {
        let broadcaster = event_broadcaster.clone();
        let conn_mgr = connection_manager.clone();
        tokio::spawn(async move {
            let mut rx = broadcaster.subscribe_oracle_prices();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let message = ServerMessage::OraclePriceUpdate {
                            oracle_id: event.oracle_id.clone(),
                            faucet_id: event.faucet_id,
                            price: event.price,
                            timestamp: event.timestamp,
                        };

                        // Broadcast to "all oracles" channel
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::OraclePrices { oracle_id: None },
                            message.clone(),
                        );

                        // Also broadcast to specific oracle channel for clients subscribed to this oracle
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::OraclePrices {
                                oracle_id: Some(event.oracle_id),
                            },
                            message,
                        );
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!("Oracle price updates bridge lagged, skipped {} events", skipped);
                    }
                    Err(RecvError::Closed) => {
                        error!("Oracle price updates broadcast channel closed");
                        break;
                    }
                }
            }
        });
    }

    // Bridge task for pool state updates
    {
        let broadcaster = event_broadcaster.clone();
        let conn_mgr = connection_manager.clone();
        tokio::spawn(async move {
            let mut rx = broadcaster.subscribe_pool_state();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let message = ServerMessage::PoolStateUpdate {
                            faucet_id: event.faucet_id.clone(),
                            balances: event.balances,
                            timestamp: event.timestamp,
                        };

                        // Broadcast to "all pools" channel
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::PoolState { faucet_id: None },
                            message.clone(),
                        );

                        // Also broadcast to specific pool channel for clients subscribed to this faucet
                        conn_mgr.broadcast_to_channel(
                            &SubscriptionChannel::PoolState {
                                faucet_id: Some(event.faucet_id),
                            },
                            message,
                        );
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!("Pool state updates bridge lagged, skipped {} events", skipped);
                    }
                    Err(RecvError::Closed) => {
                        error!("Pool state updates broadcast channel closed");
                        break;
                    }
                }
            }
        });
    }

    // Bridge task for stats updates
    {
        let broadcaster = event_broadcaster.clone();
        let conn_mgr = connection_manager.clone();
        tokio::spawn(async move {
            let mut rx = broadcaster.subscribe_stats();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let message = ServerMessage::StatsUpdate {
                            open_orders: event.open_orders,
                            closed_orders: event.closed_orders,
                            timestamp: event.timestamp,
                        };
                        conn_mgr.broadcast_to_channel(&SubscriptionChannel::Stats, message);
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!("Stats updates bridge lagged, skipped {} events", skipped);
                    }
                    Err(RecvError::Closed) => {
                        error!("Stats updates broadcast channel closed");
                        break;
                    }
                }
            }
        });
    }

    debug!("WebSocket event bridge tasks spawned");
}

#[tokio::main]
async fn run_main_tokio(
    (mut oracle_client, original_amm_state, faucet_tx, connection_manager, event_broadcaster): (
        OracleSSEClient,
        Arc<AmmState>,
        Sender<FaucetMintInstruction>,
        Arc<ConnectionManager>,
        Arc<EventBroadcaster>,
    ),
) {
    let server_url = original_amm_state.config().server_url;
    info!("[RUN] Starting oracle listener");
    tokio::spawn(async move {
        if let Err(e) = oracle_client.start().await {
            error!("Critical error on oracle client: {e}. Exiting with status 1.");
            std::process::exit(1);
        }
    });
    info!("[RUN] Starting WebSocket heartbeat task");
    connection_manager.clone().start_heartbeat_task();

    // Start bridge tasks to forward EventBroadcaster events to WebSocket clients
    info!("[RUN] Starting WebSocket event bridge tasks");
    spawn_event_bridge_tasks(event_broadcaster.clone(), connection_manager.clone());

    info!("[RUN] Starting ZORO server");
    let app = create_router(AppState {
        amm_state: original_amm_state.clone(),
        faucet_tx,
        connection_manager,
        event_broadcaster,
    });
    let listener = tokio::net::TcpListener::bind(server_url)
        .await
        .unwrap_or_else(|err| panic!("Failed to bind TCP listener to {}: {err:?}", server_url));
    info!("Server listening on {}", server_url);
    println!("\n🚀 Zoro server is running!");
    println!("📡 Available endpoints:");
    println!("  GET  /health                    - Health check");
    println!("  GET  /pools/info                - Pool AccountId & liq. pools info");
    println!("  GET  /stats                     - Runtime statistics");
    println!("  POST /orders/submit             - Submit a new order");
    println!("  POST /faucets/mint              - Mint from a faucet");
    println!("  GET  /ws                        - WebSocket connection");
    println!("🌐 Server address: {}", server_url);
    println!("📊 Example: {}/health", server_url);
    println!(
        "🔌 WebSocket: ws://{}/ws\n",
        server_url.replace("http://", "")
    );

    if let Err(e) = axum::serve(listener, app).await {
        error!("Critical error on server: {e}. Exiting with status 1.");
        std::process::exit(1);
    };
}
