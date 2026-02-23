use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    Felt, Word,
    account::AccountId,
    asset::FungibleAsset,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{Note, NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::time::Duration;
use zoro_miden::{account::MidenAccount, client::MidenClient, note::TrustedNote, pool::ZoroPool};
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
        &mut miden_client,
        endpoint.clone(),
        keystore.clone(),
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
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        let amount_in: u64 = amount * 10u64.pow(pool.decimals as u32);
        let max_slippage = 0.005; // 0.5 %
        let min_lp_amount_out = (amount_in as f64) * (1.0 - max_slippage);
        let min_lp_amount_out = min_lp_amount_out as u64;
        let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
        let p2id_tag = NoteTag::with_account_target(*lp_account.id());
        let deadline = (Utc::now().timestamp_millis() as u64) + 120000;
        let inputs = vec![
            Felt::new(0),
            Felt::new(min_lp_amount_out), // min_lp_amount_out
            Felt::new(deadline),          // deadline
            p2id_tag.into(),              // p2id tag
            Felt::new(0),
            Felt::new(0),
            lp_account.id().suffix(),
            lp_account.id().prefix().into(),
        ];
        let deposit_serial_num = miden_client.client_mut().rng().draw_word();
        println!(
            "Made an deposit note for {amount_in} {} expecting  at least {min_lp_amount_out} lp amount out.",
            pool.symbol
        );
        let deposit_note = TrustedNote::new_deposit(
            inputs,
            vec![asset_in.into()],
            *lp_account.id(),
            deposit_serial_num,
            NoteTag::with_account_target(*zoro_pool.miden_account().id()),
            NoteType::Public,
        )?;

        let note_req = TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(deposit_note.note().clone())])
            .build()
            .unwrap();

        println!("tx request built");
        let _tx_id = miden_client
            .client_mut()
            .submit_new_transaction(*lp_account.id(), note_req)
            .await?;
        println!("Minted note of {} tokens for liq pool.", amount_in);
        miden_client.sync_state().await?;
    }

    // Consume DEPOSIT notes by POOL CONTRACT
    let failed_notes = Vec::new();
    loop {
        ////////    !!!!!!!!!!!!!!!!!!!!!!!!!

        // Resync to get the latest data
        match miden_client
            .fetch_new_notes_by_tag(&zoro_pool.miden_account().tag())
            .await
        {
            Ok(notes) => {
                let valid_notes: Vec<&Note> = notes
                    .iter()
                    .filter(|n| !failed_notes.contains(&n.id()))
                    .collect();

                let number_of_notes = valid_notes.len();
                if number_of_notes == config.liquidity_pools.len() {
                    println!(
                        "Found consumable DEPOSIT notes for pool contract account. Consuming them now..."
                    );

                    let in_amount: u64 = amount * 10u64.pow(8);
                    let args: Word = [
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                    ]
                    .into();
                    let consume_req = TransactionRequestBuilder::new()
                        .input_notes(
                            valid_notes
                                .iter()
                                .map(|deposit_note| ((*deposit_note).clone(), Some(args)))
                                .collect::<Vec<_>>(),
                        )
                        .build()
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to build batch transaction request: {}", e)
                        })?;
                    let _tx_id = miden_client
                        .client_mut()
                        .submit_new_transaction(*zoro_pool.miden_account().id(), consume_req)
                        .await?;

                    println!("All of liq pool's DEPOSIT notes consumed successfully.");
                    break;
                } else {
                    println!(
                        "Currently, pool contract has {} consumable DEPOSIT notes. Waiting...",
                        number_of_notes
                    );
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
            Err(e) => {
                println!("Error in listening for zoro swap notes: {}", e);
            }
        };
    }

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
