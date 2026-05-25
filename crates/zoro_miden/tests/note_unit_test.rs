use miden_protocol::word::LexicographicWord;
use num_traits::pow::Pow;
use std::{collections::HashMap, time::Duration};

use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Felt, Word, account::AccountId, asset::FungibleAsset, note::NoteType};
use tracing::info;
use zoro_miden::{
    assembly_utils::link_all_libraries,
    asset_utils::{asset_to_word, word_to_asset},
    note::{
        DepositInstructions, NoteInstructions, NoteStorageBuilder, SwapInstructions, TrustedNote,
        WithdrawInstructions,
    },
    pool::ZoroPool,
    price::PriceData,
    test_utils::{PoolWithMeta, TestUtils, format_word_to_masm_string},
};

use miden_protocol::note::{Note, NoteAssets, NoteMetadata, NoteRecipient, NoteStorage};

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
                padw exec.common::get_pool_0_state push.{liabilities_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
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
async fn note_arguments_advicemap_unit_test() -> Result<()> {
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

    let liabilities_1 = 234_u64;
    let reserve_1 = 567_u64;
    let reserve_with_slippage_1 = 789_u64;
    let arguments_word_1: Word = [
        Felt::ZERO,
        Felt::new(liabilities_1),
        Felt::new(reserve_1),
        Felt::new(reserve_with_slippage_1),
    ]
    .into();

    let test_note_code = format!(
        "use zoro_miden::note::common\n\
            use common::AMOUNT_OUT\n\
            use common::LIABILITIES_0\n\
            use common::RESERVE_0\n\
            use common::RESERVE_WITH_SLIPPAGE_0\n\
            use common::EMPTY_ARGUMENT_1\n\
            use common::LIABILITIES_1\n\
            use common::RESERVE_1\n\
            use common::RESERVE_WITH_SLIPPAGE_1\n\
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
            const ERR_A_EMPTY_ARGUMENT_1 = \"Issue with empty_argument_1 argument in memory\"\n\
            const ERR_A_LIABILITIES_1 = \"Issue with liabilities_1 argument in memory\"\n\
            const ERR_A_RESERVE_1 = \"Issue with reserve_1 argument in memory\"\n\
            const ERR_A_RESERVE_WS_1 = \"Issue with reserve_with_slippage_1 argument in memory\"\n\
            const ERR_G_LIABILITIES_1 = \"Issue with liabilities_1 getter\"\n\
            const ERR_G_RESERVE_1 = \"Issue with reserve_1 getter\"\n\
            const ERR_G_RESERVE_WS_1 = \"Issue with reserve_with_slippage_1 getter\"\n\
            const ERR_G_POOL_1_STATE = \"Issue with pool_1_state getter\"\n\
            \n\
            @note_script\n\
            pub proc main\n\
                exec.common::store_arguments_from_advicemap_to_memory\n\
                mem_load.AMOUNT_OUT push.{amount_out} assert_eq.err=ERR_A_AMOUNT_OUT\n\
                mem_load.LIABILITIES_0 push.{liabilities_0} assert_eq.err=ERR_A_LIABILITIES_0\n\
                mem_load.RESERVE_0 push.{reserve_0} assert_eq.err=ERR_A_RESERVE_0\n\
                mem_load.RESERVE_WITH_SLIPPAGE_0 push.{reserve_with_slippage_0} assert_eq.err=ERR_A_RESERVE_WS_0\n\
                exec.common::get_amount_out_argument push.{amount_out} assert_eq.err=ERR_G_AMOUNT_OUT\n\
                exec.common::get_pool_0_liabilities push.{liabilities_0} assert_eq.err=ERR_G_LIABILITIES_0\n\
                exec.common::get_pool_0_reserve push.{reserve_0} assert_eq.err=ERR_G_RESERVE_0\n\
                exec.common::get_pool_0_reserve_with_slippage push.{reserve_with_slippage_0} assert_eq.err=ERR_G_RESERVE_WS_0\n\
                padw exec.common::get_pool_0_state push.{liabilities_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
                push.{reserve_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
                push.{reserve_with_slippage_0} assert_eq.err=ERR_G_POOL_0_STATE\n\
                mem_load.EMPTY_ARGUMENT_1 push.0 assert_eq.err=ERR_A_EMPTY_ARGUMENT_1\n\
                mem_load.LIABILITIES_1 push.{liabilities_1} assert_eq.err=ERR_A_LIABILITIES_1\n\
                mem_load.RESERVE_1 push.{reserve_1} assert_eq.err=ERR_A_RESERVE_1\n\
                mem_load.RESERVE_WITH_SLIPPAGE_1 push.{reserve_with_slippage_1} assert_eq.err=ERR_A_RESERVE_WS_1\n\
                exec.common::get_pool_1_liabilities push.{liabilities_1} assert_eq.err=ERR_G_LIABILITIES_1\n\
                exec.common::get_pool_1_reserve push.{reserve_1} assert_eq.err=ERR_G_RESERVE_1\n\
                exec.common::get_pool_1_reserve_with_slippage push.{reserve_with_slippage_1} assert_eq.err=ERR_G_RESERVE_WS_1\n\
                padw exec.common::get_pool_1_state push.{liabilities_1} assert_eq.err=ERR_G_POOL_1_STATE\n\
                push.{reserve_1} assert_eq.err=ERR_G_POOL_1_STATE\n\
                push.{reserve_with_slippage_1} assert_eq.err=ERR_G_POOL_1_STATE\n\
                dropw\n\
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

    let advice_map = [(
        serial_number.into(),
        arguments_word_0
            .iter()
            .copied()
            .chain(arguments_word_1.iter().copied())
            .collect::<Vec<Felt>>(),
    )];
    let consume_transaction_request = TransactionRequestBuilder::new()
        .extend_advice_map(advice_map)
        .input_notes(vec![(test_note.clone(), None)])
        .build()?;
    let _tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), consume_transaction_request)
        .await?;

    Ok(())
}

#[tokio::test]
async fn note_arguments_advicemap_wrong_key_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let user = test_utils.get_accounts(1).await?.first().unwrap().clone();

    let serial_number = TrustedNote::random_word()?;

    let test_note_code = format!(
        "use zoro_miden::note::common\n\
            const ERR_HAS_ARGUMENTS_FROM_ADVICE_MAP = \"Issue with has_arguments_from_advicemap: key found where no arguments expected\"\n\
            \n\
            @note_script\n\
            pub proc main\n\
                exec.common::has_arguments_from_advicemap\n\
                assertz.err=ERR_HAS_ARGUMENTS_FROM_ADVICE_MAP\n\
                dropw\n\
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
        .input_notes(vec![(test_note.clone(), None)])
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

    let note_storage = NoteStorageBuilder::new(beneficiary_id)
        .with_asset_compact(asset_compact)
        .with_metadata(metadata_storage)
        .build()?;

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
        format_word_to_masm_string(
            [
                beneficiary_id.suffix(),
                beneficiary_id.prefix().into(),
                Felt::ZERO,
                Felt::ZERO,
            ]
            .into(),
        ),
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

#[tokio::test]
async fn respwawn_simple_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let faucets = test_utils.get_faucets(2).await?;
    let faucet0 = faucets.first().unwrap();
    let faucet1 = faucets.last().unwrap();

    let amount0 = 10_000;
    let amount1 = 20_000;
    let user = test_utils
        .get_funded_accounts(
            1,
            vec![
                (*faucet0.miden_account.id(), amount0, amount0 * 10),
                (*faucet1.miden_account.id(), amount1, amount1 * 50),
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
        FungibleAsset::new(*faucet0.miden_account.id(), amount0)?.into(),
        FungibleAsset::new(*faucet1.miden_account.id(), amount1)?.into(),
    ])?;

    //@todo write a base test note with all generic imports
    let respawn_test_note_code = format!(
        "use zoro_miden::note::common\n\
        use zoro_miden::note::respawn\n\
        use common::DEFAULT_NUMBER_OF_STORAGE_ITEMS\n\
        use common::STORAGE_POINTER\n\
        \n\
        @note_script\n\
        pub proc main\n\
            exec.respawn::get_note_type_and_tag_from_active_note
            # => [note_type, tag]
            exec.common::store_all_active_note_storage_items_to_output_storage_memory swap drop
            # => [num_storage_items, note_type, tag]
            exec.common::store_all_active_note_assets_to_output_assets_memory swap drop
            # => [num_assets, num_storage_items, storage_ptr, note_type, tag]
 
            exec.respawn::recreate_note
            # => [note_idx]
            drop
        end",
    );
    let code_builder =
        link_all_libraries(test_utils.miden_client().client().code_builder().clone())?;
    let respawn_test_note_script = code_builder.compile_note_script(respawn_test_note_code)?;

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
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
    ])?;

    let recipient = NoteRecipient::new(
        serial_number,
        respawn_test_note_script.clone(),
        note_storage.clone(),
    );

    let metadata = NoteMetadata::new(user_id, NoteType::Public);
    let respawn_note = Note::new(assets.clone(), metadata, recipient.clone());

    test_utils
        .miden_client_mut()
        .send_note_untrusted(&user_id, respawn_note.clone())
        .await?;

    let new_serial_number: Word = [
        serial_number[0],
        serial_number[1],
        serial_number[2],
        serial_number[3] + Felt::new(1),
    ]
    .into();
    let respawned_recipient =
        NoteRecipient::new(new_serial_number, respawn_test_note_script, note_storage);

    let metadata = NoteMetadata::new(user_id, NoteType::Public);
    let respawned_note = Note::new(assets, metadata, respawned_recipient.clone());

    // consume as unauthenticated note @note move consumption into client (or test utils) with args/advice map
    let transaction_request = TransactionRequestBuilder::new()
        .input_notes(vec![(respawn_note.clone(), None)])
        .expected_output_recipients(vec![respawned_recipient.clone()])
        .build()?;
    let tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), transaction_request)
        .await?;

    let user_balance0_after_sent = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_after_sent = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;

    assert_eq!(user_balance0_after_sent, user_balance0_at_start - amount0);
    assert_eq!(user_balance1_after_sent, user_balance1_at_start - amount1);

    Ok(())
}

