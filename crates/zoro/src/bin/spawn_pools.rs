use anyhow::{Result, anyhow};
use clap::Parser;
use dotenv::dotenv;
use miden_client::{
    Felt,
    account::{AccountBuilder, AccountStorageMode, AccountType, StorageSlot},
    asset::FungibleAsset,
    auth::AuthSecretKey,
    keystore::FilesystemKeyStore,
    note::NoteType,
    transaction::{TransactionRequestBuilder, TransactionScript},
};
use miden_lib::{
    account::{auth::AuthRpoFalcon512, wallets::BasicWallet},
    transaction::TransactionKernel,
};
use miden_objects::{account::AccountComponent, assembly::Assembler};
use rand::RngCore;
use std::{fs, path::Path, time::Duration};
use tracing::info;
use zoro::{
    Config, fetch_pool_state_from_chain, fetch_vault_for_account_from_chain, print_transaction_info,
};
use zoro_miden_client::{create_library, instantiate_simple_client};

#[derive(Parser, Debug)]
#[command(name = "spawn_pools")]
#[command(about = "Spawn liquidity pools for Zoro DEX", long_about = None)]
struct Args {
    /// Path to the config file
    #[arg(short, long, default_value = "./config.toml")]
    config: String,

    /// Path to the MASM files directory
    #[arg(short, long, default_value = "./crates/zoro/masm")]
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
    let keystore: FilesystemKeyStore<rand::prelude::StdRng> =
        FilesystemKeyStore::new(config.keystore_path.into())?;
    let mut client = instantiate_simple_client(&args.keystore_path, &endpoint).await?;

    let sync_summary = client.sync_state().await?;
    println!("\nLatest block: {}", sync_summary.block_num);
    println!("\n[STEP 1] Create two_pools_account");

    // Load the MASM file for the counter contract
    let pool_reader_path = format!("{}/accounts/two_asset_pool.masm", config.masm_path);
    let pool_reader_path = Path::new(&pool_reader_path);
    let pool_code = fs::read_to_string(pool_reader_path)?;

