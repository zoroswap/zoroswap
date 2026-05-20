use num_traits::pow::Pow;
use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use chrono::Utc;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Felt, Word, account::AccountId, asset::FungibleAsset, note::NoteType};
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

fn format_word_to_masm_string(word: Word) -> String {
    format!("push.{}.{}.{}.{}", word[3], word[2], word[1], word[0])
}

#[tokio::test]
async fn note_arguments_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let user = test_utils.get_accounts(1).await?.first().unwrap().clone();

    let serial_number = TrustedNote::random_word()?;

    let amount_out = 9_999_u64;
    let liabilities_0 = 123_u64;
    let reserve_0 = 456_u64;
    let reserve_with_slippage_0 = 478_u64;
    let arguments_word_0: Word = [
        Felt::new(amount_out),
        Felt::new(liabilities_0),
        Felt::new(reserve_0),
        Felt::new(reserve_with_slippage_0),
    ]
    .into();

    let test_note_code = format!(
        "use zoro_miden::note::common\n\
            use common::AMOUNT_OUT\n\
            use common::LIABILITIES_0\n\
            use common::RESERVE_0\n\
            use common::RESERVE_WITH_SLIPPAGE_0\n\
            \n\
            const ERR_A_AMOUNT_OUT = \"Issue with amount_out argument in memory\"\n\
            const ERR_A_LIABILITIES_0 = \"Issue with liabilities_0 argument in memory\"\n\
            const ERR_A_RESERVE_0 = \"Issue with reserve_0 argument in memory\"\n\
            const ERR_A_RESERVE_WS_0 = \"Issue with reserve_with_slippage_0 argument in memory\"\n\
            const ERR_G_AMOUNT_OUT = \"Issue with amount_out getter\"\n\
            const ERR_G_LIABILITIES_0 = \"Issue with liabilities_0 getter\"\n\
            const ERR_G_RESERVE_0 = \"Issue with reserve_0 getter\"\n\
            const ERR_G_RESERVE_WS_0 = \"Issue with reserve_with_slippage_0 getter\"\n\
            const ERR_G_POOL_0_STATE = \"Issue with pool_0_state getter\"\n\
            \n\
            @note_script\n\
            pub proc main\n\
                exec.common::store_arguments_from_stack_to_memory\n\
                mem_load.AMOUNT_OUT push.{amount_out} assert_eq.err=ERR_A_AMOUNT_OUT\n\
                mem_load.LIABILITIES_0 push.{liabilities_0} assert_eq.err=ERR_A_LIABILITIES_0\n\
                mem_load.RESERVE_0 push.{reserve_0} assert_eq.err=ERR_A_RESERVE_0\n\
                mem_load.RESERVE_WITH_SLIPPAGE_0 push.{reserve_with_slippage_0} assert_eq.err=ERR_A_RESERVE_WS_0\n\
                exec.common::get_amount_out_argument push.{amount_out} assert_eq.err=ERR_G_AMOUNT_OUT\n\
                exec.common::get_pool_0_liabilities push.{liabilities_0} assert_eq.err=ERR_G_LIABILITIES_0\n\
                exec.common::get_pool_0_reserve push.{reserve_0} assert_eq.err=ERR_G_RESERVE_0\n\
                exec.common::get_pool_0_reserve_with_slippage push.{reserve_with_slippage_0} assert_eq.err=ERR_G_RESERVE_WS_0\n\
                padw exec.common::get_pool_0_state push.{liabilities_0} debug.stack.4 assert_eq.err=ERR_G_POOL_0_STATE\n\
                push.{reserve_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
                push.{reserve_with_slippage_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
            end"
    );

    let code_builder =
        link_all_libraries(test_utils.miden_client().client().code_builder().clone())?;
    let test_note_script = code_builder.compile_note_script(test_note_code)?;

    let recipient = NoteRecipient::new(serial_number, test_note_script, NoteStorage::new(vec![])?);
    let metadata = NoteMetadata::new(user.miden_account.id().clone(), NoteType::Public);
    let assets = NoteAssets::new(vec![])?;
    let test_note = Note::new(assets, metadata, recipient);

    test_utils
        .miden_client_mut()
        .send_note_untrusted(user.miden_account.id(), test_note.clone())
        .await?;

    let consume_transaction_request = TransactionRequestBuilder::new()
        .input_notes(vec![(test_note.clone(), Some(arguments_word_0))])
        .build()?;
    let _tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), consume_transaction_request)
        .await?;

    Ok(())
}

