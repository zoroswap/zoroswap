use num_traits::pow::Pow;
use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use chrono::Utc;
use miden_client::{
    Felt, account::AccountId, asset::FungibleAsset, note::NoteType,
    transaction::TransactionRequestBuilder,
};
use tracing::info;
use zoro_miden::{
    asset_utils::asset_to_word,
    note::{NoteInstructions, NoteKind, TrustedNote},
    pool::ZoroPool,
    price::PriceData,
    test_utils::{PoolWithMeta, TestUtils},
};

#[tokio::test]
async fn executing_deposit() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let pool = test_utils.get_pools(1).await?;
    let pool = pool.first().unwrap();
    let pool_config = pool.pool_configs[..][0];
    let mut zoro_pool = ZoroPool::new_from_existing_pool(
        test_utils.miden_endpoint(),
        &test_utils.miden_client().keystore_dir(),
        &test_utils.miden_client().store_dir(),
        pool.miden_account.id(),
        pool.pool_configs.clone(),
    )
    .await?;
    let amount = 10_000;
    let user = test_utils
        .get_funded_accounts(1, vec![(pool_config.faucet_id, amount, amount * 10)])
        .await?;
    let mut user = user.first().unwrap().clone();
    let user_id = *user.miden_account.id();

    let user_balance_before = user
        .miden_account
        .get_balance(&pool_config.faucet_id)
        .await?;

    let pool_balances_before = *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();

    let deposit_note = TrustedNote::new(
        NoteInstructions {
            note_kind: NoteKind::Deposit,
            attached_assets: vec![FungibleAsset::new(pool_config.faucet_id, amount)?],
            amount_input: amount - 100,
            asset_input: None,
            beneficiary: user_id,
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: pool.miden_account.tag(),
        },
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, pool.miden_account.id(), deposit_note.clone())
        .await?;
    zoro_pool
        .execute_notes(vec![deposit_note], HashMap::default(), HashMap::default())
        .await?;

    let user_balance_after = user
        .miden_account
        .get_balance(&pool_config.faucet_id)
        .await?;
    let pool_balances_after = *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();

    assert_eq!(user_balance_after, user_balance_before - amount);
    assert_eq!(
        pool_balances_after.balances().reserve.to::<u64>(),
        pool_balances_before.balances().reserve.to::<u64>() + amount
    );
    assert_eq!(
        pool_balances_after
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_before
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            + amount
    );
    assert_eq!(
        pool_balances_after.balances().total_liabilities.to::<u64>(),
        pool_balances_before
            .balances()
            .total_liabilities
            .to::<u64>()
            + amount
    );
    assert!(pool_balances_after.lp_total_supply() > pool_balances_before.lp_total_supply());

    zoro_pool.update_pool_state_from_chain().await?;

    // wait for one block
    tokio::time::sleep(Duration::from_millis(2100)).await;

    let pool_balances_after_sync = *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();
    assert_eq!(
        pool_balances_after_sync.balances().reserve.to::<u64>(),
        pool_balances_before.balances().reserve.to::<u64>() + amount
    );
    assert_eq!(
        pool_balances_after_sync
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_before
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            + amount
    );
    assert_eq!(
        pool_balances_after_sync
            .balances()
            .total_liabilities
            .to::<u64>(),
        pool_balances_before
            .balances()
            .total_liabilities
            .to::<u64>()
            + amount
    );
    assert!(pool_balances_after_sync.lp_total_supply() > pool_balances_before.lp_total_supply());

    Ok(())
}

fn get_decimals_scaling_factor(decimals_in: u8, decimals_out: u8) -> f64 {
    10_f64.pow(decimals_out as f32 - decimals_in as f32)
}

