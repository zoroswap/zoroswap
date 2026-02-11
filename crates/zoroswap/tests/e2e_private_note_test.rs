use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use dotenv::dotenv;
use miden_client::crypto::FeltRng;
use miden_client::store::TransactionFilter;
use miden_client::{
    Felt, Word,
    account::Account,
    asset::FungibleAsset,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::{str::FromStr, time::Duration};
use url::Url;
use zoro_miden_client::{
    MidenClient, create_basic_account, wait_for_consumable_notes, wait_for_note,
};
use zoroswap::{
    Config, config::LiquidityPoolConfig, create_deposit_note, create_withdraw_note,
    create_zoroswap_note, fetch_lp_total_supply_from_chain, fetch_pool_state_from_chain,
    fetch_vault_for_account_from_chain, get_oracle_prices, instantiate_client, print_note_info,
    print_transaction_info, serialize_note,
};

struct Accounts {
    pub zoro: Account,
    pub user: Account,
}

async fn set_up_with_store(store_path: &str) -> Result<(
    Config,
    MidenClient,
    FilesystemKeyStore,
    Accounts,
    Vec<LiquidityPoolConfig>,
)> {
    dotenv().ok();

    let config = Config::from_config_file(
        "../../config.toml",
        "../../masm",
        "../../keystore",
        store_path,
    )
    .unwrap();

    assert!(
        config.liquidity_pools.len() > 1,
        "Less than 2 liquidity pools configured"
    );
    let mut client = instantiate_client(config.clone(), store_path)
        .await
        .unwrap();
    let endpoint = config.miden_endpoint.clone();
    let keystore = FilesystemKeyStore::new(config.keystore_path.into()).unwrap();
    let sync_summary = client.sync_state().await.unwrap();
    println!("\nLatest block: {}", sync_summary.block_num);
    let (account, _) = create_basic_account(&mut client, keystore.clone())
        .await
        .unwrap();
    println!(
        "Created Account ⇒ ID: {:?}",
        account.id().to_bech32(endpoint.to_network_id())
    );
    client.sync_state().await.unwrap();

    let accounts = Accounts {
        zoro: account.clone(),
        user: account.clone(),
    };

    println!("\n\t[STEP 2] Fund user wallet\n");
    let pool0 = config
        .liquidity_pools
        .first()
        .expect("No liquidity pools found in config.");

    let pool1 = config
        .liquidity_pools
        .last()
        .expect("No liquidity pools found in config.");
    let pools = vec![*pool0, *pool1];

    fund_user_wallet(&mut client, &accounts.user, pool0, 0).await?;
    Ok((config, client, keystore, accounts, pools))
}

async fn fund_user_wallet(
    client: &mut MidenClient,
    account: &Account,
    pool: &LiquidityPoolConfig,
    amount: u64,
) -> Result<()> {
    let amount: u64 = if amount > 0 {
        amount
    } else {
        5 * 10u64.pow(pool.decimals as u32 - 2)
    }; // 0.05
    let fungible_asset = FungibleAsset::new(pool.faucet_id, amount)?;
    client.import_account_by_id(pool.faucet_id).await?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        account.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let tx_id = client
        .submit_new_transaction(pool.faucet_id, transaction_request)
        .await?;
    println!("Minted {amount} {} for the user.", pool.symbol);
    client.sync_state().await?;

    let transaction = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .with_context(|| "failed to find transaction {tx_id:?} after submission")
        .unwrap();
    let minted_note = match transaction.details.output_notes.get_note(0) {
        OutputNote::Full(n) => n.clone(),
        _ => panic!("Expected OutputNote::Full, got something else"),
    };

    wait_for_note(client, account, &minted_note).await?;

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(minted_note, None)])
        .build()
        .unwrap();

    let _tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet");

    Ok(())
}

