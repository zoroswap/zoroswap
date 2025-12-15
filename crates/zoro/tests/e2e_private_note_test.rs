use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use dotenv::dotenv;
use miden_client::store::TransactionFilter;
use miden_client::{
    Felt, Word,
    asset::FungibleAsset,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::str::FromStr;
use url::Url;
use zoro::{
    Config, ZoroStorageSettings, create_zoroswap_note, fetch_vault_for_account_from_chain,
    get_oracle_prices, instantiate_client, print_note_info, print_transaction_info, serialize_note,
};
use zoro_miden_client::{create_basic_account, wait_for_consumable_notes, wait_for_note};

#[tokio::test]
async fn e2e_private_note() -> Result<()> {
    dotenv().ok();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 0] Init client and config\n");

    let config = Config::from_config_file(
        "../../config.toml",
        "../../masm",
        "../../keystore",
        "../../testing_store.sqlite3",
    )?;
    assert!(
        config.liquidity_pools.len() > 1,
        "Less than 2 liquidity pools configured"
    );
    let mut client = instantiate_client(
        &config,
        ZoroStorageSettings::trading_storage("../../testing_store.sqlite3".to_string()),
    )
    .await?;
    let endpoint = config.miden_endpoint;
    let keystore = FilesystemKeyStore::new(config.keystore_path.into()).unwrap();
    let sync_summary = client.sync_state().await?;
    println!("\nLatest block: {}", sync_summary.block_num);

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Create user account\n");

    let (account, _) = create_basic_account(&mut client, keystore.clone()).await?;
    println!(
        "Created Account ⇒ ID: {:?}",
        account.id().to_bech32(endpoint.to_network_id())
    );
    client.sync_state().await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Fund user wallet\n");

    let pool0 = config
        .liquidity_pools
        .first()
        .expect("No liquidity pools found in config.");
    let pool1 = config
        .liquidity_pools
        .last()
        .expect("No liquidity pools found in config.");
    let amount: u64 = 5 * 10u64.pow(pool0.decimals as u32 - 2); // 0.05
    let fungible_asset = FungibleAsset::new(pool0.faucet_id, amount)?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        account.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let tx_id = client
        .submit_new_transaction(pool0.faucet_id, transaction_request)
        .await?;
    println!("Minted {amount} {} for the user.", pool0.symbol);
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

    wait_for_note(&mut client, &account, &minted_note).await?;

    let consume_req = TransactionRequestBuilder::new()
        .authenticated_input_notes([(minted_note.id(), None)])
        .build()
        .unwrap();

    let _tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(&mut client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet");

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
    let min_amount_out =
        ((pool0_price as f64) / (pool1_price as f64)) * (amount_in as f64) * (1.0 - max_slippage);
    let min_amount_out = min_amount_out as u64;
    let asset_in = FungibleAsset::new(pool0.faucet_id, amount_in)?;
    let asset_out = FungibleAsset::new(pool1.faucet_id, min_amount_out)?;
    let requested_asset_word: Word = asset_out.into();
    let p2id_tag = NoteTag::from_account_id(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 10000;
    let inputs = vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(deadline), // deadline
        p2id_tag.into(),     // p2id tag
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
        account.id().suffix().into(),
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
        NoteTag::LocalAny(0),
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
    send_to_server(&format!("http://{}", config.server_url), serialized_note).await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 6] Wait for notes back\n");
    let consumable_notes = wait_for_consumable_notes(&mut client, account.id()).await?;
    println!("Received {} consumable notes.", consumable_notes.len());
    let input_note_record = consumable_notes[0].clone().0;
    let note_id = input_note_record.id();
    let consume_req = TransactionRequestBuilder::new()
        .authenticated_input_notes([(note_id, None)])
        .build()
        .unwrap();

    let tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(&mut client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet.");
    print_transaction_info(&tx_id);

    Ok(())
}

async fn send_to_server(server_url: &str, note: String) -> Result<()> {
    let url = Url::from_str(format!("{server_url}/orders/submit").as_str())?;
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
