use num_traits::pow::Pow;
use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use chrono::Utc;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Felt, account::AccountId, asset::FungibleAsset, note::NoteType};
use tracing::info;
use zoro_miden::{
    assembly_utils::link_all_libraries,
    note::{
        DepositInstructions, NoteInstructions, SwapInstructions, TrustedNote, WithdrawInstructions,
    },
    pool::ZoroPool,
    price::PriceData,
    test_utils::{PoolWithMeta, TestUtils},
};

use miden_protocol::note::{Note, NoteAssets, NoteMetadata, NoteRecipient, NoteStorage};

#[tokio::test]
async fn reclaim_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let faucets = test_utils.get_faucets(2).await?;
    let faucet0 = faucets.first().unwrap();
    let faucet1 = faucets.last().unwrap();

    let amount = 10_000;
    let user = test_utils
        .get_funded_accounts(
            1,
            vec![
                (*faucet0.miden_account.id(), amount, amount * 10),
                (*faucet1.miden_account.id(), amount, amount * 50),
            ],
        )
        .await?;
    let mut user = user.first().unwrap().clone();
    let user_id = *user.miden_account.id();
    let user2_id = test_utils.user_2.clone();

    let user_balance0_at_start = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_at_start = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;

    let assets = NoteAssets::new(vec![
        FungibleAsset::new(*faucet0.miden_account.id(), amount)?.into(),
        FungibleAsset::new(*faucet1.miden_account.id(), amount)?.into(),
    ])?;

    let reclaim_note_script = TrustedNote::get_note_script(
        test_utils.miden_client().client().code_builder(),
        "TRADER_DEPOSIT.masm",
    )?;
    let serial_number = TrustedNote::random_word()?;

    let note_storage = NoteStorage::new(vec![
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        user_id.suffix(),
        user_id.prefix().into(),
        Felt::ZERO,
        Felt::ZERO,
    ])?;

    let recipient = NoteRecipient::new(serial_number, reclaim_note_script, note_storage);

    let metadata = NoteMetadata::new(user_id, NoteType::Public);
    let reclaim_note = Note::new(assets, metadata, recipient);

    test_utils
        .miden_client_mut()
        .send_note_untrusted(&user_id, reclaim_note.clone())
        .await?;

    // wait for one block
    tokio::time::sleep(Duration::from_millis(4100)).await;
    test_utils.miden_client_mut().sync_state().await?;

    let user_balance0_after_sent = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_after_sent = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;

    assert_eq!(user_balance0_after_sent, user_balance0_at_start - amount);
    assert_eq!(user_balance1_after_sent, user_balance1_at_start - amount);

    /////  consume notes: move to test_utils with args / advice map
    let transaction_request =
        TransactionRequestBuilder::new().build_consume_notes(vec![reclaim_note.clone()])?;
    let tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user_id.clone(), transaction_request)
        .await?;

    tokio::time::sleep(Duration::from_millis(4100)).await;
    test_utils.miden_client_mut().sync_state().await?;
    ///
    let user_balance0_after_sync = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_after_sync = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;
    assert_eq!(user_balance0_after_sync, user_balance0_at_start);
    assert_eq!(user_balance1_after_sync, user_balance1_at_start);

    Ok(())
}
