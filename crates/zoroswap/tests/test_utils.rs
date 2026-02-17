use anyhow::{Context, Result, anyhow};
use miden_client::store::TransactionFilter;
use miden_client::{
    Felt, Word,
    account::{Account, AccountId},
    asset::FungibleAsset,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::{collections::HashMap, str::FromStr};
use url::Url;
use zoro_miden_client::{MidenClient, create_basic_account, wait_for_note};
use zoroswap::{
    Config, PoolBalances, config::LiquidityPoolConfig, fetch_pool_state_from_chain,
    fetch_vault_for_account_from_chain, instantiate_client, oracle_sse::PriceMetadata,
};

pub struct TestSetup {
    pub config: Config,
    pub client: MidenClient,
    pub keystore: FilesystemKeyStore,
    pub account: Account,
    pub pools: Vec<LiquidityPoolConfig>,
}

/// Load config, create a Miden client, sync state, and create a fresh basic account.
pub async fn setup_test_environment(store_path: &str) -> Result<TestSetup> {
    dotenv::dotenv().ok();

    let config = Config::from_config_file(
        "../../config.toml",
        "../../masm",
        "../../keystore",
        store_path,
    )?;

    assert!(
        config.liquidity_pools.len() > 1,
        "Less than 2 liquidity pools configured"
    );

    let mut client = instantiate_client(config.clone(), store_path).await?;
    let endpoint = config.miden_endpoint.clone();
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())?;
    let sync_summary = client.sync_state().await?;
    println!("\nLatest block: {}", sync_summary.block_num);

    let (account, _) = create_basic_account(&mut client, keystore.clone()).await?;
    println!(
        "Created Account ⇒ ID: {:?}",
        account.id().to_bech32(endpoint.to_network_id())
    );
    client.sync_state().await?;

    let pool0 = *config
        .liquidity_pools
        .first()
        .expect("No liquidity pools found in config.");
    let pool1 = *config
        .liquidity_pools
        .last()
        .expect("No liquidity pools found in config.");
    let pools = vec![pool0, pool1];

    Ok(TestSetup {
        config,
        client,
        keystore,
        account,
        pools,
    })
}

/// Mint tokens from a pool's faucet and consume them into the user's wallet.
/// Pass `amount = 0` to use the default (0.05 of the pool's token).
pub async fn fund_user_wallet(
    client: &mut MidenClient,
    account: &Account,
    pool: &LiquidityPoolConfig,
    amount: u64,
) -> Result<()> {
    let amount: u64 = if amount > 0 {
        amount
    } else {
        5 * 10u64.pow(pool.decimals as u32 - 2)
    }; // 0.05
    let fungible_asset = FungibleAsset::new(pool.faucet_id, amount)?;
    client.import_account_by_id(pool.faucet_id).await?;
    let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        account.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let tx_id = client
        .submit_new_transaction(pool.faucet_id, transaction_request)
        .await?;
    println!("Minted {amount} {} for the user.", pool.symbol);
    client.sync_state().await?;

    let transaction = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .with_context(|| "failed to find transaction {tx_id:?} after submission")
        .unwrap();
    let minted_note = match transaction.details.output_notes.get_note(0) {
        OutputNote::Full(n) => n.clone(),
        _ => panic!("Expected OutputNote::Full, got something else"),
    };

    wait_for_note(client, account, &minted_note).await?;

    let consume_req = TransactionRequestBuilder::new()
        .input_notes([(minted_note, None)])
        .build()
        .unwrap();

    let _tx_id = client
        .submit_new_transaction(account.id(), consume_req)
        .await?;
    client.sync_state().await?;
    let new_balance_user = fetch_vault_for_account_from_chain(client, account.id()).await?;
    println!("New account vault: {:?}", new_balance_user);
    println!("User successfully consumed swap into its wallet");

    Ok(())
}

/// POST a serialized note to `{server_url}/{endpoint}/submit`.
pub async fn send_to_server(server_url: &str, note: String, endpoint: &str) -> Result<()> {
    let url = Url::from_str(format!("{server_url}/{endpoint}/submit").as_str())?;
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        .body(serde_json::json!({ "note_data": note }).to_string())
        .header("Content-Type", "application/json")
        .send()
        .await?;

    println!("Server response: {:?}", res.text().await?);
    Ok(())
}

/// Look up a single price by oracle ID from fetched price metadata.
pub fn extract_oracle_price(
    prices: &[PriceMetadata],
    oracle_id: &str,
    symbol: &str,
) -> Result<u64> {
    Ok(prices
        .iter()
        .find(|p| p.id.eq(oracle_id))
        .ok_or(anyhow!(
            "No price for {} ({}) on price oracle.",
            symbol,
            oracle_id
        ))?
        .price
        .price)
}

/// Build the 12-element input vector expected by the ZOROSWAP note script.
pub fn build_zoroswap_inputs(
    requested_asset_word: Word,
    deadline: u64,
    p2id_tag: NoteTag,
    beneficiary_id: AccountId,
    sender_id: AccountId,
) -> Vec<Felt> {
    vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(deadline),
        p2id_tag.into(),
        Felt::new(0),
        Felt::new(0),
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        sender_id.suffix(),
        sender_id.prefix().into(),
    ]
}

/// Fetch current pool states and assert they differ from the provided old values.
pub async fn assert_pool_states_changed(
    client: &mut MidenClient,
    pool_account_id: AccountId,
    pool0_faucet_id: AccountId,
    pool1_faucet_id: AccountId,
    old_balances_0: &PoolBalances,
    old_balances_1: &PoolBalances,
    old_vault: &HashMap<AccountId, u64>,
) -> Result<()> {
    let (new_balances_pool_0, _) =
        fetch_pool_state_from_chain(client, pool_account_id, pool0_faucet_id).await?;
    let (new_balances_pool_1, _) =
        fetch_pool_state_from_chain(client, pool_account_id, pool1_faucet_id).await?;
    let new_vault = fetch_vault_for_account_from_chain(client, pool_account_id).await?;

    println!("previous balances for liq pool 0: {old_balances_0:?}");
    println!("previouse balances for liq pool 1: {old_balances_1:?}");
    println!("new balances for liq pool 0: {new_balances_pool_0:?}");
    println!("new balances for liq pool 1: {new_balances_pool_1:?}");
    println!("pool vault: {old_vault:?}");

    assert!(
        *old_balances_0 != new_balances_pool_0,
        "Balances for pool 0 havent changed"
    );
    assert!(
        *old_balances_1 != new_balances_pool_1,
        "Balances for pool 1 havent changed"
    );
    assert!(new_vault != *old_vault, "Balances for pool 1 havent changed");

    Ok(())
}
