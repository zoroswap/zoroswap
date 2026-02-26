use anyhow::{Context, Result, anyhow};
use miden_client::store::TransactionFilter;
use miden_client::{
    Felt, Word,
    account::{Account, AccountId},
    asset::FungibleAsset,
    keystore::FilesystemKeyStore,
    note::{Note, NoteTag, NoteType},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use std::{collections::HashMap, str::FromStr, thread};
use url::Url;
use zoro_miden_client::{MidenClient, create_basic_account, wait_for_note};

use crate::{Config, PoolBalances, config::LiquidityPoolConfig, fetch_pool_state_from_chain,
    fetch_vault_for_account_from_chain, instantiate_client, oracle_sse::PriceMetadata};

/// Common state returned by [`setup_test_environment`] for E2E tests.
pub struct TestSetup {
    /// Parsed application config (endpoints, pool account ID, oracle URL, etc.).
    pub config: Config,
    /// Miden client connected to the node and ready to submit transactions.
    pub client: MidenClient,
    /// A freshly created basic account that acts as the test user.
    pub account: Account,
    /// The first two liquidity pools from the config, used as swap pair.
    pub pools: Vec<LiquidityPoolConfig>,
}

/// Load config, create a Miden client, sync state, and create a fresh basic account.
///
/// Uses default paths relative to the integration test working directory.
/// For custom paths, use [`setup_test_environment_with_paths`].
pub async fn setup_test_environment(store_path: &str) -> Result<TestSetup> {
    setup_test_environment_with_paths("../../config.toml", "../../masm", "../../keystore", store_path).await
}

/// Load config with custom paths, create a Miden client, sync state, and create a fresh basic
/// account. Unlike [`setup_test_environment`], all file paths are caller-supplied.
pub async fn setup_test_environment_with_paths(
    config_path: &str,
    masm_path: &str,
    keystore_path: &str,
    store_path: &str,
) -> Result<TestSetup> {
    dotenv::dotenv().ok();

    let config = Config::from_config_file(config_path, masm_path, keystore_path, store_path)?;

    assert!(
        config.liquidity_pools.len() > 1,
        "Less than 2 liquidity pools configured"
    );

    let mut client = instantiate_client(config.clone(), store_path, None).await?;
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
        account,
        pools,
    })
}

