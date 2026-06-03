mod test_utils;

use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use miden_client::asset::FungibleAsset;
use miden_client::note::NoteTag;
use miden_client::transaction::TransactionRequestBuilder;
use test_utils::*;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zoro_miden::account::MidenAccount;
use zoro_miden::note::{NoteInstructions, NoteKind, TrustedNote};
use zoroswap::server::AddPositionResponse;

use futures_util::future::try_join_all;

#[tokio::test]
async fn e2e_position_scale_swap() -> Result<()> {
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
        prices,
    } = E2ETestSetup::new().await?;
    let mut account = MidenAccount::deploy_new(&mut miden_client).await?;
    let pool0 = config.liquidity_pools[0];
    let pool1 = config.liquidity_pools[1];

    info!(
        "Testing with account {} with tag {}",
        account
            .id()
            .to_bech32(config.miden_endpoint.to_network_id()),
        NoteTag::with_account_target(*account.id())
    );

    let initial_vault = zoro_pool.vault().await?;
    let initial_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let initial_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Fund user wallet\n");
    let amount0 = 500000;
    let amount = amount0;
    // let amount1 = 1000000;
    let n_notes = 5;
    miden_client
        .mint_asset(pool0.faucet_id, *account.id(), amount * n_notes)
        .await?;

    tokio::time::sleep(Duration::from_millis(4100)).await;

    let user_balance0 = account.get_balance(&pool0.faucet_id).await?;
    let user_balance1 = account.get_balance(&pool1.faucet_id).await?;
    info!("Minted: {amount} to the test account. New balance: {user_balance0}");

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Create position\n");

    let notes = (0..n_notes)
        .map(|i| {
            TrustedNote::new(
                NoteInstructions {
                    note_kind: NoteKind::Position,
                    attached_assets: vec![FungibleAsset::new(pool0.faucet_id, amount)?],
                    asset_input: None,
                    beneficiary: *account.id(),
                    amount_input: amount,
                    note_type: miden_client::note::NoteType::Public,
                    deadline: Utc::now().timestamp_millis() as u64 + 120_000,
                    p2id_tag: account.tag(),
                    pool_tag: zoro_pool.miden_account().tag(),
                },
                miden_client.client().code_builder(),
            )
        })
        .collect::<Result<Vec<_>>>()?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 3] Init position on server\n");

    miden_client
        .send_notes(account.id(), &config.pool_account_id, notes.clone())
        .await?;
    let res = send_to_server(
        &format!("http://{}", config.server_url),
        notes
            .iter()
            .map(|note| note.serialize_to_string().unwrap())
            .collect(),
        "positions/new",
    )
    .await?;

    let res: Vec<AddPositionResponse> = res
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();

    let pool0_price = prices.get(&pool0.faucet_id).unwrap().price;
    let pool1_price = prices.get(&pool1.faucet_id).unwrap().price;
    let amount_in = amount / 2;
    let max_slippage = 0.005; // 0.5 %
    let min_amount_out =
        ((pool0_price as f64) / (pool1_price as f64)) * (amount_in as f64) * (1.0 - max_slippage);
    let min_amount_out = min_amount_out as u64;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 4] Do position swap on server\n");
    try_join_all(res.iter().map(|r| {
        send_position_swap_to_server(
            format!("http://{}", config.server_url),
            "positions/swap".to_string(),
            r.position_id,
            pool0.faucet_id.to_bech32(config.network_id.clone()),
            pool1.faucet_id.to_bech32(config.network_id.clone()),
            amount,
            min_amount_out,
        )
    }))
    .await?;

    println!("\n\t... waiting for the note to be executed on the server \n");
    tokio::time::sleep(Duration::from_millis(20_000)).await;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 5] Get note back from server\n");

    let reclaimed_notes = try_join_all(res.iter().map(|r| {
        get_position_note(
            format!("{}", config.server_url),
            "positions/get_note".to_string(),
            r.position_id,
        )
    }))
    .await?;
    println!(
        "Reclaim note ids: {}",
        reclaimed_notes
            .iter()
            .map(|r| r.note().id().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 6] Reclaim the note\n");
    miden_client.sync_state().await?;

    let reclaim_transaction_request = TransactionRequestBuilder::new().build_consume_notes(
        reclaimed_notes
            .into_iter()
            .map(|r| r.note().clone())
            .collect::<Vec<_>>(),
    )?;
    miden_client
        .client_mut()
        .submit_new_transaction(*account.id(), reclaim_transaction_request)
        .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 7] Confirm pool states updated accordingly\n");
    tokio::time::sleep(Duration::from_millis(4100)).await;
    zoro_pool.update_pool_state_from_chain().await?;
    let end_vault = zoro_pool.vault().await?;
    let end_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let end_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();
    let end_user_balance0 = account.get_balance(&pool0.faucet_id).await?;
    let end_user_balance1 = account.get_balance(&pool1.faucet_id).await?;

    zoro_pool.print_pool_states();

    assert!(
        end_pool0.balances() != initial_pool0.balances(),
        "Balances for pool 0 havent changed"
    );
    assert!(
        end_pool1.balances() != initial_pool1.balances(),
        "Balances for pool 1 havent changed"
    );
    assert!(end_vault != initial_vault, "Vault hasn't changed");
    assert!(
        end_user_balance0 != user_balance0,
        "Balances for user for faucet0 havent changed"
    );
    assert!(
        end_user_balance1 != user_balance1,
        "Balances for user for faucet1 havent changed"
    );

    Ok(())
}