#[tokio::test]
async fn executing_swap() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let PoolWithMeta {
        zoro_pool,
        test_pool,
    } = &mut test_utils.get_funded_pools(1).await?[..][0];
    info!("--- Pool ready");
    let pool_config_token0 = test_pool.pool_configs[..][0];
    let pool_config_token1 = test_pool.pool_configs[..][1];
    let decimals_scaling_factor =
        get_decimals_scaling_factor(pool_config_token0.decimals, pool_config_token1.decimals);
    info!("--- Decimals scaling factor: {}", decimals_scaling_factor);
    let amount = 100_000;
    let min_amount_out = (amount as f64 * decimals_scaling_factor) as u64 / 2;

    let user = test_utils
        .get_funded_accounts(1, vec![(pool_config_token0.faucet_id, amount, amount * 10)])
        .await?;
    info!("--- User ready");
    let mut user = user.first().unwrap().clone();
    let user_id = *user.miden_account.id();
    let mut prices: HashMap<AccountId, PriceData> = HashMap::with_capacity(2);

    let user_balance_before_0 = user
        .miden_account
        .get_balance(&pool_config_token0.faucet_id)
        .await?;
    let user_balance_before_1 = user
        .miden_account
        .get_balance(&pool_config_token1.faucet_id)
        .await?;
    let pool_balances_before_0 = *zoro_pool
        .pool_states()
        .get(&pool_config_token0.faucet_id)
        .unwrap();
    let pool_balances_before_1 = *zoro_pool
        .pool_states()
        .get(&pool_config_token1.faucet_id)
        .unwrap();

    prices.insert(pool_config_token0.faucet_id, PriceData::new_at_now(1));
    prices.insert(pool_config_token1.faucet_id, PriceData::new_at_now(1));
    info!("--- Creating note min amount out: {}", min_amount_out);
    let note = TrustedNote::new(
        NoteInstructions {
            note_kind: NoteKind::Swap,
            attached_assets: vec![FungibleAsset::new(pool_config_token0.faucet_id, amount)?],
            asset_input: Some(FungibleAsset::new(
                pool_config_token1.faucet_id,
                min_amount_out,
            )?),
            amount_input: 0,
            beneficiary: *user.miden_account.id(),
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: test_pool.miden_account.tag(),
        },
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, test_pool.miden_account.id(), note.clone())
        .await?;
    info!("--- Swap sent");
    zoro_pool
        .execute_notes(vec![note], prices, HashMap::default())
        .await?;
    info!("--- Swap executed");

    test_utils
        .miden_client_mut()
        .consume_simple_notes(&user_id, 1)
        .await?;
    info!("--- User claim executed");

    // wait for one block
    tokio::time::sleep(Duration::from_millis(4100)).await;

    let user_balance_after_0 = user
        .miden_account
        .get_balance(&pool_config_token0.faucet_id)
        .await?;
    let user_balance_after_1 = user
        .miden_account
        .get_balance(&pool_config_token1.faucet_id)
        .await?;
    let pool_balances_after_0 = *zoro_pool
        .pool_states()
        .get(&pool_config_token0.faucet_id)
        .unwrap();
    let pool_balances_after_1 = *zoro_pool
        .pool_states()
        .get(&pool_config_token1.faucet_id)
        .unwrap();

    // User balances should change accordingly
    assert_eq!(user_balance_after_0, user_balance_before_0 - amount);
    info!(
        "--- User balance: before 1: {}, after 1: {}, diff: {}, min amount out: {}",
        user_balance_before_1,
        user_balance_after_1,
        if user_balance_after_1 < user_balance_before_1 {
            0
        } else {
            user_balance_after_1 - user_balance_before_1
        },
        min_amount_out
    );
    // assert!(user_balance_after_1 >= user_balance_before_1 + min_amount_out);

    // Reserve should change accordingly
    assert_eq!(
        pool_balances_after_0.balances().reserve.to::<u64>(),
        pool_balances_before_0.balances().reserve.to::<u64>() + amount
    );
    assert!(
        pool_balances_after_1.balances().reserve.to::<u64>()
            <= pool_balances_before_1.balances().reserve.to::<u64>() + amount
    );

    // Reserve should change accordingly
    assert_eq!(
        pool_balances_after_0
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_before_0
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            + amount
    );
    assert!(
        pool_balances_after_1
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            <= pool_balances_before_1
                .balances()
                .reserve_with_slippage
                .to::<u64>()
                + amount
    );

    // Liabilities should remain unchanged
    assert_eq!(
        pool_balances_after_0.balances().total_liabilities,
        pool_balances_before_0.balances().total_liabilities
    );

    assert!(
        pool_balances_after_1
            .balances()
            .total_liabilities
            .to::<u64>()
            > pool_balances_before_1
                .balances()
                .total_liabilities
                .to::<u64>()
    );

    Ok(())
}

