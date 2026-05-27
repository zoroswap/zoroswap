use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    asset::FungibleAsset,
    note::{NoteTag, NoteType},
};
use std::collections::HashMap;
use tracing_subscriber::EnvFilter;
use zoro_miden::{
    account::MidenAccount,
    client::MidenClient,
    note::{NoteInstructions, NoteKind, TrustedNote},
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
    #[arg(short, long, default_value = "./stores")]
    store_dir: String,
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
    let config = Config::from_config_file(&args.config)?;
    let endpoint = config.miden_endpoint;
    let mut miden_client =
        MidenClient::new(endpoint.clone(), &args.keystore_path, &args.store_dir).await?;

    miden_client.sync_state().await?;
    println!("\n[STEP 1] Create zoro_pool account");

    let mut zoro_pool = ZoroPool::new_deployment(
        config.liquidity_pools.clone(),
        endpoint.clone(),
        "keystore",
        "stores",
    )
    .await?;
    let acc = zoro_pool.miden_account_mut().account().await?;
    miden_client.client_mut().add_account(&acc, true).await?;

    println!("\n[STEP 2] Mint tokens from faucet to lp account");
    let amount = 1000000000;
    let lp_account = MidenAccount::deploy_new(&mut miden_client).await?;
    for pool in config.liquidity_pools.iter() {
        miden_client.import_account(&pool.faucet_id).await?;
        let amount = amount * 10_u64.pow(pool.decimals as u32);
        miden_client
            .mint_asset(pool.faucet_id, *lp_account.id(), amount)
            .await?;
        println!("Minted note of {} tokens for liq pool.", amount);
        miden_client.sync_state().await?;
    }

    println!("\n[STEP 3] LP Account makes DEPOSIT notes for each liq pool");
    let mut notes = Vec::new();
    for pool in config.liquidity_pools.iter() {
        let amount = amount * 10_u64.pow(pool.decimals as u32);
        println!("liq pool: {:?}", pool.name);
        let max_slippage = 0.5; // 0.5 %
        let min_lp_amount_out = ((amount as f64) * (1.0 - max_slippage)) as u64;
        let deposit_note = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(pool.faucet_id, amount)?],
                amount_input: min_lp_amount_out,
                beneficiary: *lp_account.id(),
                note_type: NoteType::Private,
                deadline: (Utc::now().timestamp_millis() + 120_000) as u64,
                p2id_tag: lp_account.tag(),
                pool_tag: NoteTag::with_account_target(*zoro_pool.miden_account().id()),
                asset_input: None,
                note_kind: NoteKind::Deposit,
            },
            miden_client.client_mut().code_builder(),
        )?;
        miden_client
            .send_note(
                lp_account.id(),
                zoro_pool.miden_account().id(),
                deposit_note.clone(),
            )
            .await?;
        notes.push(deposit_note);
    }
    println!("\n[STEP 4] Execute DEPOSIT notes on zoro pool");
    zoro_pool.execute_notes(notes, HashMap::default()).await?;
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
