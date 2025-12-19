use alloy::primitives::U256;
use anyhow::{Result, anyhow};
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use miden_assembly::LinkLibraryKind;
use miden_client::{
    Felt, Word,
    account::{AccountBuilder, AccountStorageMode, AccountType, StorageMap, StorageSlot},
    asset::FungibleAsset,
    auth::AuthSecretKey,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{Note, NoteId, NoteTag, NoteType},
    store::NoteFilter,
    transaction::{OutputNote, TransactionRequestBuilder},
};
use miden_lib::{
    account::{auth::AuthRpoFalcon512, wallets::BasicWallet},
    transaction::TransactionKernel,
};
use miden_objects::{account::AccountComponent, assembly::Assembler};
use rand::RngCore;
use std::{collections::HashSet, fs, path::Path, time::Duration};
use tracing::info;
use zoro_miden_client::{
    MidenClient, create_basic_account, create_library, instantiate_simple_client,
};
use zoroswap::{
    Config, create_deposit_note, fetch_pool_state_from_chain, fetch_vault_for_account_from_chain,
    print_transaction_info,
};

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
    let word: Word = ([
        Felt::new(13603942263569969660),
        Felt::new(13080156875089878972),
        Felt::new(7935997992824405568),
        Felt::new(3892474124107991533),
    ])
    .into();
    let word_reversed: Word = ([
        Felt::new(3892474124107991533),
        Felt::new(7935997992824405568),
        Felt::new(13080156875089878972),
        Felt::new(13603942263569969660),
    ])
    .into();
    println!("+++++Word: {:?} {:?}", word, word.to_hex());
    println!(
        "+++++Word reversed: {:?} {:?}",
        word_reversed,
        word_reversed.to_hex()
    );
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
    let pool_code = fs::read_to_string(pool_reader_path)
        .unwrap_or_else(|err| panic!("unable to read from {pool_reader_path:?}: {err}"));

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

    let mut assets_mapping = StorageMap::new();
    let asset0_index = [Felt::new(0), Felt::new(0), Felt::new(0), Felt::new(0)];
    let asset0_id = [
        // pool0.faucet_id.prefix().as_felt(),
        // pool0.faucet_id.suffix(),
        Felt::new(0),
        Felt::new(0),
        pool0.faucet_id.suffix(),
        pool0.faucet_id.prefix().as_felt(),
    ];
    let asset1_index = [Felt::new(1), Felt::new(0), Felt::new(0), Felt::new(0)];
    let asset1_id = [
        // pool1.faucet_id.prefix().as_felt(),
        // pool1.faucet_id.suffix(),
        Felt::new(0),
        Felt::new(0),
        pool1.faucet_id.suffix(),
        pool1.faucet_id.prefix().as_felt(),
    ];
    assets_mapping
        .insert(asset0_index.into(), asset0_id.into())
        .unwrap_or_else(|err| panic!("Failed to insert asset0 into mapping: {err:?}"));
    assets_mapping
        .insert(asset1_index.into(), asset1_id.into())
        .unwrap_or_else(|err| panic!("Failed to insert asset1 into mapping: {err:?}"));
    let assets_mapping = StorageSlot::Map(assets_mapping);
    let pool_states_mapping = StorageSlot::Map(StorageMap::new());
    let user_deposits_mapping = StorageSlot::Map(StorageMap::new());
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
            assets_mapping,
            pool_states_mapping,
            user_deposits_mapping,
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

    println!("+++++Pool contract procedures");
    pool_contract
        .code()
        .procedures()
        .into_iter()
        .for_each(|proc| {
            println!("+++++proc root: {:?} ", proc.mast_root().to_hex());
        });
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

    let (lp_account, _) = create_basic_account(&mut client, keystore.clone()).await?;

    let amount = 1000000;
    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        println!("Importing the faucet account to client");
        client.import_account_by_id(pool.faucet_id).await?;
        let amount_raw: u64 = amount * 10u64.pow(pool.decimals as u32);
        let fungible_asset = FungibleAsset::new(pool.faucet_id, amount_raw)?;
        let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            lp_account.id(),
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

        let consumable_notes = client.get_consumable_notes(Some(lp_account.id())).await?;
        let list_of_note_ids: Vec<_> = consumable_notes.iter().map(|(note, _)| note.id()).collect();

        if list_of_note_ids.len() == config.liquidity_pools.len() {
            println!("Found consumable notes for lp account. Consuming them now...");
            let transaction_request =
                TransactionRequestBuilder::new().build_consume_notes(list_of_note_ids)?;
            let _tx_id = client
                .submit_new_transaction(lp_account.id(), transaction_request)
                .await?;

            println!("All of liq pool's P2ID notes consumed successfully.");
            break;
        } else {
            println!(
                "Currently, liq pool has {} consumable P2ID notes. Waiting...",
                list_of_note_ids.len()
            );
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    // Re-sync so minted notes become visible
    client.sync_state().await?;

    let pool_contract_tag = NoteTag::from_account_id(pool_contract.id());

    // Retrieve updated contract data to see the state
    let account = client
        .get_account(pool_contract.id())
        .await?
        .ok_or(anyhow!("Account {:?} not found.", pool_contract.id()))?;
    println!(
        "pool contract storage: {:?}",
        account.account().storage().get_item(0)
    );
    println!("\n[STEP 3] Make DEPOSIT notes for each liq pool");

    for pool in config.liquidity_pools.iter() {
        println!("liq pool: {:?}", pool.name);
        // println!("Importing the lp account to client");
        //client.import_account_by_id(lp_account.id()).await?;
        let amount_in: u64 = amount * 10u64.pow(pool.decimals as u32);
        let max_slippage = 0.005; // 0.5 %
        let min_lp_amount_out = (amount_in as f64) * (1.0 - max_slippage);
        let min_lp_amount_out = min_lp_amount_out as u64;
        let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
        //let asset_out: FungibleAsset = FungibleAsset::new(pool1.faucet_id, min_amount_out)?;
        // let requested_asset_word: Word = asset_out.into();
        let p2id_tag = NoteTag::from_account_id(lp_account.id());
        let deadline = (Utc::now().timestamp_millis() as u64) + 10000;
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
        let deposit_serial_num = client.rng().draw_word();
        println!(
            "Made an deposit note for {amount_in} {} expecting  at least {min_lp_amount_out} lp amount out.",
            pool0.symbol
        );
        let deposit_note = create_deposit_note(
            inputs,
            vec![asset_in.into()],
            lp_account.id(),
            deposit_serial_num,
            pool_contract_tag,
            NoteType::Public,
        )?;

        let note_req = TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(deposit_note.clone())])
            .build()
            .unwrap();

        println!("tx request built");
        let _tx_id = client
            .submit_new_transaction(lp_account.id(), note_req)
            .await?;
        println!("Minted note of {} tokens for liq pool.", amount_in);
        client.sync_state().await?;
    }

    // Consume DEPOSIT notes by POOL CONTRACT
    let failed_notes = Vec::new();
    loop {
        ////////    !!!!!!!!!!!!!!!!!!!!!!!!!

        // Resync to get the latest data
        match fetch_new_notes_by_tag(&mut client, &pool_contract_tag).await {
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

                    let in_amount: u64 = amount * 10u64.pow(8 as u32);
                    let args: Word = [
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                        Felt::new(in_amount),
                    ]
                    .into();
                    let consume_req = TransactionRequestBuilder::new()
                        .unauthenticated_input_notes(
                            valid_notes
                                .iter()
                                .map(|deposit_note| {
                                    (deposit_note.clone().clone(), Some(args.clone()))
                                })
                                .collect::<Vec<_>>(),
                        )
                        .build()
                        .map_err(|e| {
                            anyhow::anyhow!("Failed to build batch transaction request: {}", e)
                        })?;
                    let _tx_id = client
                        .submit_new_transaction(pool_contract.id(), consume_req)
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

    println!("\n[STEP 3] Set initial states of the two_pools_account");

    // for pool_num in [0, 1] {
    //     let pool_config = config.liquidity_pools[pool_num];
    //     let amount_raw = U256::from(amount * 10u64.pow(pool_config.decimals as u32));
    //     println!("Setting initial state of pool {}", pool_config.name);
    //     let tx_request = create_set_pool_state_tx(
    //         pool_num,
    //         zoro::PoolBalances {
    //             reserve: amount_raw,
    //             reserve_with_slippage: amount_raw,
    //             total_liabilities: amount_raw,
    //         },
    //         config.masm_path,
    //     )?;
    //     // Execute the transaction locally
    //     let tx_id = client
    //         .submit_new_transaction(pool_contract.id(), tx_request)
    //         .await?;
    //     println!("Successfuly set state for pool{pool_num}");
    //     print_transaction_info(&tx_id);
    //     client.sync_state().await?;
    // }

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

// async fn fetch_new_notes_by_tag(
//     client: &mut MidenClient,
//     pool_id_tag: &NoteTag,
// ) -> Result<Vec<Note>> {
//     client.sync_state().await?;
//     let all_notes = client.get_input_notes(NoteFilter::Committed).await?;
//     let notes: Vec<Note> = all_notes
//         .iter()
//         .filter_map(|n| {
//             if let Some(metadata) = n.metadata() {
//                 if metadata.tag().eq(pool_id_tag) {
//                     let note = Note::new(
//                         n.assets().clone(),
//                         *metadata,
//                         n.details().recipient().clone(),
//                     );
//                     Some(note)
//                 } else {
//                     None
//                 }
//             } else {
//                 None
//             }
//         })
//         .collect();
//     Ok(notes)
// }

async fn fetch_new_notes_by_tag(
    client: &mut MidenClient,
    pool_id_tag: &NoteTag,
) -> Result<Vec<Note>> {
    client.sync_state().await?;
    let all_notes = client.get_output_notes(NoteFilter::Committed).await?;
    let notes: Vec<Note> = all_notes
        .iter()
        .filter_map(|n| {
            if n.metadata().tag().eq(pool_id_tag)
                && let Some(recipient) = n.recipient()
            {
                let note = Note::new(n.assets().clone(), *n.metadata(), recipient.clone());
                Some(note)
            } else {
                None
            }
        })
        .collect();
    Ok(notes)
}