#[tokio::test]
async fn respwawn_reclaim_simple_unit_test() -> Result<()> {
    let mut test_utils = TestUtils::from_cache().await?;
    let faucets = test_utils.get_faucets(2).await?;
    let faucet0 = faucets.first().unwrap();
    let faucet1 = faucets.last().unwrap();

    let amount0 = 10_000;
    let amount1 = 20_000;
    let user = test_utils
        .get_funded_accounts(
            1,
            vec![
                (*faucet0.miden_account.id(), amount0, amount0 * 10),
                (*faucet1.miden_account.id(), amount1, amount1 * 50),
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
        FungibleAsset::new(*faucet0.miden_account.id(), amount0)?.into(),
        FungibleAsset::new(*faucet1.miden_account.id(), amount1)?.into(),
    ])?;

    //@todo write a base test note with all generic imports
    let respawn_test_note_code = format!(
        "use zoro_miden::note::common\n\
        use zoro_miden::note::respawn\n\
        use zoro_miden::note::reclaim\n\
        use common::ARGUMENTS_WORD_0\n\
        use common::DEFAULT_NUMBER_OF_STORAGE_ITEMS\n\
        \n\
        @note_script\n\
        pub proc main\n\
            exec.common::store_arguments_from_stack_to_memory
            push.DEFAULT_NUMBER_OF_STORAGE_ITEMS exec.common::store_storage_to_memory
            mem_load.ARGUMENTS_WORD_0 neq.0
            if.true 
                exec.reclaim::reclaim_note
            else
                exec.respawn::get_note_type_and_tag_from_active_note
                # => [note_type, tag]
                exec.common::store_all_active_note_storage_items_to_output_storage_memory swap drop
                # => [num_storage_items, note_type, tag]
                exec.common::store_all_active_note_assets_to_output_assets_memory swap drop
                # => [num_assets, num_storage_items, note_type, tag]
                exec.respawn::recreate_note
                # => [note_idx]
                drop
            end
        end",
    );
    let code_builder =
        link_all_libraries(test_utils.miden_client().client().code_builder().clone())?;
    let respawn_test_note_script = code_builder.compile_note_script(respawn_test_note_code)?;

    let serial_number = TrustedNote::random_word()?;

    let note_storage = NoteStorageBuilder::new(user_id).build()?;

    let recipient = NoteRecipient::new(
        serial_number,
        respawn_test_note_script.clone(),
        note_storage.clone(),
    );

    let metadata = NoteMetadata::new(user_id, NoteType::Public);
    let respawn_note = Note::new(assets.clone(), metadata, recipient.clone());

    test_utils
        .miden_client_mut()
        .send_note_untrusted(&user_id, respawn_note.clone())
        .await?;

    let new_serial_number: Word = [
        serial_number[0],
        serial_number[1],
        serial_number[2],
        serial_number[3] + Felt::new(1),
    ]
    .into();
    let respawned_recipient =
        NoteRecipient::new(new_serial_number, respawn_test_note_script, note_storage);

    let metadata = NoteMetadata::new(user_id, NoteType::Public);
    let respawned_note = Note::new(assets, metadata, respawned_recipient.clone());

    // consume as unauthenticated note @note move consumption into client (or test utils) with args/advice map
    let transaction_request = TransactionRequestBuilder::new()
        .input_notes(vec![(respawn_note.clone(), None)])
        .expected_output_recipients(vec![respawned_recipient.clone()])
        .build()?;
    let tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), transaction_request)
        .await?;

    let user_balance0_after_sent = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_after_sent = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;

    assert_eq!(user_balance0_after_sent, user_balance0_at_start - amount0);
    assert_eq!(user_balance1_after_sent, user_balance1_at_start - amount1);

    let reclaim_transaction_request = TransactionRequestBuilder::new()
        .input_notes(vec![(
            respawned_note.clone(),
            Some([Felt::new(1), Felt::ZERO, Felt::ZERO, Felt::ZERO].into()),
        )])
        .build()?;
    let tx_id = test_utils
        .miden_client_mut()
        .client_mut()
        .submit_new_transaction(user.miden_account.id().clone(), reclaim_transaction_request)
        .await?;

    tokio::time::sleep(Duration::from_millis(4100)).await;
    test_utils.miden_client_mut().sync_state().await?;

    let user_balance0_after_reclaim = user
        .miden_account
        .get_balance(faucet0.miden_account.id())
        .await?;
    let user_balance1_after_reclaim = user
        .miden_account
        .get_balance(faucet1.miden_account.id())
        .await?;

    assert_eq!(user_balance0_after_reclaim, user_balance0_at_start);
    assert_eq!(user_balance1_after_reclaim, user_balance1_at_start);

    Ok(())
}