#[tokio::test]
async fn e2e_private_deposit_withdraw_test() -> Result<()> {
    // Use a fresh store to avoid leftover data from prior test runs.
    let store_path = "../../deposit_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);
    let (config, mut client, _keystore, accounts, pools) = set_up_with_store(store_path).await?;
    let account = accounts.user;
    let pool = pools[0];

    let lp_total_supply_before =
        fetch_lp_total_supply_from_chain(&mut client, config.pool_account_id, pool.faucet_id)
            .await?;

    let amount_in = 4;
    println!("\n\t[STEP 1] Create DEPOSIT note\n");
    let amount_in: u64 = amount_in * 10u64.pow(pool.decimals as u32 - 2);
    let max_slippage = 0.005; // 0.5 %
    let min_lp_amount_out = ((amount_in as f64) * (1.0 - max_slippage)) as u64;
    let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 120000;
    let inputs = vec![
        Felt::new(0),
        Felt::new(min_lp_amount_out), // min_lp_amount_out
        Felt::new(deadline),          // deadline
        p2id_tag.into(),              // p2id tag
        Felt::new(0),
        Felt::new(0),
        account.id().suffix(),
        account.id().prefix().into(),
    ];
    let _pool_contract_tag = NoteTag::with_account_target(config.pool_account_id);
    let deposit_serial_num = client.rng().draw_word();
    println!(
        "Made an deposit note for {amount_in} {} expecting  at least {min_lp_amount_out} lp amount out.",
        pool.symbol
    );
    let deposit_note = create_deposit_note(
        inputs,
        vec![asset_in.into()],
        account.id(),
        deposit_serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )?;

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note.clone())])
        .build()
        .unwrap();

    println!("tx request built");
    let _tx_id = client
        .submit_new_transaction(account.id(), note_req)
        .await?;
    println!("Minted note of {} tokens for liq pool.", amount_in);
    client.sync_state().await?;
    let serialized_note = serialize_note(&deposit_note)?;
    send_to_server(
        &format!("http://{}", config.server_url),
        serialized_note,
        "deposit",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(30)).await;
    client.sync_state().await?;

    let lp_total_supply_after =
        fetch_lp_total_supply_from_chain(&mut client, config.pool_account_id, pool.faucet_id)
            .await?;
    println!("lp_total_supply_after: {lp_total_supply_after}");
    println!("lp_total_supply_before: {lp_total_supply_before}");
    println!("min_lp_amount_out: {min_lp_amount_out}");
    assert!(
        lp_total_supply_after >= lp_total_supply_before + min_lp_amount_out,
        "LP total supply didnt increase"
    );

    println!("\n\t[STEP 2] Create WITHDRAW note\n");
    // Recreate the client since the pool account's state changed on-chain.
    drop(client);
    let mut client = instantiate_client(config.clone(), store_path).await?;

    let amount_to_withdraw = 2;
    let amount_to_withdraw: u64 = amount_to_withdraw * 10u64.pow(pool.decimals as u32 - 2);
    let max_slippage = 0.005; // 0.5 %
    let min_asset_amount_out = (amount_to_withdraw as f64) * (1.0 - max_slippage);
    let min_asset_amount_out = min_asset_amount_out as u64;
    let asset_out: FungibleAsset = FungibleAsset::new(pool.faucet_id, min_asset_amount_out)?;
    // let requested_asset_word: Word = asset_out.into();
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 120000;
    let asset_out_word: Word = asset_out.into();
    let inputs = vec![
        asset_out_word[0],
        asset_out_word[1],
        asset_out_word[2],
        asset_out_word[3],
        Felt::new(0),
        Felt::new(amount_to_withdraw), // min_lp_amount_out
        Felt::new(deadline),           // deadline
        p2id_tag.into(),               // p2id tag
        Felt::new(0),
        Felt::new(0),
        account.id().suffix(),
        account.id().prefix().into(),
    ];
    let _pool_contract_tag = NoteTag::with_account_target(config.pool_account_id);
    let withdraw_serial_num = client.rng().draw_word();
    println!("######################################################### deadline: {deadline}");
    println!(
        "Made an deposit note for {amount_in} {} expecting  at least {min_lp_amount_out} lp amount out.",
        pool.symbol
    );
    let withdraw_note = create_withdraw_note(
        inputs,
        vec![],
        account.id(),
        withdraw_serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )?;

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(withdraw_note.clone())])
        .build()
        .unwrap();

    let _tx_id = client
        .submit_new_transaction(account.id(), note_req)
        .await?;

    client.sync_state().await?;
    send_to_server(
        &format!("http://{}", config.server_url),
        serialize_note(&withdraw_note)?,
        "withdraw",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(25)).await;

    let total_lp_supply_after_withdraw: u64 =
        fetch_lp_total_supply_from_chain(&mut client, config.pool_account_id, pool.faucet_id)
            .await?;

    println!(
        "LP total supply before withdraw: {lp_total_supply_after}, after withdraw: {total_lp_supply_after_withdraw}"
    );

    assert!(
        lp_total_supply_after > total_lp_supply_after_withdraw,
        "LP amount after deposit and then withdraw is incorrect"
    );

    Ok(())
}