#[tokio::test]
async fn executing_position_swap() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let PoolWithMeta {
        zoro_pool,
        test_pool,
    } = &mut test_utils.get_funded_pools(1).await?[..][0];
    info!("--- Pool ready");
    let pool_config_token0 = test_pool.pool_configs[..][0];
    let pool_config_token1 = test_pool.pool_configs[..][1];
    let decimals_scaling_factor =
        get_decimals_scaling_factor(pool_config_token0.decimals, pool_config_token1.decimals);
    info!("--- Decimals scaling factor: {}", decimals_scaling_factor);
    let amount = 100_000;
    let min_amount_out = (amount as f64 * decimals_scaling_factor) as u64 / 2;
    let user = test_utils
        .get_funded_accounts(1, vec![(pool_config_token0.faucet_id, amount, amount * 10)])
        .await?;
    info!("--- User ready");
    let mut user = user.first().unwrap().clone();
    let user_id = *user.miden_account.id();
    let mut prices: HashMap<AccountId, PriceData> = HashMap::with_capacity(2);

    let user_balance_before_0 = user
        .miden_account
        .get_balance(&pool_config_token0.faucet_id)
        .await?;
    let user_balance_before_1 = user
        .miden_account
        .get_balance(&pool_config_token1.faucet_id)
        .await?;
    let pool_balances_before_0 = *zoro_pool
        .pool_states()
        .get(&pool_config_token0.faucet_id)
        .unwrap();
    let pool_balances_before_1 = *zoro_pool
        .pool_states()
        .get(&pool_config_token1.faucet_id)
        .unwrap();

    prices.insert(pool_config_token0.faucet_id, PriceData::new_at_now(1));
    prices.insert(pool_config_token1.faucet_id, PriceData::new_at_now(1));
    info!("--- Creating note min amount out: {}", min_amount_out);
    let note = TrustedNote::new(
        NoteInstructions {
            note_kind: NoteKind::Position,
            attached_assets: vec![FungibleAsset::new(pool_config_token0.faucet_id, amount)?],
            asset_input: None,
            beneficiary: *user.miden_account.id(),
            amount_input: amount,
            note_type: NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: test_pool.miden_account.tag(),
        },
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, test_pool.miden_account.id(), note.clone())
        .await?;
    info!("--- POSITION note sent");

    let sell_asset_amount = amount / 2;
    let sell_asset_arg = asset_to_word(FungibleAsset::new(
        pool_config_token0.faucet_id,
        sell_asset_amount,
    )?);
    let min_buy_asset_amount = amount / 4;
    let buy_asset_arg = asset_to_word(FungibleAsset::new(
        pool_config_token1.faucet_id,
        min_buy_asset_amount,
    )?);
    let user_swap_args = HashMap::from([(
        note.note().id(),
        sell_asset_arg
            .iter()
            .copied()
            .chain(buy_asset_arg.iter().copied())
            .collect::<Vec<Felt>>(),
    )]);
    let note_id = note.note().id();
    let execution_result = zoro_pool
        .execute_notes(vec![note], prices, user_swap_args)
        .await?;

    let respawned_note = execution_result.get(&note_id).unwrap().clone().1.unwrap();
    info!("--- POSITION note executed");
    tokio::time::sleep(Duration::from_millis(4100)).await;
    test_utils.miden_client_mut().sync_state().await?;

    let reclaim_transaction_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![respawned_note.note().clone()])?;
    test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user_id, reclaim_transaction_request)
        .await?;
    info!("--- User claim executed");

    // wait for one block
    tokio::time::sleep(Duration::from_millis(4100)).await;

    let user_balance_after_0 = user
        .miden_account
        .get_balance(&pool_config_token0.faucet_id)
        .await?;
    let user_balance_after_1 = user
        .miden_account
        .get_balance(&pool_config_token1.faucet_id)
        .await?;
    let pool_balances_after_0 = *zoro_pool
        .pool_states()
        .get(&pool_config_token0.faucet_id)
        .unwrap();
    let pool_balances_after_1 = *zoro_pool
        .pool_states()
        .get(&pool_config_token1.faucet_id)
        .unwrap();

    // User balances should change accordingly
    assert_eq!(
        user_balance_after_0,
        user_balance_before_0 - sell_asset_amount
    );
    info!(
        "--- User balance: before 1: {}, after 1: {}, diff: {}, min amount out: {}",
        user_balance_before_1,
        user_balance_after_1,
        if user_balance_after_1 < user_balance_before_1 {
            0
        } else {
            user_balance_after_1 - user_balance_before_1
        },
        min_amount_out
    );
    // assert!(user_balance_after_1 >= user_balance_before_1 + min_amount_out);

    // Reserve should change accordingly
    assert_eq!(
        pool_balances_after_0.balances().reserve.to::<u64>(),
        pool_balances_before_0.balances().reserve.to::<u64>() + sell_asset_amount
    );
    assert!(
        pool_balances_after_1.balances().reserve.to::<u64>()
            <= (pool_balances_before_1.balances().reserve.to::<u64>() - min_buy_asset_amount)
    );

    // Reserve should change accordingly
    assert_eq!(
        pool_balances_after_0
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_before_0
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            + sell_asset_amount
    );
    assert!(
        pool_balances_after_1
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            <= (pool_balances_before_1
                .balances()
                .reserve_with_slippage
                .to::<u64>()
                - min_buy_asset_amount)
    );

    // Liabilities should remain unchanged
    assert_eq!(
        pool_balances_after_0.balances().total_liabilities,
        pool_balances_before_0.balances().total_liabilities
    );

    assert!(
        pool_balances_after_1
            .balances()
            .total_liabilities
            .to::<u64>()
            > pool_balances_before_1
                .balances()
                .total_liabilities
                .to::<u64>()
    );

    Ok(())
}

