use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use miden_client::{account::AccountId, asset::FungibleAsset};
use tracing::info;
use zoro_miden::{
    note::{
        DepositInstructions, NoteInstructions, SwapInstructions, TrustedNote, WithdrawInstructions,
    },
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
    let user = user.first().unwrap();
    let user_id = *user.miden_account.id();
    let deposit_note = TrustedNote::new(
        NoteInstructions::Deposit(DepositInstructions {
            asset_in: FungibleAsset::new(pool_config.faucet_id, amount)?,
            min_lp_amount_out: amount - 100,
            creator: user_id,
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: pool.miden_account.tag(),
        }),
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, pool.miden_account.id(), deposit_note.clone())
        .await?;
    zoro_pool
        .execute_notes(vec![deposit_note], HashMap::default())
        .await?;
    Ok(())
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
    let amount = 10_000;
    let user = test_utils
        .get_funded_accounts(1, vec![(pool_config_token0.faucet_id, amount, amount * 10)])
        .await?;
    info!("--- User ready");
    let user = user.first().unwrap();
    let user_id = *user.miden_account.id();
    let mut prices: HashMap<AccountId, PriceData> = HashMap::with_capacity(2);
    prices.insert(pool_config_token0.faucet_id, PriceData::new_at_now(1));
    prices.insert(pool_config_token1.faucet_id, PriceData::new_at_now(1));
    let note = TrustedNote::new(
        NoteInstructions::Swap(SwapInstructions {
            asset_in: FungibleAsset::new(pool_config_token0.faucet_id, amount)?,
            min_asset_out: FungibleAsset::new(pool_config_token1.faucet_id, amount / 2)?,
            creator: *user.miden_account.id(),
            beneficiary: None,
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: test_pool.miden_account.tag(),
        }),
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, test_pool.miden_account.id(), note.clone())
        .await?;
    info!("--- Swap sent");
    zoro_pool.execute_notes(vec![note], prices).await?;
    info!("--- Swap executed");
    Ok(())
}

#[tokio::test]
async fn executing_deposit_withdraw() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let PoolWithMeta {
        zoro_pool,
        test_pool,
    } = &mut test_utils.get_funded_pools(1).await?[..][0];
    info!("--- Pool ready");
    let pool_config_token0 = test_pool.pool_configs[..][0];
    let amount = 10_000;
    let user = test_utils
        .get_funded_accounts(1, vec![(pool_config_token0.faucet_id, amount, amount * 10)])
        .await?;
    info!("--- User ready");
    let user = user.first().unwrap();
    let user_id = *user.miden_account.id();
    let mut prices: HashMap<AccountId, PriceData> = HashMap::with_capacity(2);
    prices.insert(pool_config_token0.faucet_id, PriceData::new_at_now(1));

    let deposit_note = TrustedNote::new(
        NoteInstructions::Deposit(DepositInstructions {
            asset_in: FungibleAsset::new(pool_config_token0.faucet_id, amount)?,
            min_lp_amount_out: amount - 100,
            creator: user_id,
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64,
            p2id_tag: user.miden_account.tag(),
            pool_tag: zoro_pool.miden_account().tag(),
        }),
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
        .execute_notes(vec![deposit_note], HashMap::default())
        .await?;
    info!("--- Deposit executed");

    let note = TrustedNote::new(
        NoteInstructions::Withdraw(WithdrawInstructions {
            min_asset_out: FungibleAsset::new(pool_config_token0.faucet_id, amount / 2)?,
            lp_amount_in: amount,
            creator: *user.miden_account.id(),
            note_type: miden_client::note::NoteType::Public,
            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
            p2id_tag: user.miden_account.tag(),
            pool_tag: test_pool.miden_account.tag(),
        }),
        test_utils.miden_client().client().code_builder(),
    )?;
    test_utils
        .miden_client_mut()
        .send_note(&user_id, test_pool.miden_account.id(), note.clone())
        .await?;
    info!("--- Withdraw sent");
    zoro_pool.execute_notes(vec![note], prices).await?;
    info!("--- Withdraw executed");

    test_utils
        .miden_client_mut()
        .consume_simple_notes(&user_id, 1)
        .await?;
    info!("--- User claim executed");

    Ok(())
}
