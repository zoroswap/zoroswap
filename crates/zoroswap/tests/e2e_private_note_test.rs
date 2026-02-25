mod test_utils;

use anyhow::Result;
use chrono::Utc;
use miden_client::note::{NoteTag, NoteType};
use test_utils::*;
use zoro_miden::account::MidenAccount;
use zoro_miden::note::{SwapInstructions, TrustedNote};

#[tokio::test]
async fn e2e_private_note() -> Result<()> {
    println!("\n\t[STEP 0] Init client and config\n");
    let store_path = "../../private_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);
    let E2ETestSetup {
        config,
        client: mut miden_client,
        keystore,
        mut zoro_pool,
        prices,
    } = E2ETestSetup::new(store_path).await?;
    let mut account = MidenAccount::deploy_new(&mut miden_client, keystore).await?;

    let pool0 = config.liquidity_pools[0];
    let pool1 = config.liquidity_pools[1];

    let initial_vault = zoro_pool.vault().await?;
    let initial_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let initial_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();
    zoro_pool.print_pool_states();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Fund user wallet\n");
    let amount = 500000;
    miden_client
        .mint_asset(pool0.faucet_id, *account.id(), amount)
        .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Create & send zoroswap note\n");
    let pool0_price = prices.get(&pool0.faucet_id).unwrap().price;
    let pool1_price = prices.get(&pool1.faucet_id).unwrap().price;
    let amount_in = 3 * 10u64.pow(pool0.decimals as u32 - 2); // 0.03
    let max_slippage = 0.005; // 0.5 %
    let min_amount_out =
        ((pool0_price as f64) / (pool1_price as f64)) * (amount_in as f64) * (1.0 - max_slippage);
    let min_amount_out = min_amount_out as u64;

    println!(
        "Made an order note requesting {amount_in} {} for at least {min_amount_out} {}.",
        pool0.symbol, pool1.symbol
    );

    let note = TrustedNote::new(zoro_miden::note::NoteInstructions::Swap(SwapInstructions {
        asset_in: pool0.faucet_id,
        amount_in,
        asset_out: pool1.faucet_id,
        min_amount_out,
        creator: *account.id(),
        beneficiary: None,
        note_type: NoteType::Private,
        deadline: Utc::now().timestamp_millis() as u64,
        p2id_tag: NoteTag::with_account_target(*account.id()).as_u32() as u64,
    }))?;

    zoro_pool.execute_notes(vec![note.clone()], prices).await?;
    note.print_note_info();

    miden_client.sync_state().await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 3] Send note to the server\n");

    send_to_server(
        &format!("http://{}", config.server_url),
        note.serialize_to_string()?,
        "orders",
    )
    .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 4] Wait for notes back\n");
    miden_client.consume_notes(account.id(), 1).await?;
    miden_client.sync_state().await?;
    let new_balance_user = account.get_balance(&pool1.faucet_id).await?;
    println!("New balance: {:?}", new_balance_user);

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 5] Confirm pool states updated accordingly\n");

    zoro_pool.update_pool_state_from_chain().await?;
    let end_vault = zoro_pool.vault().await?;
    let end_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let end_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();
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

    Ok(())
}