#[tokio::test]
async fn executing_deposit_withdraw() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    info!(
        "--- \n\nExecuting deposit and then withdraw and checking if the values for balances and pool states are correct.\n"
    );
    let PoolWithMeta {
        zoro_pool,
        test_pool,
    } = &mut test_utils.get_initialized_pools(1).await?[..][0];
    info!("--- Pool ready");
    let pool_config = test_pool.pool_configs[..][0];
    let amount = 100_000;
    let faucet_id = pool_config.faucet_id;
    let mut user = test_utils
        .get_funded_accounts(1, vec![(faucet_id, amount, amount * 30)])
        .await?
        .first()
        .cloned()
        .unwrap();
    info!("--- User ready");
    let user_id = *user.miden_account.id();
    let mut prices: HashMap<AccountId, PriceData> = HashMap::with_capacity(2);
    prices.insert(pool_config.faucet_id, PriceData::new_at_now(1));

    info!("Using faucet id: {:?}", pool_config.faucet_id);
    info!("Using user id: {:?}", user_id);

    let user_balance_before = user
        .miden_account
        .get_balance(&pool_config.faucet_id)
        .await?;
    let pool_balances_before = *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();

    let deposit_note = TrustedNote::new(
        NoteInstructions {
            note_kind: NoteKind::Deposit,
            attached_assets: vec![FungibleAsset::new(pool_config.faucet_id, amount)?],
            amount_input: amount - 100,
            asset_input: None,
            beneficiary: user_id,
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: zoro_pool.miden_account().tag(),
        },
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(
            &user_id,
            zoro_pool.miden_account().id(),
            deposit_note.clone(),
        )
        .await?;
    info!("--- Deposit sent");
    zoro_pool
        .execute_notes(vec![deposit_note], prices, HashMap::default())
        .await?;
    info!("--- Deposit executed");

    // wait for one block
    tokio::time::sleep(Duration::from_millis(2100)).await;

    let user_balance_after_deposit = user
        .miden_account
        .get_balance(&pool_config.faucet_id)
        .await?;
    let pool_balances_after_deposit = *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();

    assert_eq!(user_balance_after_deposit, user_balance_before - amount);
    assert_eq!(
        pool_balances_after_deposit.balances().reserve.to::<u64>(),
        pool_balances_before.balances().reserve.to::<u64>() + amount
    );
    assert_eq!(
        pool_balances_after_deposit
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_before
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            + amount
    );
    assert_eq!(
        pool_balances_after_deposit
            .balances()
            .total_liabilities
            .to::<u64>(),
        pool_balances_before
            .balances()
            .total_liabilities
            .to::<u64>()
            + amount
    );
    assert!(pool_balances_after_deposit.lp_total_supply() > pool_balances_before.lp_total_supply());

    let note = TrustedNote::new(
        NoteInstructions {
            note_kind: NoteKind::Withdraw,
            attached_assets: vec![],
            asset_input: Some(FungibleAsset::new(pool_config.faucet_id, amount / 2)?),
            amount_input: amount,
            beneficiary: *user.miden_account.id(),
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: test_pool.miden_account.tag(),
        },
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, test_pool.miden_account.id(), note.clone())
        .await?;
    info!("--- Withdraw sent");
    zoro_pool
        .execute_notes(vec![note], HashMap::default(), HashMap::default())
        .await?;
    info!("--- Withdraw executed");

    test_utils
        .miden_client_mut()
        .consume_simple_notes(&user_id, 1)
        .await?;
    info!("--- User claim executed");

    // wait for one block
    tokio::time::sleep(Duration::from_millis(4100)).await;

    let user_balance_after_withdraw = user
        .miden_account
        .get_balance(&pool_config.faucet_id)
        .await?;
    let pool_balances_after_withdraw =
        *zoro_pool.pool_states().get(&pool_config.faucet_id).unwrap();

    info!("pool balances before deposit: {:?}", pool_balances_before);
    info!(
        "user balance after {}, before {}, diff {}, amount/2 {}",
        user_balance_after_withdraw,
        user_balance_after_deposit,
        user_balance_after_withdraw - user_balance_after_deposit,
        amount / 2
    );
    info!(
        "pool reserve after {}, before {}, diff {}, amount {}",
        pool_balances_after_withdraw.balances().reserve.to::<u64>(),
        pool_balances_after_deposit.balances().reserve.to::<u64>(),
        pool_balances_after_deposit.balances().reserve.to::<u64>()
            - pool_balances_after_withdraw.balances().reserve.to::<u64>(),
        amount
    );
    info!(
        "pool reserve  with slippage after {}, before {}, diff {}, amount {}",
        pool_balances_after_withdraw
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            - pool_balances_after_withdraw
                .balances()
                .reserve_with_slippage
                .to::<u64>(),
        amount
    );
    info!(
        "pool total liabilities after {}, before {}, diff {}, amount {}",
        pool_balances_after_withdraw
            .balances()
            .total_liabilities
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .total_liabilities
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .total_liabilities
            .to::<u64>()
            - pool_balances_after_withdraw
                .balances()
                .total_liabilities
                .to::<u64>(),
        amount
    );
    info!(
        "pool total lp supply after {}, before {}, diff {}, amount {}",
        pool_balances_after_withdraw.lp_total_supply(),
        pool_balances_after_deposit.lp_total_supply(),
        pool_balances_after_deposit.lp_total_supply()
            - pool_balances_after_withdraw.lp_total_supply(),
        amount
    );
    assert!(user_balance_after_withdraw >= user_balance_after_deposit + amount / 2);
    assert_eq!(
        pool_balances_after_withdraw.balances().reserve.to::<u64>(),
        pool_balances_after_deposit.balances().reserve.to::<u64>() - amount
    );
    assert_eq!(
        pool_balances_after_withdraw
            .balances()
            .reserve_with_slippage
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .reserve_with_slippage
            .to::<u64>()
            - amount
    );
    assert_eq!(
        pool_balances_after_withdraw
            .balances()
            .total_liabilities
            .to::<u64>(),
        pool_balances_after_deposit
            .balances()
            .total_liabilities
            .to::<u64>()
            - amount
    );
    assert!(
        pool_balances_after_withdraw.lp_total_supply()
            < pool_balances_after_deposit.lp_total_supply()
    );

    Ok(())
}
