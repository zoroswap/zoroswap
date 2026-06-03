mod test_utils;

use anyhow::Result;
use chrono::Utc;
use miden_client::asset::FungibleAsset;
use miden_client::note::{NoteTag, NoteType};
use std::time::Duration;
use test_utils::*;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zoro_miden::account::MidenAccount;
use zoro_miden::note::{NoteInstructions, NoteKind, TrustedNote};

#[tokio::test]
async fn e2e_private_deposit_withdraw() -> Result<()> {
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .init();

    println!("\n\t[STEP 0] Init client and config\n");
    let E2ETestSetup {
        config,
        client: mut miden_client,
        mut zoro_pool,
        prices: _,
    } = E2ETestSetup::new().await?;
    let mut account = MidenAccount::deploy_new(&mut miden_client).await?;
    let pool = config.liquidity_pools[0];

    info!(
        "Testing with account {} with tag {}",
        account
            .id()
            .to_bech32(config.miden_endpoint.to_network_id()),
        NoteTag::with_account_target(*account.id())
    );

    let initial_pool = *zoro_pool.pool_states().get(&pool.faucet_id).unwrap();

    println!("\n\t[STEP 1] Fund user wallet\n");
    let amount = 500000;
    miden_client
        .mint_asset(pool.faucet_id, *account.id(), amount)
        .await?;
    let user_balance = account.get_balance(&pool.faucet_id).await?;
    info!("Minted: {amount} to the test account. New balance: {user_balance}");

    println!("\n\t[STEP 2] Create DEPOSIT note\n");
    let amount_in = amount / 2;
    let max_slippage = 0.005; // 0.5 %
    let min_lp_amount_out = ((amount_in as f64) * (1.0 - max_slippage)) as u64;

    let deposit_note = TrustedNote::new(
        NoteInstructions {
            attached_assets: vec![FungibleAsset::new(pool.faucet_id, amount_in)?],
            amount_input: min_lp_amount_out,
            beneficiary: *account.id(),
            note_type: NoteType::Private,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: NoteTag::with_account_target(*account.id()),
            pool_tag: NoteTag::with_account_target(config.pool_account_id),
            note_kind: NoteKind::Deposit,
            asset_input: None,
        },
        miden_client.client_mut().code_builder(),
    )?;

    miden_client
        .send_note(account.id(), &config.pool_account_id, deposit_note.clone())
        .await?;

    miden_client.sync_state().await?;
    send_to_server(
        &format!("http://{}", config.server_url),
        vec![deposit_note.serialize_to_string().unwrap()],
        "deposit/submit",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(20)).await;
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
    let amount_to_withdraw = min_lp_amount_out / 2;
    let max_slippage = 0.005; // 0.5 %
    let min_asset_amount_out = (amount_to_withdraw as f64) * (1.0 - max_slippage);
    let min_asset_amount_out = min_asset_amount_out as u64;

    let withdraw_note = TrustedNote::new(
        NoteInstructions {
            asset_input: Some(FungibleAsset::new(pool.faucet_id, min_asset_amount_out)?),
            amount_input: amount_to_withdraw,
            beneficiary: *account.id(),
            note_type: NoteType::Private,
            p2id_tag: NoteTag::with_account_target(*account.id()),
            pool_tag: NoteTag::with_account_target(config.pool_account_id),
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            note_kind: NoteKind::Withdraw,
            attached_assets: vec![],
        },
        miden_client.client_mut().code_builder(),
    )?;

    miden_client
        .send_note(account.id(), &config.pool_account_id, withdraw_note.clone())
        .await?;

    send_to_server(
        &format!("http://{}", config.server_url),
        vec![withdraw_note.serialize_to_string().unwrap()],
        "withdraw/submit",
    )
    .await?;

    tokio::time::sleep(Duration::from_secs(20)).await;

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