/// Mint tokens from a pool's faucet and consume them into the user's wallet.
/// Pass `None` to use the default (0.05 of the pool's token).
pub async fn fund_user_wallet(
    client: &mut MidenClient,
    account: &Account,
    pool: &LiquidityPoolConfig,
    amount: Option<u64>,
) -> Result<()> {
    let amount = amount.unwrap_or_else(|| 5 * 10u64.pow(pool.decimals as u32 - 2)); // default: 0.05
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
        .with_context(|| format!("failed to find transaction {tx_id:?} after submission"))?;
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

/// Read a single fungible asset balance from the local store for an account.
pub async fn get_local_balance(
    client: &mut MidenClient,
    account_id: AccountId,
    faucet_id: AccountId,
) -> Result<u64> {
    let record = client
        .get_account(account_id)
        .await?
        .ok_or(anyhow!("Account {account_id} not found in local store"))?;
    match record.account_data() {
        miden_client::store::AccountRecordData::Full(a) => Ok(a.vault().get_balance(faucet_id)?),
        _ => Err(anyhow!("Expected full account data for {account_id}")),
    }
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

/// Create `n` basic accounts and fund each from every configured pool faucet.
///
/// Mints run in parallel (one async task per faucet), consumes run in parallel
/// (one async task per account, batched into a single TX each).
///
/// Returns updated `Account` objects that reflect the post-consume state.
pub async fn create_funded_accounts(
    client: &mut MidenClient,
    config: &Config,
    n: usize,
    amount_per_pool: Option<u64>,
    remote_prover_url: Option<String>,
) -> Result<Vec<Account>> {
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())?;

    // 1. Create accounts
    let mut accounts = Vec::with_capacity(n);
    for i in 0..n {
        let (account, _) = create_basic_account(client, keystore.clone()).await?;
        println!("Created account #{i} => {:?}", account.id().to_hex());
        accounts.push(account);
    }
    client.sync_state().await?;

    // 2. Import faucets on main client (so it knows about them for wait_for_note)
    for pool in &config.liquidity_pools {
        client.import_account_by_id(pool.faucet_id).await?;
    }

    // 3. Mint in parallel: one thread per faucet, each minting for all N accounts
    let tmp_dir = tempfile::TempDir::new()?;
    let mint_handles: Vec<_> = config
        .liquidity_pools
        .iter()
        .enumerate()
        .map(|(pool_idx, pool)| {
            let pool = *pool;
            let config = config.clone();
            let accounts_clone: Vec<Account> = accounts.clone();
            let amount =
                amount_per_pool.unwrap_or_else(|| 5 * 10u64.pow(pool.decimals as u32 - 2));
            let store_path = tmp_dir
                .path()
                .join(format!("mint_{pool_idx}.sqlite3"))
                .to_str()
                .unwrap()
                .to_string();

            let prover_url = remote_prover_url.clone();
            thread::Builder::new()
                .name(format!("mint-faucet-{pool_idx}"))
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build mint-thread tokio runtime");
                    rt.block_on(async {
                        let mut mint_client =
                            instantiate_client(config, &store_path, prover_url).await
                                .map_err(|e| anyhow!(e))?;
                        mint_client.import_account_by_id(pool.faucet_id).await?;

                        let mut notes: Vec<(usize, Note)> = Vec::with_capacity(accounts_clone.len());
                        for (acct_idx, account) in accounts_clone.iter().enumerate() {
                            let asset = FungibleAsset::new(pool.faucet_id, amount)?;
                            let tx_request =
                                TransactionRequestBuilder::new().build_mint_fungible_asset(
                                    asset,
                                    account.id(),
                                    NoteType::Public,
                                    mint_client.rng(),
                                )?;
                            let tx_id = mint_client
                                .submit_new_transaction(pool.faucet_id, tx_request)
                                .await?;
                            println!("Minted {amount} {} for account #{acct_idx}", pool.symbol);

                            let transaction = mint_client
                                .get_transactions(TransactionFilter::Ids(vec![tx_id]))
                                .await?
                                .pop()
                                .with_context(|| format!("failed to find mint tx {tx_id:?}"))?;
                            let note = match transaction.details.output_notes.get_note(0) {
                                OutputNote::Full(n) => n.clone(),
                                _ => panic!("Expected OutputNote::Full from mint"),
                            };
                            notes.push((acct_idx, note));
                        }
                        Ok::<_, anyhow::Error>(notes)
                    })
                })
                .expect("failed to spawn mint thread")
        })
        .collect();

    // Collect minted notes from all faucet threads
    let mut minted_notes: Vec<Vec<Note>> = vec![Vec::new(); n];
    for handle in mint_handles {
        let notes = handle
            .join()
            .map_err(|_| anyhow!("Mint thread panicked"))??;
        for (acct_idx, note) in notes {
            minted_notes[acct_idx].push(note);
        }
    }

    // 4. Wait for all minted notes to become consumable
    client.sync_state().await?;
    for (acct_idx, account) in accounts.iter().enumerate() {
        for note in &minted_notes[acct_idx] {
            wait_for_note(client, account, note).await?;
        }
    }

    // 5. Consume in parallel — one thread per account, all notes batched in one TX.
    //    Each thread returns the updated Account (post-consume state).
    let consume_handles: Vec<_> = accounts
        .iter()
        .enumerate()
        .map(|(acct_idx, account)| {
            let config = config.clone();
            let account = account.clone();
            let notes: Vec<_> = minted_notes[acct_idx]
                .iter()
                .map(|n| (n.clone(), None))
                .collect();
            let store_path = tmp_dir
                .path()
                .join(format!("consume_{acct_idx}.sqlite3"))
                .to_str()
                .unwrap()
                .to_string();

            let prover_url = remote_prover_url.clone();
            thread::Builder::new()
                .name(format!("consume-acct-{acct_idx}"))
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build consume-thread tokio runtime");
                    rt.block_on(async {
                        let mut consume_client =
                            instantiate_client(config, &store_path, prover_url).await
                                .map_err(|e| anyhow!(e))?;
                        consume_client.add_account(&account, false).await?;
                        consume_client.sync_state().await?;

                        let consume_req = TransactionRequestBuilder::new()
                            .input_notes(notes)
                            .build()?;
                        consume_client
                            .submit_new_transaction(account.id(), consume_req)
                            .await?;

                        // Fetch updated account from local store (post-consume state)
                        let record = consume_client
                            .get_account(account.id())
                            .await?
                            .ok_or(anyhow!("Account not found after consume"))?;
                        let updated = match record.account_data() {
                            miden_client::store::AccountRecordData::Full(a) => a.clone(),
                            _ => return Err(anyhow!("Expected full account data after consume")),
                        };
                        Ok::<_, anyhow::Error>(updated)
                    })
                })
                .expect("failed to spawn consume thread")
        })
        .collect();

    let mut updated_accounts = Vec::with_capacity(n);
    for handle in consume_handles {
        let updated = handle
            .join()
            .map_err(|_| anyhow!("Consume thread panicked"))??;
        updated_accounts.push(updated);
    }

    client.sync_state().await?;
    println!(
        "All {n} accounts funded from {} pools",
        config.liquidity_pools.len()
    );
    Ok(updated_accounts)
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
        Felt::new(0), // padding (unused by note script)
        Felt::new(0), // padding (unused by note script)
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
    println!("previous balances for liq pool 1: {old_balances_1:?}");
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
    assert!(new_vault != *old_vault, "Vault hasn't changed");

    Ok(())
}
