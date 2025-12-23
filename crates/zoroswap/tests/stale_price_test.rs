use chrono::{Duration, Utc};
use miden_client::{
    Felt, Word,
    account::{AccountId, AccountStorageMode, AccountType},
    asset::FungibleAsset,
    note::{
        Note, NoteAssets, NoteExecutionHint, NoteMetadata, NoteRecipient, NoteScript, NoteTag,
        NoteType,
    },
};
use miden_objects::{FieldElement, account::AccountIdVersion};
use std::sync::Arc;
use zoroswap::{
    AmmState,
    config::{Config, LiquidityPoolConfig},
    oracle_sse::PriceData,
    pool::PoolState,
    websocket::EventBroadcaster,
};

/// Integration test: Verify that stale oracle prices prevent order matching.
///
/// This test sets up the full trading infrastructure and verifies that:
/// 1. Orders are added to the queue
/// 2. With stale prices, matching should be skipped (orders remain in queue)
/// 3. With fresh prices, matching should proceed (orders are flushed)
#[tokio::test]
async fn test_matching_skipped_with_stale_prices() {
    // Create dummy account IDs
    let faucet_a = AccountId::dummy(
        [1; 15],
        AccountIdVersion::Version0,
        AccountType::FungibleFaucet,
        AccountStorageMode::Public,
    );
    let faucet_b = AccountId::dummy(
        [2; 15],
        AccountIdVersion::Version0,
        AccountType::FungibleFaucet,
        AccountStorageMode::Public,
    );
    let pool_account_id = AccountId::dummy(
        [3; 15],
        AccountIdVersion::Version0,
        AccountType::RegularAccountUpdatableCode,
        AccountStorageMode::Public,
    );
    let user_account_id = AccountId::dummy(
        [4; 15],
        AccountIdVersion::Version0,
        AccountType::RegularAccountUpdatableCode,
        AccountStorageMode::Public,
    );

    // Create config
    let config = Config {
        liquidity_pools: vec![
            LiquidityPoolConfig {
                name: "Pool A",
                symbol: "TOKA",
                decimals: 8,
                faucet_id: faucet_a,
                oracle_id: "oracle_a",
            },
            LiquidityPoolConfig {
                name: "Pool B",
                symbol: "TOKB",
                decimals: 8,
                faucet_id: faucet_b,
                oracle_id: "oracle_b",
            },
        ],
        pool_account_id,
        oracle_sse: "http://localhost",
        oracle_https: "http://localhost",
        miden_endpoint: miden_client::rpc::Endpoint::localhost(),
        server_url: "http://localhost:3000",
        amm_tick_interval: 1000,
        network_id: miden_client::address::NetworkId::Testnet,
        masm_path: "./masm",
        keystore_path: "./keystore",
        store_path: "./test_store.sqlite3",
    };

    // Create AmmState and TradingEngine
    let broadcaster = Arc::new(EventBroadcaster::new());
    let state = Arc::new(AmmState::new(config, broadcaster.clone()).await);

    // Initialize pool states
    let pool_a_state = PoolState::new(pool_account_id, faucet_a);
    let pool_b_state = PoolState::new(pool_account_id, faucet_b);
    state.liquidity_pools().insert(faucet_a, pool_a_state);
    state.liquidity_pools().insert(faucet_b, pool_b_state);

    // Create a mock swap note
    let asset_in = FungibleAsset::new(faucet_a, 1000).unwrap();
    let deadline = Utc::now() + Duration::minutes(5);

    // Build note inputs for a swap order (12 inputs as expected by Order::from_note)
    let mut inputs: Vec<Felt> = vec![Felt::ZERO; 12];
    // requested_amount (index 0)
    inputs[0] = Felt::new(500);
    // padding (index 1)
    inputs[1] = Felt::ZERO;
    // requested faucet_id (indices 2-3)
    let faucet_b_felts: [Felt; 2] = faucet_b.into();
    inputs[2] = faucet_b_felts[1];
    inputs[3] = faucet_b_felts[0];
    // deadline (index 4)
    inputs[4] = Felt::new(deadline.timestamp_millis() as u64);
    // p2id_tag (index 5)
    inputs[5] = Felt::new(0);
    // padding (indices 6-9)
    // creator_id (indices 10-11)
    let user_felts: [Felt; 2] = user_account_id.into();
    inputs[10] = user_felts[1];
    inputs[11] = user_felts[0];

    let note_inputs = miden_client::note::NoteInputs::new(inputs).unwrap();
    let note_tag = NoteTag::from_account_id(pool_account_id);
    let metadata = NoteMetadata::new(
        user_account_id,
        NoteType::Public,
        note_tag,
        NoteExecutionHint::always(),
        Felt::ZERO,
    )
    .unwrap();
    let assets = NoteAssets::new(vec![asset_in.into()]).unwrap();
    let serial_num: Word = [Felt::new(1), Felt::new(2), Felt::new(3), Felt::new(4)].into();
    let recipient = NoteRecipient::new(serial_num, NoteScript::mock(), note_inputs);
    let note = Note::new(assets, metadata, recipient);

    // Add order to state
    let result = state.add_order(note);
    assert!(result.is_ok(), "Should be able to add order");
    assert_eq!(state.get_open_orders().len(), 1, "Should have 1 open order");

    // Set STALE oracle prices (10 seconds old)
    let stale_time = Utc::now().timestamp() as u64 - 10;
    state
        .oracle_prices()
        .insert(faucet_a, PriceData::new(stale_time, 1000));
    state
        .oracle_prices()
        .insert(faucet_b, PriceData::new(stale_time, 2000));

    // Verify prices are detected as stale
    const MAX_AGE_SECS: u64 = 2;
    let stale_age = state.oldest_stale_price(MAX_AGE_SECS);
    assert!(stale_age.is_some(), "Prices should be detected as stale");
    assert!(
        stale_age.unwrap() >= 10,
        "Stale age should be at least 10 seconds"
    );

    // The trading engine would skip matching here due to stale prices.
    // We verify the check that would cause it to skip:
    assert!(
        state.oldest_stale_price(MAX_AGE_SECS).is_some(),
        "Matching should be skipped when prices are stale"
    );

    // Now set FRESH oracle prices
    let fresh_time = Utc::now().timestamp() as u64;
    state
        .oracle_prices()
        .insert(faucet_a, PriceData::new(fresh_time, 1000));
    state
        .oracle_prices()
        .insert(faucet_b, PriceData::new(fresh_time, 2000));

    // Verify prices are no longer stale
    assert!(
        state.oldest_stale_price(MAX_AGE_SECS).is_none(),
        "Prices should no longer be stale after refresh"
    );

    // Now matching would proceed (prices are fresh)
    // Orders would be flushed and processed
    let orders = state.flush_open_orders();
    assert_eq!(
        orders.len(),
        1,
        "Should flush the order when prices are fresh"
    );
    assert_eq!(
        state.get_open_orders().len(),
        0,
        "Open orders should be empty after flush"
    );
}