    // Prepare assembler (debug mode = true)
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);

    if config.liquidity_pools.len() != 2 {
        panic!("There should be exactly 2 pools defined in config")
    }
    let pool0 = config.liquidity_pools.first().expect("Pool0 from config");
    let pool1 = config.liquidity_pools.last().expect("pool0 from config");
    let key_pair = AuthSecretKey::new_rpo_falcon512_with_rng(client.rng());

    let pool0_asset = StorageSlot::Value(
        [
            Felt::new(0),
            Felt::new(0),
            pool0.faucet_id.suffix(),
            pool0.faucet_id.prefix().as_felt(),
        ]
        .into(),
    );
    let pool0_fees = StorageSlot::Value(
        [
            Felt::new(400), // pool0_swap_fee
            Felt::new(600), // pool0_backstop_fee
            Felt::new(0),   // pool0_protocol_fee
            Felt::new(0),   // 0
        ]
        .into(),
    );
    let pool0_curve = StorageSlot::Value(
        [
            Felt::new(17075887234393789126), // c
            Felt::new(5000000000000000),     // beta
            Felt::new(0),
            Felt::new(0),
        ]
        .into(),
    );
    let pool1_asset = StorageSlot::Value(
        [
            Felt::new(0),
            Felt::new(0),
            pool1.faucet_id.suffix(),
            pool1.faucet_id.prefix().as_felt(),
        ]
        .into(),
    );
    let pool1_fees = StorageSlot::Value(
        [
            Felt::new(200), // pool1_swap_fee
            Felt::new(300), // pool1_backstop_fee
            Felt::new(0),   // pool1_protocol_fee
            Felt::new(0),   // 0
        ]
        .into(),
    );
    let pool1_curve = StorageSlot::Value(
        [
            Felt::new(17075887234393789127), // c
            Felt::new(5000000000000001),     // beta
            Felt::new(0),
            Felt::new(0),
        ]
        .into(),
    );

    info!(
        "Asset0: prefix {:?} suffix {:?} , Asset1: prefix {:?} suffix {:?}",
        pool0.faucet_id.prefix().as_felt(),
        pool0.faucet_id.suffix(),
        pool1.faucet_id.prefix().as_felt(),
        pool1.faucet_id.suffix()
    );

    // Compile the account code into `AccountComponent` with one storage slot
    let pool_component = AccountComponent::compile(
        pool_code.clone(),
        assembler.clone(),
        vec![
            pool0_asset,
            pool1_asset,
            StorageSlot::empty_value(), // pool0 balances, we set them later
            StorageSlot::empty_value(), // pool1 balances, we set them later
            pool0_fees,
            pool1_fees,
            pool0_curve,
            pool1_curve,
        ],
    )?
    .with_supports_all_types();

    // Init seed for the pool contract
    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    // Build the new `Account` with the component
    let pool_contract = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_component(pool_component.clone())
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key().to_commitment()))
        .with_component(BasicWallet)
        .build()?;

    println!(
        "pool contract commitment hash: {:?}",
        pool_contract.commitment().to_hex()
    );
    println!(
        "contract id: {:?}",
        pool_contract.id().to_bech32(endpoint.to_network_id())
    );

    keystore.add_key(&key_pair)?;

    client.add_account(&pool_contract.clone(), false).await?;

    client.sync_state().await?;

    println!("State synced");

    tokio::time::sleep(Duration::from_secs(2)).await;

    println!("\n[STEP 2] Mint tokens from our faucet to two_pools_account");

    let account = client
        .get_account(pool_contract.id())
        .await?
        .ok_or(anyhow!("Account {:?} not found.", pool_contract.id()))?;

    let amount = 1000000;
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        println!("Importing the faucet account to client");
        client.import_account_by_id(pool.faucet_id).await?;
        let amount_raw: u64 = amount * 10u64.pow(pool.decimals as u32);
        let fungible_asset = FungibleAsset::new(pool.faucet_id, amount_raw)?;
        let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            account.account().id(),
            NoteType::Public,
            client.rng(),
        )?;
        println!("tx request built");
        let _tx_id = client
            .submit_new_transaction(pool.faucet_id, transaction_request)
            .await?;
        println!("Minted note of {} tokens for liq pool.", amount_raw);
        client.sync_state().await?;
    }

    loop {
        // Resync to get the latest data
        client.sync_state().await?;

        let consumable_notes = client
            .get_consumable_notes(Some(account.account().id()))
            .await?;
        let list_of_note_ids: Vec<_> = consumable_notes.iter().map(|(note, _)| note.id()).collect();

        if list_of_note_ids.len() == config.liquidity_pools.len() {
            println!("Found consumable notes for liq pool. Consuming them now...");
            let transaction_request =
                TransactionRequestBuilder::new().build_consume_notes(list_of_note_ids)?;
            let _tx_id = client
                .submit_new_transaction(account.account().id(), transaction_request)
                .await?;

            println!("All of liq pool's notes consumed successfully.");
            break;
        } else {
            println!(
                "Currently, liq pool has {} consumable notes. Waiting...",
                list_of_note_ids.len()
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // Re-sync so minted notes become visible
    client.sync_state().await?;

    // Retrieve updated contract data to see the state
    let account = client
        .get_account(pool_contract.id())
        .await?
        .ok_or(anyhow!("Account {:?} not found.", pool_contract.id()))?;
    println!(
        "pool contract storage: {:?}",
        account.account().storage().get_item(0)
    );

    println!("\n[STEP 3] Set initial states of the two_pools_account");

    for pool_num in [0, 1] {
        let pool_config = config.liquidity_pools[pool_num];
        let amount_raw = amount * 10u64.pow(pool_config.decimals as u32);
        println!("Setting initial state of pool {}", pool_config.name);
        let script_path = format!("{}/scripts/set_pool{pool_num}_state.masm", config.masm_path);
        let script_path = Path::new(&script_path);
        let script_code = fs::read_to_string(script_path)?;

        let account_component_lib = create_library(
            assembler.clone(),
            "external_contract::two_pools_contract",
            &pool_code,
        )
        .unwrap_or_else(|err| panic!("Failed to create library: {err:?}"));

        let program = assembler
            .clone()
            .with_dynamic_library(&account_component_lib)
            .unwrap_or_else(|err| panic!("Failed to add dynamic library: {err:?}"))
            .assemble_program(script_code)
            .unwrap_or_else(|err| panic!("Failed to assemble program: {err:?}"));
        let tx_script = TransactionScript::new(program);

        // Build a transaction request with the custom script
        let tx_request = TransactionRequestBuilder::new()
            .custom_script(tx_script)
            .script_arg(
                [
                    Felt::new(amount_raw),
                    Felt::new(amount_raw),
                    Felt::new(amount_raw),
                    Felt::new(0),
                ]
                .into(),
            )
            .build()?;

        // Execute the transaction locally
        let tx_id = client
            .submit_new_transaction(pool_contract.id(), tx_request)
            .await?;

        println!("Successfuly set state for pool{pool_num}");
        print_transaction_info(&tx_id);

        client.sync_state().await?;
    }

    let (balances_pool_0, settings_pool_0) =
        fetch_pool_state_from_chain(&mut client, pool_contract.id(), 0).await?;
    let vault = fetch_vault_for_account_from_chain(&mut client, pool_contract.id()).await?;
    let (balances_pool_1, settings_pool_1) =
        fetch_pool_state_from_chain(&mut client, pool_contract.id(), 1).await?;
    println!(
        "pool account 0: {:?}\n{:?}",
        balances_pool_0, settings_pool_0
    );
    println!(
        "pool account 1: {:?}\n{:?}",
        balances_pool_1, settings_pool_1
    );
    println!("pool vault: {vault:?}");
    println!(
        "\n------\n New pool created: {:?}\n-----\n",
        pool_contract.id().to_bech32(endpoint.to_network_id())
    );

    Ok(())
}
