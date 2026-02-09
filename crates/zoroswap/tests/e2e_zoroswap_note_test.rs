use anyhow::Result;
use miden_client::{
    Felt, Word,
    asset::FungibleAsset,
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    rpc::{Endpoint, GrpcClient},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use std::sync::Arc;
use zoro_miden_client::{create_p2id_note, setup_accounts_and_faucets};
use zoroswap::{create_zoroswap_note, fetch_vault_for_account_from_chain};

#[tokio::test]
async fn zero_note_create_consume_with_refund_test() -> Result<()> {
    // Reset the store and initialize the client.
    // delete_keystore_and_store().await;

    // Initialize client
    let endpoint: Endpoint = Endpoint::localhost();
    // let endpoint: Endpoint = Endpoint::testnet();

    let timeout_ms = 10_000;
    let rpc_api = Arc::new(GrpcClient::new(&endpoint, timeout_ms));

    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .filesystem_keystore("../../keystore")
        .sqlite_store("testing_store.sqlite3".into())
        .in_debug_mode(true.into())
        .build()
        .await?;

    let sync_summary = client.sync_state().await.unwrap();
    println!("Latest block: {}", sync_summary.block_num);

    let keystore = FilesystemKeyStore::new("../../keystore".into()).unwrap();

    // let balances = vec![
    //     vec![100, 0], // For account[0] => Alice
    //     vec![0, 100], // For account[1] => Bob
    // ];
    let balances = vec![vec![100, 0], vec![0, 100]];
    let (accounts, faucets) =
        setup_accounts_and_faucets(&mut client, keystore, 2, 2, balances).await?;

    // rename for clarity
    let alice_account = accounts[0].clone();
    let bob_account = accounts[1].clone();
    let faucet_a = faucets[0].clone();
    let faucet_b = faucets[1].clone();

    // -------------------------------------------------------------------------
    // STEP 1: Create SWAPP note
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Create ZERO note");

    // // offered asset amount
    let amount_a = 50;
    let asset_a = FungibleAsset::new(faucet_a.id(), amount_a).unwrap();

    // // requested asset amount
    let amount_b = 50;
    let asset_b = FungibleAsset::new(faucet_b.id(), amount_b).unwrap();

    let zero_serial_num = client.rng().draw_word();

    let requested_asset_word: Word = asset_b.into();
    // let inputs = vec![Felt::new(128), Felt::new(33)];

    let p2id_tag = NoteTag::from_account_id(alice_account.id());

    let beneficiary_id = alice_account.id();
    let inputs = vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(123),  // deadline
        p2id_tag.into(), // p2id tag
        Felt::new(0),
        Felt::new(0),
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        alice_account.id().suffix().into(),
        alice_account.id().prefix().into(),
    ];

    let assets = vec![asset_a.into()];
    let zero_note = create_zoroswap_note(
        inputs.into(),
        assets.into(),
        alice_account.id(),
        zero_serial_num,
        NoteTag::LocalAny(123),
        NoteType::Private,
    )
    .unwrap();

    let zero_tag = zero_note.metadata().tag();

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(zero_note.clone())])
        .build()
        .unwrap();
    let tx_id = client
        .submit_new_transaction(alice_account.id(), note_req)
        .await
        .unwrap();

    println!("View transaction on MidenScan: https://testnet.midenscan.com/tx/{tx_id:?}");

    client.sync_state().await?;

    let zero_note_id = zero_note.id();

    // Time from after ZOROSWAP creation
    // let start_time = Instant::now();

    // let _ = get_note_by_tag(&mut client, zero_tag, zero_note_id).await;

    // -------------------------------------------------------------------------
    // STEP 2: Partial Consume SWAPP note
    // -------------------------------------------------------------------------

    println!("\n[STEP 2] Execute ZERO note");

    let amount_out = Felt::new(49);
    let pool_0_state = [Felt::new(556), Felt::new(556), Felt::new(556), Felt::new(0)];
    let pool_1_state = [
        Felt::new(1274),
        Felt::new(1274),
        Felt::new(1274),
        Felt::new(0),
    ];

    let zero_note_args = [Felt::new(0), Felt::new(0), Felt::new(0), amount_out];
    let p2id_note_asset = asset_a.clone();
    let p2id_serial_num = [
        zero_serial_num[0],
        zero_serial_num[1],
        zero_serial_num[2],
        Felt::new(zero_serial_num[3].as_int() + 1),
    ];

    let p2id_note = create_p2id_note(
        bob_account.id(),
        alice_account.id(),
        vec![p2id_note_asset.into()],
        NoteType::Public,
        Felt::new(0),
        p2id_serial_num.into(),
    )
    .unwrap();

    let consume_zero_req = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(zero_note, Some(zero_note_args.into()))])
        .expected_output_recipients(vec![p2id_note.recipient().clone()])
        .build()
        .unwrap();

    let tx_id = client
        .submit_new_transaction(bob_account.id(), consume_zero_req)
        .await
        .unwrap();

    println!("Consumed Note Tx on MidenScan: https://testnet.midenscan.com/tx/{tx_id:?}");

    // Stop timing

    println!("ZERO note consumed");

    Ok(())
}

