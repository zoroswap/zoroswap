mod test_utils;

use anyhow::Result;
use test_utils::*;

#[tokio::test]
async fn e2e_create_funded_accounts() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info,zoro=debug")
        .init();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 0] Init client and config\n");

    let store_path = "../../create_funded_accounts_test_store.sqlite3";
    let _ = std::fs::remove_file(store_path);

    let mut setup = setup_test_environment(store_path).await?;
    let num_accounts = 2;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Create {num_accounts} funded accounts\n");

    let amount = 5_000_000u64; // fixed amount per pool
    let accounts = create_funded_accounts(
        &mut setup.client,
        &setup.config,
        num_accounts,
        Some(amount),
        None,
    )
    .await?;

    assert_eq!(
        accounts.len(),
        num_accounts,
        "Expected {num_accounts} accounts"
    );

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Verify each account holds tokens from every pool\n");

    setup.client.sync_state().await?;

    for (i, account) in accounts.iter().enumerate() {
        for pool in &setup.config.liquidity_pools {
            let balance =
                get_local_balance(&mut setup.client, account.id(), pool.faucet_id).await?;
            println!("Account #{i} — {} balance: {balance}", pool.symbol);
            assert!(
                balance >= amount,
                "Account #{i} should have at least {amount} of {}, got {balance}",
                pool.symbol
            );
        }
    }

    println!("\nAll {num_accounts} accounts verified with expected balances.");
    Ok(())
}
