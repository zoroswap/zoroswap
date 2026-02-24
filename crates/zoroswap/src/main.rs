mod amm_state;
mod config;
mod faucet;
mod notes_listener;
mod oracle_sse;
mod order;
mod server;
mod sqlite;
mod trading_engine;
mod websocket;

use amm_state::AmmState;
use clap::Parser;
use config::Config;
use dotenv::dotenv;
use faucet::{FaucetMintInstruction, GuardedFaucet};
use notes_listener::NotesListener;
use oracle_sse::OracleSSEClient;
use server::{AppState, create_router};
use sqlite::enable_wal_mode;
use std::{sync::Arc, thread};
use tokio::{runtime::Builder, sync::mpsc::Sender};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use trading_engine::TradingEngine;
use websocket::{ConnectionManager, EventBroadcaster};
use zoro_miden::client::{MidenClient, delete_client_store};

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
    let filter_layer = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,server=info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
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
            warn!("Failed to enable WAL mode: {e}");
        }

        let config = Config::from_config_file(
            &args.config,
            &args.masm_path,
            &args.keystore_path,
            &args.store_path,
        )
        .map_err(|e| e.to_string())?;
        let mut init_client = MidenClient::new(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_path,
            None,
        )
        .await
        .unwrap_or_else(|err| panic!("Failed to instantiate init client: {err:?}"));
        info!(
            "[INFO] Pool information\n\n\tpool_id: {}",
            config.pool_account_id.to_bech32(config.network_id.clone()),
        );
        for liq_pool in &config.liquidity_pools {
            info!(
                "[INFO] Liquidity pool: \t{}",
                liq_pool.faucet_id.to_bech32(config.network_id.clone()),
            );
        }
        // Initialize WebSocket infrastructure
        info!("[INIT] Initializing WebSocket infrastructure");
        let event_broadcaster = Arc::new(EventBroadcaster::new());
        let connection_manager = Arc::new(ConnectionManager::with_broadcaster(
            event_broadcaster.clone(),
        ));

        // Create and initialize AMM state
        let amm_state = Arc::new(AmmState::new(config, event_broadcaster.clone()).await);

        info!("[INIT] Initializing liquidity pool states");
        // Initialize components with broadcaster
        let trading_engine = TradingEngine::new(amm_state.clone(), event_broadcaster.clone())
            .await
            .unwrap();
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
                    let rt = Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap_or_else(|err| {
                            panic!("Failed building runtime for trading engine: {err:?}")
                        });
                    rt.block_on(async {
                        info!("[RUN] Starting trading engine");
                        if let Err(e) = trading_engine.start().await {
                            error!(
                                "Critical error on trading engine: {e:?}. Exiting with status 1."
                            );
                            std::process::exit(1);
                        }
                    });
                });

                s.spawn(move || {
                    let rt = Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap_or_else(|err| {
                            panic!("Failed building runtime for guarded faucet: {err:?}")
                        });
                    rt.block_on(async {
                        info!("[RUN] Starting guarded faucet");
                        if let Err(e) = guarded_faucet.start().await {
                            error!("Critical error on faucet: {e:?}. Exiting with status 1.");
                            std::process::exit(1);
                        }
                    });
                });

                s.spawn(move || {
                    let rt = Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap_or_else(|err| {
                            panic!("Failed building runtime for notes listener: {err:?}")
                        });
                    rt.block_on(async {
                        info!("[RUN] Starting notes listener");
                        if let Err(e) = notes_listener.start().await {
                            error!(
                                "Critical error on notes listener: {e:?}. Exiting with status 1."
                            );
                            std::process::exit(1);
                        }
                    });
                });

                run_main_tokio((
                    oracle_client,
                    amm_state,
                    faucet_tx,
                    connection_manager,
                    event_broadcaster,
                ));
            });
        }
        Err(e) => {
            println!("[INIT] Critical error: {e:?}");
        }
    };
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

    // Start event forwarding from EventBroadcaster to WebSocket clients
    info!("[RUN] Starting WebSocket event forwarding");
    connection_manager.clone().start_event_forwarding();

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
    println!("  GET  /pools/balance             - Pool balances");
    println!("  GET  /pools/settings            - Pool fee settings");
    println!("  GET  /stats                     - Runtime statistics");
    println!("  POST /orders/submit             - Submit a new swap order");
    println!("  POST /deposit/submit            - Submit a new deposit");
    println!("  POST /withdraw/submit           - Submit a new withdrawal");
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