#[tokio::test]
async fn zero_note_create_consume_test() -> Result<()> {
    // Reset the store and initialize the client.
    //delete_keystore_and_store().await;

    // Initialize client
    let endpoint: Endpoint = Endpoint::localhost();
    // let endpoint: Endpoint = Endpoint::testnet();

    let timeout_ms = 10_000;
    let rpc_api = Arc::new(GrpcClient::new(&endpoint, timeout_ms));

    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .filesystem_keystore("../../keystore")
        .in_debug_mode(true.into())
        .build()
        .await?;

    let sync_summary = client.sync_state().await.unwrap();
    println!("Latest block: {}", sync_summary.block_num);

    let keystore = FilesystemKeyStore::new("../../keystore".into()).unwrap();

    // let balances = vec![
    //     vec![100, 0], // For account[0] => Alice
    //     vec![0, 100], // For account[1] => Bob
    // ];
    let balances = vec![vec![100, 0], vec![0, 100]];
    let (accounts, faucets) =
        setup_accounts_and_faucets(&mut client, keystore, 2, 2, balances).await?;

    // rename for clarity
    let alice_account = accounts[0].clone();
    let real_balances = fetch_vault_for_account_from_chain(&mut client, alice_account.id()).await?;

    println!("New account alice vault: {:?}", real_balances);

    let bob_account = accounts[1].clone();
    let faucet_a = faucets[0].clone();
    let faucet_b = faucets[1].clone();

    // -------------------------------------------------------------------------
    // STEP 1: Create SWAPP note
    // -------------------------------------------------------------------------
    println!("\n[STEP 1] Create ZERO note");

    // // offered asset amount
    let amount_a = 50;
    let asset_a = FungibleAsset::new(faucet_a.id(), amount_a).unwrap();

    // // requested asset amount
    let amount_b = 50;
    let asset_b = FungibleAsset::new(faucet_b.id(), amount_b).unwrap();

    let zero_serial_num = client.rng().draw_word();

    let requested_asset_word: Word = asset_b.into();
    // let inputs = vec![Felt::new(128), Felt::new(33)];

    let p2id_tag = NoteTag::from_account_id(alice_account.id());

    let beneficiary_id = alice_account.id();
    let inputs = vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(123),  // deadline
        p2id_tag.into(), // p2id tag
        Felt::new(0),
        Felt::new(0),
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        alice_account.id().suffix().into(),
        alice_account.id().prefix().into(),
    ];

    let assets = vec![asset_a.into()];
    let zero_note = create_zoroswap_note(
        inputs.into(),
        assets.into(),
        alice_account.id(),
        zero_serial_num,
        NoteTag::LocalPublicAny(123),
        NoteType::Private,
    )
    .unwrap();

    let zero_tag = zero_note.metadata().tag();

    let note_req = TransactionRequestBuilder::new()
        .own_output_notes(vec![OutputNote::Full(zero_note.clone())])
        .build()
        .unwrap();
    let tx_id = client
        .submit_new_transaction(alice_account.id(), note_req)
        .await
        .unwrap();

    println!("View transaction on MidenScan: https://testnet.midenscan.com/tx/{tx_id:?}");

    // let _ = client.submit_transaction(tx_result).await;
    client.sync_state().await?;

    let zero_note_id = zero_note.id();

    // Time from after ZOROSWAP creation
    // let start_time = Instant::now();

    // let _ = get_note_by_tag(&mut client, zero_tag, zero_note_id).await;

    // -------------------------------------------------------------------------
    // STEP 2: Partial Consume SWAPP note
    // -------------------------------------------------------------------------

    println!("\n[STEP 2] Execute ZERO note");

    let amount_out = Felt::new(51);
    let pool_0_state = [Felt::new(556), Felt::new(556), Felt::new(556), Felt::new(0)];
    let pool_1_state = [
        Felt::new(1274),
        Felt::new(1274),
        Felt::new(1274),
        Felt::new(0),
    ];

    let zero_note_args = [Felt::new(0), Felt::new(0), Felt::new(0), amount_out];
    let p2id_note_asset = asset_b.clone();
    let p2id_serial_num = [
        zero_serial_num[0],
        zero_serial_num[1],
        zero_serial_num[2],
        Felt::new(zero_serial_num[3].as_int() + 1),
    ];

    let p2id_note = create_p2id_note(
        bob_account.id(),
        alice_account.id(),
        vec![p2id_note_asset.into()],
        NoteType::Public,
        Felt::new(0),
        p2id_serial_num.into(),
    )
    .unwrap();

    let consume_zero_req = TransactionRequestBuilder::new()
        .unauthenticated_input_notes([(zero_note, Some(zero_note_args.into()))])
        .expected_output_recipients(vec![p2id_note.recipient().clone()])
        .build()
        .unwrap();

    let tx_id = client
        .submit_new_transaction(bob_account.id(), consume_zero_req)
        .await
        .unwrap();

    println!("Consumed Note Tx on MidenScan: https://testnet.midenscan.com/tx/{tx_id:?}",);

    // Stop timing

    println!("ZERO note consumed");

    Ok(())
}
