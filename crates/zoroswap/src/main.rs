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

use amm_state::AmmState;
use clap::Parser;
use common::{ZoroStorageSettings, instantiate_client};
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
        .with_env_filter("info,zoro=debug,miden-client=debug")
        .init();
    dotenv().ok();
    let runtime = tokio::runtime::Runtime::new()
        .unwrap_or_else(|err| panic!("Failed to create tokio runtime: {err:?}"));
    match runtime.block_on(async {
        info!("[INIT] Parsing config");
        info!("Deleting old sqlite3 store");
        delete_client_store(&args.store_path).await;
        let config = Config::from_config_file(
            &args.config,
            &args.masm_path,
            &args.keystore_path,
            &args.store_path,
        ).map_err(|e| e.to_string())?;
        let mut init_client = instantiate_client(&config, ZoroStorageSettings::ammstate_storage(config.store_path.to_string()))
            .await
            .unwrap_or_else(|err| panic!("Failed to instantiate init client: {err:?}"));
        info!(
            "[INFO] Pool information\n\n\tpool_id: {}\n\tpool0 faucet_id: {}\n\tpool1 faucet_id: {}\n",
            config.pool_account_id.to_bech32(config.network_id.clone()),
            config.liquidity_pools[0].faucet_id.to_bech32(config.network_id.clone()),
            config.liquidity_pools[1].faucet_id.to_bech32(config.network_id.clone()),
        );
        let amm_state = Arc::new(AmmState::new(config).await);
        info!("[INIT] Initializing faucet metadata");
        amm_state
            .init_faucet_metadata()
            .await
            .map_err(|e| e.to_string())?;
        info!("[INIT] Initializing liquidity pool states");
        amm_state
            .init_liquidity_pool_states(&mut init_client)
            .await
            .map_err(|e| e.to_string())?;
        let trading_engine = TradingEngine::new(&args.store_path, amm_state.clone());
        let oracle_client = OracleSSEClient::new(amm_state.clone());
        info!("[INIT] Initializing oracle prices");
        oracle_client
            .init_prices_in_state()
            .await
            .map_err(|e| e.to_string())?;
        let (guarded_faucet, faucet_tx) = GuardedFaucet::new(amm_state.config().clone());
        let notes_listener = NotesListener::new(amm_state.clone());
        Ok::<
            (
                OracleSSEClient,
                TradingEngine,
                Arc<AmmState>,
                GuardedFaucet,
                Sender<FaucetMintInstruction>,
                NotesListener,
            ),
            String,
        >((
            oracle_client,
            trading_engine,
            amm_state,
            guarded_faucet,
            faucet_tx,
            notes_listener,
        ))
    }) {
        Ok((
            oracle_client,
            mut trading_engine,
            amm_state,
            mut guarded_faucet,
            faucet_tx,
            mut notes_listener,
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
                        info!("[RUN] Guarded faucet");
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
                        info!("[RUN] Public Notes Listener");
                        notes_listener.start().await
                    });
                });
                run_main_tokio((oracle_client, amm_state, faucet_tx));
            });
        }
        Err(e) => {
            println!("[INIT] Critical error: {e:?}");
        }
    };
}

#[tokio::main]
async fn run_main_tokio(
    (mut oracle_client, original_amm_state, faucet_tx): (
        OracleSSEClient,
        Arc<AmmState>,
        Sender<FaucetMintInstruction>,
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
    info!("[RUN] Starting ZORO server");
    let app = create_router(AppState {
        amm_state: original_amm_state.clone(),
        faucet_tx,
    });
    let listener = tokio::net::TcpListener::bind(server_url)
        .await
        .unwrap_or_else(|err| panic!("Failed to bind TCP listener to {}: {err:?}", server_url));
    info!("Server listening on {}", server_url);
    println!("\n🚀 Zoro server is running!");
    println!("📡 Available endpoints:");
    println!("  GET  /health                    - Health check");
    println!("  POST /orders/submit             - Submit a new order");
    println!("🌐 Server address: {}", server_url);
    println!("📊 Example: {}/health\n", server_url);
    if let Err(e) = axum::serve(listener, app).await {
        error!("Critical error on server: {e}. Exiting with status 1.");
        std::process::exit(1);
    };
}