#[tokio::test]
async fn note_storage_default_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let user = test_utils.get_accounts(1).await?.first().unwrap().clone();

    let serial_number = TrustedNote::random_word()?;

    //// NOTE STORAGE
    //   @todo move storage creation into note.rs
    //
    // ASSET
    //   @todo user proper asset createion and asset compression
    //
    let faucet_id = test_utils.faucet_1;
    let faucet_cb_enabled = Felt::new(0);
    let amount = 12_345;
    let asset_compact: Word = [
        faucet_id.suffix(),
        faucet_id.prefix().into(),
        faucet_cb_enabled,
        Felt::new(amount),
    ]
    .into();
    // METADATA
    let deadline = Utc::now().timestamp_millis() as u64 + 5000;
    let p2id_tag = 45677789;
    let metadata_item_2 = amount * 1000 / 9500;
    let metadata_storage: Word = [
        Felt::new(deadline),
        Felt::new(p2id_tag),
        Felt::new(metadata_item_2),
        Felt::ZERO,
    ]
    .into();
    // BENEFICIARY
    let beneficiary_id = test_utils.user_1;
    let beneficiary: Word = [
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        Felt::ZERO,
        Felt::ZERO,
    ]
    .into();

    let note_storage = NoteStorage::new(vec![
        asset_compact[0],
        asset_compact[1],
        asset_compact[2],
        asset_compact[3],
        metadata_storage[0],
        metadata_storage[1],
        metadata_storage[2],
        metadata_storage[3],
        beneficiary[0],
        beneficiary[1],
        beneficiary[2],
        beneficiary[3],
    ])?;

    //@todo write a base test note with all generic imports
    let test_note_code = format!(
        "use zoro_miden::note::common\n\
             use common::AccountId\n\
             use common::bool\n\
             use common::DEFAULT_NUMBER_OF_STORAGE_ITEMS\n\
             #use common::STORAGE_POINTER\n\
             use common::STORAGE_EXPECTED_ASSET_WORD\n\
             use common::STORAGE_METADATA_WORD\n\
             use common::STORAGE_BENEFICIARY_WORD\n\
             \n\
             const ERR_ASSET = \"Issue with asset in note storage\"\n\
             const ERR_METADATA = \"Issue with metadata in note storage\"\n\
             const ERR_BENEFICIARY = \"Issue with beneficiary in note storage\"\n\
             const ERR_G_ASSET = \"Issue with expected asset getter\"\n\
             const ERR_G_METADATA = \"Issue with expected metadata getter\"\n\
             const ERR_G_BENEFICIARY = \"Issue with expected beneficiary getter\"\n\
             const ERR_G_DEADLINE = \"Issue with expected deadline getter\"\n\
             const ERR_G_P2ID_TAG = \"Issue with expected p2id tag getter\"\n\
             const ERR_G_METADATA_ITEM_2 = \"Issue with expected metadata item 2 getter\"\n\
             const ERR_G_METADATA_ITEM_3 = \"Issue with expected metadata item 3 getter\"\n\
             \n\
             \n\
             \n\
             @note_script\n\
             pub proc main\n\
                 push.DEFAULT_NUMBER_OF_STORAGE_ITEMS exec.common::store_storage_to_memory\n\
                 drop padw mem_loadw_le.STORAGE_EXPECTED_ASSET_WORD {}\n\
                 assert_eqw.err=ERR_ASSET dropw dropw\n\
                 padw mem_loadw_le.STORAGE_METADATA_WORD {}\n\
                 assert_eqw.err=ERR_METADATA dropw dropw\n\
                 padw mem_loadw_le.STORAGE_BENEFICIARY_WORD {}\n\
                 assert_eqw.err=ERR_BENEFICIARY dropw dropw\n\
                 padw exec.common::get_expected_asset padw mem_loadw_le.STORAGE_EXPECTED_ASSET_WORD\n\
                 assert_eqw.err=ERR_G_ASSET dropw dropw\n\
                 padw exec.common::get_metadata padw mem_loadw_le.STORAGE_METADATA_WORD\n\
                 assert_eqw.err=ERR_G_METADATA dropw dropw\n\
                 exec.common::get_deadline push.{deadline} assert_eq.err=ERR_G_DEADLINE\n\
                 exec.common::get_p2id_tag push.{p2id_tag} assert_eq.err=ERR_G_P2ID_TAG\n\
                 exec.common::get_beneficiary_id push.{} assert_eq.err=ERR_G_BENEFICIARY\n\
                 push.{} assert_eq.err=ERR_G_BENEFICIARY\n\
                 exec.common::get_metadata_item_2 push.{metadata_item_2} assert_eq.err=ERR_G_METADATA_ITEM_2\n\
                 exec.common::get_metadata_item_3 push.0 assert_eq.err=ERR_G_METADATA_ITEM_3\n\
             end",
        format_word_to_masm_string(asset_compact),
        format_word_to_masm_string(metadata_storage),
        format_word_to_masm_string(beneficiary),
        beneficiary_id.suffix(),
        beneficiary_id.prefix(),
    );

    let code_builder =
        link_all_libraries(test_utils.miden_client().client().code_builder().clone())?;
    let test_note_script = code_builder.compile_note_script(test_note_code)?;

    let recipient = NoteRecipient::new(serial_number, test_note_script, note_storage.clone());

    let metadata = NoteMetadata::new(user.miden_account.id().clone(), NoteType::Public);
    let assets = NoteAssets::new(vec![])?;
    let test_note = Note::new(assets, metadata, recipient);

    test_utils
        .miden_client_mut()
        .send_note_untrusted(user.miden_account.id(), test_note.clone())
        .await?;

    // consume as unauthenticated note @note move consumption into client (or test utils) with args/advice map
    let transaction_request =
        TransactionRequestBuilder::new().build_consume_notes(vec![test_note.clone()])?;
    let tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), transaction_request)
        .await?;

    Ok(())
}

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
