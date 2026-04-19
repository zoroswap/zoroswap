use std::{env, fs};

use anyhow::{Result, anyhow};
use clap::Parser;
use dotenv::dotenv;
use miden_client::rpc::Endpoint;
use serde::Deserialize;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zoro_miden::client::MidenClient;

#[derive(Deserialize, Debug)]
struct FaucetConfig {
    symbol: String,
    max_supply: u64,
    decimals: u8,
}
#[derive(Deserialize, Debug)]
struct FaucetsConfig {
    pub faucets: Vec<FaucetConfig>,
}

#[derive(Parser, Debug)]
#[command(name = "spawn_faucets")]
#[command(about = "Spawn faucets for Zoro DEX", long_about = None)]
struct Args {
    /// Path to the faucets config file
    #[arg(short, long, default_value = "./faucets.toml")]
    faucets_config: String,

    /// Path to the keystore directory
    #[arg(short, long, default_value = "./keystore")]
    keystore_path: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .init();

    dotenv().ok();

    let contents = fs::read_to_string(&args.faucets_config)
        .map_err(|e| anyhow!("Error opening {}: {e}", args.faucets_config))?;
    let miden_endpoint =
        env::var("MIDEN_NODE_ENDPOINT").expect("Missing MIDEN_NODE_ENDPOINT in .env file.");
    let parsed: FaucetsConfig = toml::from_str(&contents)?;
    let faucets = parsed.faucets;
    let miden_endpoint = match miden_endpoint.as_str() {
        "testnet" => Endpoint::testnet(),
        "devnet" => Endpoint::devnet(),
        _ => Endpoint::localhost(),
    };

    let mut miden_client =
        MidenClient::new(miden_endpoint.clone(), &args.keystore_path, "stores/").await?;

    miden_client.sync_state().await?;
    // Generate key pair
    for faucet in faucets {
        info!(symbol =? faucet.symbol, decimals=faucet.decimals, max_supply=faucet.max_supply, "\nDeploying a new fungible faucet.");
        miden_client
            .deploy_new_faucet(
                &args.keystore_path,
                &faucet.symbol,
                faucet.decimals,
                faucet.max_supply,
            )
            .await?;
    }

    info!("All faucets deployed successfully.");

    Ok(())
}
