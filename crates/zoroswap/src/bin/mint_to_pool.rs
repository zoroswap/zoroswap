use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    account::AccountId,
    asset::FungibleAsset,
    note::{NoteTag, NoteType},
    transaction::TransactionRequestBuilder,
};
use tracing_subscriber::EnvFilter;
use zoro_miden::{
    account::MidenAccount,
    client::MidenClient,
    note::{DepositInstructions, NoteInstructions, TrustedNote},
};
use zoroswap::Config;

#[derive(Parser, Debug)]
#[command(name = "mint_to_pool")]
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
    let lp_account = MidenAccount::deploy_new(&mut miden_client, config.keystore_path).await?;

    miden_client.import_account(&faucet_id.1).await?;
    miden_client.import_account(lp_account.id()).await?;
    miden_client.import_account(&config.pool_account_id).await?;

    let fungible_asset = FungibleAsset::new(faucet_id.1, args.amount)?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        *lp_account.id(),
        NoteType::Public,
        miden_client.client_mut().rng(),
    )?;

    // Create transaction and submit it to create P2ID notes for Alice's account
    let tx_id = miden_client
        .client_mut()
        .submit_new_transaction(faucet_id.1, transaction_request.clone())
        .await?;
    miden_client.sync_state().await?;

    println!(
        "Mint transaction submitted successfully, ID: {:?}",
        tx_id.to_hex()
    );

    let note = transaction_request.input_notes();
    let consume_tx_request = TransactionRequestBuilder::new().build_consume_notes(note.to_vec())?;

    // Create transaction and submit it to consume notes
    let consume_tx_id = miden_client
        .client_mut()
        .submit_new_transaction(*lp_account.id(), consume_tx_request)
        .await?;

    println!(
        "Consume transaction submitted successfully, ID: {:?}",
        consume_tx_id.to_hex()
    );

    let mut notes = Vec::new();
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        let max_slippage = 0.5; // 0.5 %
        let min_lp_amount_out = ((args.amount as f64) * (1.0 - max_slippage)) as u64;
        let deposit_note = TrustedNote::new(NoteInstructions::Deposit(DepositInstructions {
            asset_in: pool.faucet_id,
            amount_in: args.amount,
            min_lp_amount_out,
            creator: *lp_account.id(),
            note_type: NoteType::Private,
            deadline: (Utc::now().timestamp_millis() + 120_000) as u64,
            p2id_tag: lp_account.tag(),
            pool_tag: NoteTag::with_account_target(config.pool_account_id),
        }))?;
        miden_client
            .send_note(
                lp_account.id(),
                &config.pool_account_id,
                deposit_note.clone(),
            )
            .await?;
        notes.push(deposit_note);
    }

    miden_client.sync_state().await?;
    Ok(())
}

// TODO: add tests