#[tokio::test]
async fn e2e_private_note() -> Result<()> {
    // Use a fresh store to avoid leftover data from prior test runs.
    let store_path = "../../swap_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);
    let (config, mut client, keystore, accounts, pools) = set_up_with_store(store_path).await?;
    let account = accounts.user;
    let pool0 = pools[0];
    let pool1 = pools[1];

    let (balances_pool_0, _) =
        fetch_pool_state_from_chain(&mut client, config.pool_account_id, pool0.faucet_id).await?;
    let (balances_pool_1, _) =
        fetch_pool_state_from_chain(&mut client, config.pool_account_id, pool1.faucet_id).await?;
    let vault = fetch_vault_for_account_from_chain(&mut client, config.pool_account_id).await?;
    println!("balances for liq pool 0: {balances_pool_0:?}");
    println!("balances for liq pool 1: {balances_pool_1:?}");
    println!("pool vault on-chain: {vault:?}");

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 3] Fetching latest prices from the oracle\n");

    let prices =
        get_oracle_prices(config.oracle_https, vec![pool0.oracle_id, pool1.oracle_id]).await?;
    let pool0_price: u64 = prices
        .iter()
        .find(|p| p.id.eq(pool0.oracle_id))
        .ok_or(anyhow!(
            "No price for pool0 ({}) on price oracle.",
            pool0.symbol
        ))?
        .price
        .price;
    let pool1_price: u64 = prices
        .iter()
        .find(|p| p.id.eq(pool1.oracle_id))
        .ok_or(anyhow!(
            "No price for pool1 ({}) on price oracle.",
            pool1.symbol
        ))?
        .price
        .price;

    println!(
        "Latest prices {}: {} usd, {}: {} usd",
        pool0.symbol, pool0_price, pool1.symbol, pool1_price
    );

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 4] Create user zoroswap note\n");
    let amount_in = 3 * 10u64.pow(pool0.decimals as u32 - 2); // 0.03
    let max_slippage = 0.005; // 0.5 %
    let min_amount_out = (((pool0_price as f64) / (pool1_price as f64))
        * (amount_in as f64)
        * (1.0 - max_slippage)) as u64;
    //let min_amount_out = 0 as u64;
    let asset_in = FungibleAsset::new(pool0.faucet_id, amount_in)?;
    let asset_out = FungibleAsset::new(pool1.faucet_id, min_amount_out)?;
    let requested_asset_word: Word = asset_out.into();
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 120000;

    let beneficiary_id = account.id();
    let inputs = vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(deadline), // deadline
        p2id_tag.into(),     // p2id tag
        Felt::new(0),
        Felt::new(0),
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        account.id().suffix(),
        account.id().prefix().into(),
    ];
    let zoroswap_serial_num = client.rng().draw_word();
    println!(
        "Made an order note requesting {amount_in} {} for at least {min_amount_out} {}.",
        pool0.symbol, pool1.symbol
    );
    let zoroswap_note = create_zoroswap_note(
        inputs,
        vec![asset_in.into()],
        account.id(),
        zoroswap_serial_num,
        NoteTag::new(123),
        NoteType::Private,
    )?;

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(zoroswap_note.clone())])
        .build()
        .unwrap();

    let tx_id = client
        .submit_new_transaction(account.id(), note_req)
        .await
        .unwrap();

    print_transaction_info(&tx_id);
    print_note_info(&zoroswap_note.id());

    client.sync_state().await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 5] Send note to the server\n");

    let serialized_note = serialize_note(&zoroswap_note)?;
    send_to_server(
        &format!("http://{}", config.server_url),
        serialized_note,
        "orders",
    )
    .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 6] Wait for notes back\n");
    let consumable_notes = wait_for_consumable_notes(&mut client, account.id()).await?;
    println!("Received {} consumable notes.", consumable_notes.len());
    let consume_req = TransactionRequestBuilder::new()
        .build_consume_notes(consumable_notes)
        .unwrap();

    let tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(&mut client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet.");
    print_transaction_info(&tx_id);

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 7] Confirm pool states updated accordingly\n");
    let (new_balances_pool_0, _) =
        fetch_pool_state_from_chain(&mut client, config.pool_account_id, pool0.faucet_id).await?;
    let (new_balances_pool_1, _) =
        fetch_pool_state_from_chain(&mut client, config.pool_account_id, pool1.faucet_id).await?;
    let new_vault = fetch_vault_for_account_from_chain(&mut client, config.pool_account_id).await?;
    println!("previous balances for liq pool 0: {balances_pool_0:?}");
    println!("previouse balances for liq pool 1: {balances_pool_1:?}");
    println!("new balances for liq pool 0: {new_balances_pool_0:?}");
    println!("new balances for liq pool 1: {new_balances_pool_1:?}");
    println!("pool vault: {vault:?}");

    assert!(
        balances_pool_0 != new_balances_pool_0,
        "Balances for pool 0 havent changed"
    );
    assert!(
        balances_pool_1 != new_balances_pool_1,
        "Balances for pool 1 havent changed"
    );
    assert!(new_vault != vault, "Balances for pool 1 havent changed");

    Ok(())
}

// #[tokio::test]
// async fn e2e_private_deposit_withdraw_test() -> Result<()> {

async fn send_to_server(server_url: &str, note: String, endpoint: &str) -> Result<()> {
    let url = Url::from_str(format!("{server_url}/{endpoint}/submit").as_str())?;
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        .body(serde_json::json!({ "note_data": note }).to_string())
        .header("Content-Type", "application/json")
        .send()
        .await?;

    println!("Server response: {:?}", res.text().await?);
    Ok(())
}
