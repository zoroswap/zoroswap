mod test_utils;

use anyhow::Result;
use chrono::Utc;
use miden_client::crypto::FeltRng;
use miden_client::{
    Felt, Word,
    asset::FungibleAsset,
    note::{NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::time::Duration;
use test_utils::*;
use zoroswap::{
    create_deposit_note, create_withdraw_note, create_zoroswap_note,
    fetch_lp_total_supply_from_chain, fetch_pool_state_from_chain,
    fetch_vault_for_account_from_chain, get_oracle_prices, print_note_info,
    print_transaction_info, serialize_note,
};

#[tokio::test]
async fn e2e_private_deposit_withdraw_test() -> Result<()> {
    // Use a fresh store to avoid leftover data from prior test runs.
    let store_path = "../../deposit_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);
    let mut setup = setup_test_environment(store_path).await?;
    let account = setup.account.clone();
    let pool = setup.pools[0];

    println!("\n\t[STEP 1] Fund user wallet\n");
    fund_user_wallet(&mut setup.client, &account, &pool, None).await?;

    let lp_total_supply_before = fetch_lp_total_supply_from_chain(
        &mut setup.client,
        setup.config.pool_account_id,
        pool.faucet_id,
    )
    .await?;

    println!("\n\t[STEP 2] Create DEPOSIT note\n");
    // 0.04 tokens: 4 hundredths × 10^decimals (the base unit) / 100
    let one_hundredth = 10u64.pow(pool.decimals as u32) / 100;
    let amount_in: u64 = 4 * one_hundredth;
    let max_slippage = 0.005; // 0.5 %
    let min_lp_amount_out = ((amount_in as f64) * (1.0 - max_slippage)) as u64;
    println!("\n\t min amount out: {min_lp_amount_out}");
    let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 120000;
    let inputs = vec![
        Felt::new(min_lp_amount_out), // min_lp_amount_out
        Felt::new(deadline),          // deadline
        p2id_tag.into(),              // p2id tag
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
        account.id().suffix(),
        account.id().prefix().into(),
    ];
    let _pool_contract_tag = NoteTag::with_account_target(setup.config.pool_account_id);
    let deposit_serial_num = setup.client.rng().draw_word();
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
    let _tx_id = setup
        .client
        .submit_new_transaction(account.id(), note_req)
        .await?;
    println!("Minted note of {} tokens for liq pool.", amount_in);
    setup.client.sync_state().await?;
    let serialized_note = serialize_note(&deposit_note)?;
    send_to_server(
        &format!("http://{}", setup.config.server_url),
        serialized_note,
        "deposit",
    )
    .await?;

    // Successful deposits don't produce P2ID notes back to the user. LP shares
    // are recorded in the pool's storage map. Just wait for the pool to process.
    tokio::time::sleep(Duration::from_secs(30)).await;
    setup.client.sync_state().await?;

    let lp_total_supply_after = fetch_lp_total_supply_from_chain(
        &mut setup.client,
        setup.config.pool_account_id,
        pool.faucet_id,
    )
    .await?;
    println!("lp_total_supply_after: {lp_total_supply_after}");
    println!("lp_total_supply_before: {lp_total_supply_before}");
    println!("min_lp_amount_out: {min_lp_amount_out}");
    assert!(
        lp_total_supply_after >= lp_total_supply_before + min_lp_amount_out,
        "LP total supply didnt increase"
    );

    println!("\n\t[STEP 3] Create WITHDRAW note\n");
    // 0.02 tokens
    let amount_to_withdraw: u64 = 2 * one_hundredth;
    let max_slippage = 0.005; // 0.5 %
    let min_asset_amount_out = (amount_to_withdraw as f64) * (1.0 - max_slippage);
    let min_asset_amount_out = min_asset_amount_out as u64;
    let asset_out: FungibleAsset = FungibleAsset::new(pool.faucet_id, min_asset_amount_out)?;
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) - 120000;
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
    let _pool_contract_tag = NoteTag::with_account_target(setup.config.pool_account_id);
    let withdraw_serial_num = setup.client.rng().draw_word();
    println!("######################################################### deadline: {deadline}");
    println!(
        "Made withdrawal note for {amount_to_withdraw} {} expecting  at least {min_asset_amount_out} lp amount out.",
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

    let _tx_id = setup
        .client
        .submit_new_transaction(account.id(), note_req)
        .await?;

    setup.client.sync_state().await?;
    send_to_server(
        &format!("http://{}", setup.config.server_url),
        serialize_note(&withdraw_note)?,
        "withdraw",
    )
    .await?;

    let consumable_notes =
        zoro_miden_client::wait_for_consumable_notes(&mut setup.client, account.id()).await?;
    println!(
        "Received {} consumable withdraw receipt notes.",
        consumable_notes.len()
    );
    let consume_req = TransactionRequestBuilder::new()
        .build_consume_notes(consumable_notes)
        .unwrap();

    let balance_before_withdraw =
        get_local_balance(&mut setup.client, account.id(), pool.faucet_id).await?;
    println!("User balance before withdraw (local): {balance_before_withdraw}");

    setup
        .client
        .submit_new_transaction(account.id(), consume_req)
        .await?;

    let balance_after_withdraw =
        get_local_balance(&mut setup.client, account.id(), pool.faucet_id).await?;
    println!("User balance after withdraw (local): {balance_after_withdraw}");

    // We can't assert_eq because the exact payout depends on the pool's
    // curve/fee calculation, which may return more than the minimum.
    // We only know the payout is >= min_asset_amount_out (the slippage floor).
    let expected_min_balance = balance_before_withdraw + min_asset_amount_out;
    assert!(
        balance_after_withdraw >= expected_min_balance,
        "User should have received at least min_asset_amount_out ({min_asset_amount_out}) \
         from withdraw (balance: {balance_after_withdraw}, expected >= {expected_min_balance})"
    );

    setup.client.sync_state().await?;

    let total_lp_supply_after_withdraw: u64 = fetch_lp_total_supply_from_chain(
        &mut setup.client,
        setup.config.pool_account_id,
        pool.faucet_id,
    )
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
    let mut setup = setup_test_environment(store_path).await?;
    let account = setup.account.clone();
    let pool0 = setup.pools[0];
    let pool1 = setup.pools[1];

    println!("\n\t[STEP 1] Fund user wallet\n");
    fund_user_wallet(&mut setup.client, &account, &pool0, None).await?;

    let (balances_pool_0, _) = fetch_pool_state_from_chain(
        &mut setup.client,
        setup.config.pool_account_id,
        pool0.faucet_id,
    )
    .await?;
    let (balances_pool_1, _) = fetch_pool_state_from_chain(
        &mut setup.client,
        setup.config.pool_account_id,
        pool1.faucet_id,
    )
    .await?;
    let vault =
        fetch_vault_for_account_from_chain(&mut setup.client, setup.config.pool_account_id)
            .await?;
    println!("balances for liq pool 0: {balances_pool_0:?}");
    println!("balances for liq pool 1: {balances_pool_1:?}");
    println!("pool vault on-chain: {vault:?}");

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Fetching latest prices from the oracle\n");

    let prices = get_oracle_prices(
        setup.config.oracle_https,
        vec![pool0.oracle_id, pool1.oracle_id],
    )
    .await?;
    let pool0_price = extract_oracle_price(&prices, pool0.oracle_id, pool0.symbol)?;
    let pool1_price = extract_oracle_price(&prices, pool1.oracle_id, pool1.symbol)?;

    println!(
        "Latest prices {}: {} usd, {}: {} usd",
        pool0.symbol, pool0_price, pool1.symbol, pool1_price
    );

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 3] Create user zoroswap note\n");
    let amount_in = 3 * 10u64.pow(pool0.decimals as u32 - 2); // 0.03
    let max_slippage = 0.005; // 0.5 %
    let min_amount_out = (((pool0_price as f64) / (pool1_price as f64))
        * (amount_in as f64)
        * (1.0 - max_slippage)) as u64;
    let asset_in = FungibleAsset::new(pool0.faucet_id, amount_in)?;
    let asset_out = FungibleAsset::new(pool1.faucet_id, min_amount_out)?;
    let requested_asset_word: Word = asset_out.into();
    let p2id_tag = NoteTag::with_account_target(account.id());
    let deadline = (Utc::now().timestamp_millis() as u64) + 120000;

    let inputs = build_zoroswap_inputs(
        requested_asset_word,
        deadline,
        p2id_tag,
        account.id(),
        account.id(),
    );
    let zoroswap_serial_num = setup.client.rng().draw_word();
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

    let tx_id = setup
        .client
        .submit_new_transaction(account.id(), note_req)
        .await
        .unwrap();

    print_transaction_info(&tx_id);
    print_note_info(&zoroswap_note.id());

    setup.client.sync_state().await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 4] Send note to the server\n");

    let serialized_note = serialize_note(&zoroswap_note)?;
    send_to_server(
        &format!("http://{}", setup.config.server_url),
        serialized_note,
        "orders",
    )
    .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 5] Wait for notes back\n");
    let consumable_notes =
        zoro_miden_client::wait_for_consumable_notes(&mut setup.client, account.id()).await?;
    println!("Received {} consumable notes.", consumable_notes.len());
    let consume_req = TransactionRequestBuilder::new()
        .build_consume_notes(consumable_notes)
        .unwrap();

    let tx_id = setup
        .client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    setup.client.sync_state().await?;
    let new_balance_user =
        fetch_vault_for_account_from_chain(&mut setup.client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet.");
    print_transaction_info(&tx_id);

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 6] Confirm pool states updated accordingly\n");
    assert_pool_states_changed(
        &mut setup.client,
        setup.config.pool_account_id,
        pool0.faucet_id,
        pool1.faucet_id,
        &balances_pool_0,
        &balances_pool_1,
        &vault,
    )
    .await?;

    Ok(())
}
