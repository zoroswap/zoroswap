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
use zoro_miden::account::MidenAccount;
use zoro_miden::note::TrustedNote;

#[tokio::test]
async fn e2e_private_deposit_withdraw_test() -> Result<()> {
    println!("\n\t[STEP 0] Init client and config\n");
    let store_path = "../../private_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);
    let E2ETestSetup {
        config,
        client: mut miden_client,
        keystore,
        mut zoro_pool,
    } = E2ETestSetup::new(store_path).await?;

    let account = MidenAccount::deploy_new(&mut miden_client, keystore).await?;
    let pool = config.liquidity_pools[0];
    let initial_pool = *zoro_pool.pool_states().get(&pool.faucet_id).unwrap();
    zoro_pool.print_pool_states();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Fund user wallet\n");
    let amount = 500000;
    miden_client.mint_asset(pool.faucet_id, *account.id(), amount);

    println!("\n\t[STEP 2] Create DEPOSIT note\n");
    let amount_in = 4;
    let amount_in: u64 = amount_in * 10u64.pow(pool.decimals as u32 - 2);
    let max_slippage = 0.005; // 0.5 %
    let min_lp_amount_out = ((amount_in as f64) * (1.0 - max_slippage)) as u64;
    println!("\n\t min amount out: {min_lp_amount_out}");
    let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
    let p2id_tag = NoteTag::with_account_target(*account.id());
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
    let deposit_serial_num = miden_client.client_mut().rng().draw_word();
    println!(
        "Made an deposit note for {amount_in} {} expecting  at least {min_lp_amount_out} lp amount out.",
        pool.symbol
    );
    let deposit_note = TrustedNote::new_deposit(
        inputs,
        vec![asset_in.into()],
        *account.id(),
        deposit_serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )?;

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(deposit_note.note().clone())])
        .build()
        .unwrap();

    println!("tx request built");
    let _tx_id = miden_client
        .client_mut()
        .submit_new_transaction(*account.id(), note_req)
        .await?;
    println!("Minted note of {} tokens for liq pool.", amount_in);
    miden_client.sync_state().await?;
    send_to_server(
        &format!("http://{}", config.server_url),
        deposit_note.serialize_to_string()?,
        "deposit",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(30)).await;
    miden_client.sync_state().await?;

    zoro_pool.update_pool_state_from_chain().await?;
    let pool_after_deposit = *zoro_pool.pool_states().get(&pool.faucet_id).unwrap();
    println!(
        "lp_total_supply_after: {}",
        pool_after_deposit.lp_total_supply()
    );
    println!("lp_total_supply_before: {}", initial_pool.lp_total_supply());
    println!("min_lp_amount_out: {min_lp_amount_out}");
    assert!(
        pool_after_deposit.lp_total_supply() >= initial_pool.lp_total_supply() + min_lp_amount_out,
        "LP total supply didnt increase"
    );

    println!("\n\t[STEP 3] Create WITHDRAW note\n");
    let amount_to_withdraw = 2;
    let amount_to_withdraw: u64 = amount_to_withdraw * 10u64.pow(pool.decimals as u32 - 2);
    let max_slippage = 0.005; // 0.5 %
    let min_asset_amount_out = (amount_to_withdraw as f64) * (1.0 - max_slippage);
    let min_asset_amount_out = min_asset_amount_out as u64;
    let asset_out: FungibleAsset = FungibleAsset::new(pool.faucet_id, min_asset_amount_out)?;
    let p2id_tag = NoteTag::with_account_target(*account.id());
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
    let _pool_contract_tag = NoteTag::with_account_target(config.pool_account_id);
    let withdraw_serial_num = miden_client.client_mut().rng().draw_word();
    println!("######################################################### deadline: {deadline}");
    println!(
        "Made withdrawal note for {amount_to_withdraw} {} expecting  at least {min_asset_amount_out} lp amount out.",
        pool.symbol
    );
    let withdraw_note = TrustedNote::new_withdraw(
        inputs,
        vec![],
        *account.id(),
        withdraw_serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )
    .await?;

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(withdraw_note.note().clone())])
        .build()
        .unwrap();

    let _tx_id = miden_client
        .client_mut()
        .submit_new_transaction(*account.id(), note_req)
        .await?;

    miden_client.sync_state().await?;
    send_to_server(
        &format!("http://{}", config.server_url),
        withdraw_note.serialize_to_string()?,
        "withdraw",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(25)).await;

    zoro_pool.update_pool_state_from_chain().await?;
    let pool_after_withdraw = *zoro_pool.pool_states().get(&pool.faucet_id).unwrap();

    println!(
        "LP total supply before withdraw: {}, after withdraw: {}",
        pool_after_deposit.lp_total_supply(),
        pool_after_withdraw.lp_total_supply()
    );

    assert!(
        pool_after_withdraw.lp_total_supply() < pool_after_deposit.lp_total_supply(),
        "Total LP amount after withdraw did not decrease"
    );

    Ok(())
}
