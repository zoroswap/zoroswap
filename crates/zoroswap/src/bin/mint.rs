use anyhow::Result;
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    account::AccountId, asset::FungibleAsset, note::NoteType,
    transaction::TransactionRequestBuilder,
};
use tracing_subscriber::EnvFilter;
use zoro_miden::client::MidenClient;
use zoroswap::Config;

#[derive(Parser, Debug)]
#[command(name = "mint")]
#[command(about = "Mint tokens from a faucet", long_about = None)]
struct Args {
    /// Path to the config file
    #[arg(short, long, default_value = "./config.toml")]
    config: String,

    /// Path to the keystore directory
    #[arg(short, long, default_value = "./keystore")]
    keystore_path: String,

    /// Path to the SQLite store file
    #[arg(short, long, default_value = "./stores")]
    store_dir: String,

    /// target id
    #[arg(short, long, required = true, help = "Target id in bech32")]
    target: String,

    /// faucet id
    #[arg(short, long, required = true, help = "Faucet id in bech32")]
    faucet: String,

    /// faucet id
    #[arg(short, long, required = true, help = "Raw amount to mint")]
    amount: u64,
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

    // Initialize client
    let config = Config::from_config_file(&args.config, &args.keystore_path, &args.store_dir)?;
    let endpoint = config.miden_endpoint;
    let mut miden_client =
        MidenClient::new(endpoint.clone(), &args.keystore_path, &args.store_dir).await?;

    miden_client.sync_state().await?;
    let faucet_id = AccountId::from_bech32(&args.faucet)?;
    let target_id = AccountId::from_bech32(&args.target)?;

    miden_client.import_account(&faucet_id.1).await?;

    let fungible_asset = FungibleAsset::new(faucet_id.1, args.amount)?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        target_id.1,
        NoteType::Public,
        miden_client.client_mut().rng(),
    )?;

    // Create transaction and submit it to create P2ID notes for Alice's account
    let tx_id = miden_client
        .client_mut()
        .submit_new_transaction(faucet_id.1, transaction_request)
        .await?;
    miden_client.sync_state().await?;

    println!(
        "Mint transaction submitted successfully, ID: {:?}",
        tx_id.to_hex()
    );

    Ok(())
}

// TODO: add tests
