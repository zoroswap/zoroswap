use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use miden_client::{keystore::FilesystemKeyStore, note::NoteType};
use std::collections::HashMap;
use zoro_miden::{
    account::MidenAccount,
    client::MidenClient,
    note::{DepositInstructions, NoteInstructions, TrustedNote},
    pool::ZoroPool,
};
use zoroswap::Config;

#[derive(Parser, Debug)]
#[command(name = "spawn_pools")]
#[command(about = "Spawn liquidity pools for Zoro DEX", long_about = None)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter("info,zoro=debug")
        .init();

    dotenv().ok();

    // Initialize client
    let config = Config::from_config_file(
        &args.config,
        &args.masm_path,
        &args.keystore_path,
        &args.store_path,
    )?;
    let endpoint = config.miden_endpoint;
    let keystore: FilesystemKeyStore = FilesystemKeyStore::new(config.keystore_path.into())?;
    let mut miden_client = MidenClient::new(
        endpoint.clone(),
        &args.keystore_path,
        &args.store_path,
        None,
    )
    .await?;

    miden_client.sync_state().await?;
    println!("\n[STEP 1] Create zoro_pool account");

    let mut zoro_pool = ZoroPool::new_deployment(
        config.masm_path,
        config.liquidity_pools.clone(),
        endpoint.clone(),
        config.keystore_path,
        config.store_path,
    )
    .await?;

    println!("\n[STEP 2] Mint tokens from our faucet to zoro_pool account");
    let amount = 100000000;
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        println!("Importing the faucet account to client");
        miden_client.import_account(&pool.faucet_id).await?;
        miden_client
            .mint_asset(pool.faucet_id, config.pool_account_id, amount)
            .await?;
        println!("Minted note of {} tokens for liq pool.", amount);
        miden_client.sync_state().await?;
    }
    miden_client
        .consume_notes(zoro_pool.miden_account().id(), config.liquidity_pools.len())
        .await?;

    println!("\n[STEP 3] Make DEPOSIT notes for each liq pool");
    let lp_account = MidenAccount::deploy_new(&mut miden_client, keystore.clone()).await?;
    let mut notes = Vec::new();
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        let amount_in: u64 = amount * 10u64.pow(pool.decimals as u32);
        let max_slippage = 0.005; // 0.5 %
        let min_lp_amount_out = (amount_in as f64) * (1.0 - max_slippage);
        let min_lp_amount_out = min_lp_amount_out as u64;
        let deposit_note = TrustedNote::new(NoteInstructions::Deposit(DepositInstructions {
            asset_in: pool.faucet_id,
            amount_in,
            min_lp_amount_out,
            creator: *lp_account.id(),
            note_type: NoteType::Private,
            deadline: (Utc::now().timestamp_millis() + 120_000) as u64,
            p2id_tag: 0,
        }))?;
        notes.push(deposit_note);
    }

    zoro_pool.execute_notes(notes, HashMap::default()).await?;
    zoro_pool
        .miden_account_mut()
        .refetch_account()
        .await
        .expect("Failed refreshing miden account");
    zoro_pool.print_pool_states();

    println!(
        "\n------\n New pool created: {:?}\n-----\n",
        zoro_pool
            .miden_account()
            .id()
            .to_bech32(endpoint.to_network_id())
    );

    Ok(())
}
