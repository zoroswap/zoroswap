use anyhow::{Result, anyhow};
use backon::{BackoffBuilder, ExponentialBuilder};
use chrono::Utc;
use clap::Parser;
use miden_client::{
    Felt, Word,
    RemoteTransactionProver,
    account::{Account, AccountId},
    asset::FungibleAsset,
    builder::ClientBuilder,
    crypto::FeltRng,
    keystore::FilesystemKeyStore,
    note::{NoteTag, NoteType},
    rpc::GrpcClient,
    transaction::{
        OutputNote, TransactionProver, TransactionRequestBuilder,
    },
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::{collections::HashSet, fmt, sync::Arc, thread, time::{Duration, Instant}};
use tempfile::TempDir;
use tracing::{error, info, warn};
use miden_client::asset::Asset;
use zoro_miden_client::{MidenClient, create_basic_account, create_p2id_note};
use zoroswap::{
    Config,
    config::LiquidityPoolConfig,
    create_deposit_note, create_withdraw_note, create_zoroswap_note,
    get_oracle_prices, instantiate_client,
    note_serialization::serialize_note,
    oracle_sse::PriceMetadata,
    testing::{
        build_zoroswap_inputs, extract_oracle_price, send_to_server,
    },
};

#[derive(Parser, Debug)]
#[command(name = "stresstest", about = "Continuous stress test for Zoroswap")]
struct Cli {
    /// Number of accounts to create and fund (one worker per account).
    #[arg(long, default_value_t = 3)]
    accounts: usize,

    /// Total number of operations to dispatch.
    #[arg(long, default_value_t = 10_000)]
    total_ops: usize,

    /// Path to the config.toml file.
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Remote prover endpoints (comma-separated). Workers are assigned round-robin.
    #[arg(long, value_delimiter = ',')]
    remote_prover: Vec<String>,
}

#[derive(Debug, Clone)]
enum StressOp {
    Swap {
        pool_in_idx: usize,
        pool_out_idx: usize,
        amount_fraction: f64,
    },
    Deposit {
        pool_idx: usize,
        amount_fraction: f64,
    },
    Withdraw {
        pool_idx: usize,
        amount_fraction: f64,
    },
    Malformed {
        variant: MalformedVariant,
        pool_idx: usize,
    },
}

#[derive(Debug, Clone)]
enum MalformedVariant {
    ZeroAmount,
    ExpiredDeadline,
    GarbageInputs,
}

impl fmt::Display for StressOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StressOp::Swap { pool_in_idx, pool_out_idx, amount_fraction } => {
                write!(f, "Swap(in={pool_in_idx}, out={pool_out_idx}, frac={amount_fraction:.3})")
            }
            StressOp::Deposit { pool_idx, amount_fraction } => {
                write!(f, "Deposit(pool={pool_idx}, frac={amount_fraction:.3})")
            }
            StressOp::Withdraw { pool_idx, amount_fraction } => {
                write!(f, "Withdraw(pool={pool_idx}, frac={amount_fraction:.3})")
            }
            StressOp::Malformed { variant, pool_idx } => {
                write!(f, "Malformed({variant:?}, pool={pool_idx})")
            }
        }
    }
}

#[derive(Debug)]
enum StressOutcome {
    Success,
    ExpectedFailure(String),
    UnexpectedFailure(String),
}

impl fmt::Display for StressOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StressOutcome::Success => write!(f, "OK"),
            StressOutcome::ExpectedFailure(msg) => write!(f, "Expected failure: {msg}"),
            StressOutcome::UnexpectedFailure(msg) => write!(f, "UNEXPECTED failure: {msg}"),
        }
    }
}

struct StressResult {
    op: StressOp,
    outcome: StressOutcome,
}

struct WorkItem {
    account_id: AccountId,
    op: StressOp,
    prices: Vec<PriceMetadata>,
}

enum WorkResult {
    OpComplete {
        account_id: AccountId,
        result: StressResult,
    },
    Error {
        account_id: AccountId,
        error: String,
    },
}

struct StressStats {
    total_ops: usize,
    successes: usize,
    expected_failures: usize,
    unexpected_failures: usize,
}

impl StressStats {
    fn new() -> Self {
        Self {
            total_ops: 0,
            successes: 0,
            expected_failures: 0,
            unexpected_failures: 0,
        }
    }

    fn record(&mut self, result: &StressResult) {
        self.total_ops += 1;
        match &result.outcome {
            StressOutcome::Success => self.successes += 1,
            StressOutcome::ExpectedFailure(_) => self.expected_failures += 1,
            StressOutcome::UnexpectedFailure(_) => self.unexpected_failures += 1,
        }
    }
}

/// Pick one random operation type and generate it with random parameters.
fn generate_single_op(rng: &mut StdRng, num_pools: usize) -> StressOp {
    let malformed_variants = [
        MalformedVariant::ZeroAmount,
        MalformedVariant::ExpiredDeadline,
        MalformedVariant::GarbageInputs,
    ];

    let op_type = rng.random_range(0..4u32);
    match op_type {
        0 => {
            let pool_in_idx = rng.random_range(0..num_pools);
            let mut pool_out_idx = rng.random_range(0..num_pools);
            if num_pools > 1 {
                while pool_out_idx == pool_in_idx {
                    pool_out_idx = rng.random_range(0..num_pools);
                }
            }
            StressOp::Swap {
                pool_in_idx,
                pool_out_idx,
                amount_fraction: rng.random_range(0.01..0.5_f64),
            }
        }
        1 => StressOp::Deposit {
            pool_idx: rng.random_range(0..num_pools),
            amount_fraction: rng.random_range(0.01..0.5_f64),
        },
        2 => StressOp::Withdraw {
            pool_idx: rng.random_range(0..num_pools),
            amount_fraction: rng.random_range(0.01..0.5_f64),
        },
        _ => {
            let variant_idx = rng.random_range(0..malformed_variants.len());
            StressOp::Malformed {
                variant: malformed_variants[variant_idx].clone(),
                pool_idx: rng.random_range(0..num_pools),
            }
        }
    }
}

async fn execute_swap(
    client: &mut MidenClient,
    account_id: AccountId,
    pools: &[LiquidityPoolConfig],
    pool_account_id: AccountId,
    server_url: &str,
    prices: &[PriceMetadata],
    pool_in_idx: usize,
    pool_out_idx: usize,
    amount_fraction: f64,
) -> Result<StressOutcome> {
    let pool_in = &pools[pool_in_idx];
    let pool_out = &pools[pool_out_idx];

    // Get balance and compute amount
    let balance = client
        .get_account(account_id)
        .await?
        .ok_or(anyhow!("Account not found"))?;
    let balance = match balance.account_data() {
        miden_client::store::AccountRecordData::Full(a) => a.vault().get_balance(pool_in.faucet_id)?,
        _ => return Err(anyhow!("Partial account data")),
    };
    if balance == 0 {
        return Ok(StressOutcome::ExpectedFailure("Zero balance for swap input".into()));
    }

    let amount_in = ((balance as f64) * amount_fraction) as u64;
    if amount_in == 0 {
        return Ok(StressOutcome::ExpectedFailure("Computed swap amount is 0".into()));
    }

    let pool_in_price = extract_oracle_price(prices, pool_in.oracle_id, pool_in.symbol)?;
    let pool_out_price = extract_oracle_price(prices, pool_out.oracle_id, pool_out.symbol)?;

    let max_slippage = 0.05;
    let min_amount_out =
        (((pool_in_price as f64) / (pool_out_price as f64)) * (amount_in as f64) * (1.0 - max_slippage)) as u64;
    let min_amount_out = min_amount_out.max(1);

    let asset_in = FungibleAsset::new(pool_in.faucet_id, amount_in)?;
    let asset_out = FungibleAsset::new(pool_out.faucet_id, min_amount_out)?;
    let requested_asset_word: Word = asset_out.into();
    let p2id_tag = NoteTag::with_account_target(account_id);
    let deadline = (Utc::now().timestamp_millis() as u64) + 120_000;

    let inputs = build_zoroswap_inputs(
        requested_asset_word,
        deadline,
        p2id_tag,
        account_id,
        account_id,
    );
    let serial_num = client.rng().draw_word();
    let note = create_zoroswap_note(
        inputs,
        vec![asset_in.into()],
        account_id,
        serial_num,
        NoteTag::with_account_target(pool_account_id),
        NoteType::Private,
    )?;

    let note_clone = note.clone();
    submit_with_retry(client, account_id, || {
        Ok(TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(note_clone.clone())])
            .build()?)
    }).await?;

    let serialized = serialize_note(&note)?;
    send_to_server(
        &server_base_url(server_url),
        serialized,
        "orders",
    ).await?;

    info!("Swap submitted: {amount_in} {} -> {} (min_out={min_amount_out})",
        pool_in.symbol, pool_out.symbol);
    Ok(StressOutcome::Success)
}

async fn execute_deposit(
    client: &mut MidenClient,
    account_id: AccountId,
    pools: &[LiquidityPoolConfig],
    server_url: &str,
    pool_idx: usize,
    amount_fraction: f64,
) -> Result<StressOutcome> {
    let pool = &pools[pool_idx];

    let balance = client
        .get_account(account_id)
        .await?
        .ok_or(anyhow!("Account not found"))?;
    let balance = match balance.account_data() {
        miden_client::store::AccountRecordData::Full(a) => a.vault().get_balance(pool.faucet_id)?,
        _ => return Err(anyhow!("Partial account data")),
    };
    if balance == 0 {
        return Ok(StressOutcome::ExpectedFailure("Zero balance for deposit".into()));
    }

    let amount_in = ((balance as f64) * amount_fraction) as u64;
    if amount_in == 0 {
        return Ok(StressOutcome::ExpectedFailure("Computed deposit amount is 0".into()));
    }

    let max_slippage = 0.05;
    let min_lp_amount_out = ((amount_in as f64) * (1.0 - max_slippage)) as u64;
    let asset_in = FungibleAsset::new(pool.faucet_id, amount_in)?;
    let p2id_tag = NoteTag::with_account_target(account_id);
    let deadline = (Utc::now().timestamp_millis() as u64) + 120_000;

    let inputs = vec![
        Felt::new(min_lp_amount_out),
        Felt::new(deadline),
        p2id_tag.into(),
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
        account_id.suffix(),
        account_id.prefix().into(),
    ];

    let serial_num = client.rng().draw_word();
    let note = create_deposit_note(
        inputs,
        vec![asset_in.into()],
        account_id,
        serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )?;

    let note_clone = note.clone();
    submit_with_retry(client, account_id, || {
        Ok(TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(note_clone.clone())])
            .build()?)
    }).await?;

    let serialized = serialize_note(&note)?;
    send_to_server(
        &server_base_url(server_url),
        serialized,
        "deposit",
    ).await?;

    info!("Deposit submitted: {amount_in} {} (min_lp={min_lp_amount_out})", pool.symbol);
    Ok(StressOutcome::Success)
}

async fn execute_withdraw(
    client: &mut MidenClient,
    account_id: AccountId,
    pools: &[LiquidityPoolConfig],
    server_url: &str,
    pool_idx: usize,
    amount_fraction: f64,
) -> Result<StressOutcome> {
    let pool = &pools[pool_idx];

    let amount_to_withdraw = ((pool.decimals as u32 - 2) as f64 * amount_fraction * 100.0) as u64;
    let amount_to_withdraw = amount_to_withdraw.max(1) * 10u64.pow(pool.decimals as u32 - 2);

    let max_slippage = 0.05;
    let min_asset_amount_out = ((amount_to_withdraw as f64) * (1.0 - max_slippage)) as u64;
    let min_asset_amount_out = min_asset_amount_out.max(1);

    let asset_out = FungibleAsset::new(pool.faucet_id, min_asset_amount_out)?;
    let p2id_tag = NoteTag::with_account_target(account_id);
    let deadline = (Utc::now().timestamp_millis() as u64) + 120_000;
    let asset_out_word: Word = asset_out.into();

    let inputs = vec![
        asset_out_word[0],
        asset_out_word[1],
        asset_out_word[2],
        asset_out_word[3],
        Felt::new(0),
        Felt::new(amount_to_withdraw),
        Felt::new(deadline),
        p2id_tag.into(),
        Felt::new(0),
        Felt::new(0),
        account_id.suffix(),
        account_id.prefix().into(),
    ];

    let serial_num = client.rng().draw_word();
    let note = create_withdraw_note(
        inputs,
        vec![],
        account_id,
        serial_num,
        NoteTag::new(0),
        NoteType::Private,
    )?;

    let note_clone = note.clone();
    submit_with_retry(client, account_id, || {
        Ok(TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(note_clone.clone())])
            .build()?)
    }).await?;

    let serialized = serialize_note(&note)?;
    send_to_server(
        &server_base_url(server_url),
        serialized,
        "withdraw",
    ).await?;

    info!("Withdraw submitted: {amount_to_withdraw} LP from {} (min_asset_out={min_asset_amount_out})",
        pool.symbol);
    Ok(StressOutcome::Success)
}

async fn execute_malformed(
    client: &mut MidenClient,
    account_id: AccountId,
    pools: &[LiquidityPoolConfig],
    pool_account_id: AccountId,
    server_url: &str,
    variant: &MalformedVariant,
    pool_idx: usize,
) -> Result<StressOutcome> {
    let pool = &pools[pool_idx];

    let (inputs, assets, endpoint) = match variant {
        MalformedVariant::ZeroAmount => {
            let asset_out = FungibleAsset::new(pool.faucet_id, 1)?;
            let requested_asset_word: Word = asset_out.into();
            let p2id_tag = NoteTag::with_account_target(account_id);
            let deadline = (Utc::now().timestamp_millis() as u64) + 120_000;
            let inputs = build_zoroswap_inputs(
                requested_asset_word,
                deadline,
                p2id_tag,
                account_id,
                account_id,
            );
            (inputs, vec![], "orders")
        }
        MalformedVariant::ExpiredDeadline => {
            let amount = 10u64.pow(pool.decimals as u32 - 2);
            let asset_in = FungibleAsset::new(pool.faucet_id, amount)?;
            let p2id_tag = NoteTag::with_account_target(account_id);
            let deadline = 1u64;
            let inputs = vec![
                Felt::new(1),
                Felt::new(deadline),
                p2id_tag.into(),
                Felt::new(0),
                Felt::new(0),
                Felt::new(0),
                account_id.suffix(),
                account_id.prefix().into(),
            ];
            (inputs, vec![asset_in.into()], "deposit")
        }
        MalformedVariant::GarbageInputs => {
            let garbage = (1u64 << 63) - 1;
            let amount = 10u64.pow(pool.decimals as u32 - 2);
            let asset_in = FungibleAsset::new(pool.faucet_id, amount)?;
            let inputs = vec![
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(0),
                Felt::new(0),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
                Felt::new(garbage),
            ];
            (inputs, vec![asset_in.into()], "orders")
        }
    };

    let serial_num = client.rng().draw_word();
    let note_result = match endpoint {
        "orders" => create_zoroswap_note(
            inputs,
            assets,
            account_id,
            serial_num,
            NoteTag::with_account_target(pool_account_id),
            NoteType::Private,
        ),
        "deposit" => create_deposit_note(
            inputs,
            assets,
            account_id,
            serial_num,
            NoteTag::new(0),
            NoteType::Private,
        ),
        _ => create_withdraw_note(
            inputs,
            assets,
            account_id,
            serial_num,
            NoteTag::new(0),
            NoteType::Private,
        ),
    };

    let note = match note_result {
        Ok(n) => n,
        Err(e) => {
            return Ok(StressOutcome::ExpectedFailure(
                format!("Malformed note creation failed (expected): {e}")
            ));
        }
    };

    let note_clone = note.clone();
    match submit_with_retry(client, account_id, || {
        Ok(TransactionRequestBuilder::new()
            .own_output_notes(vec![OutputNote::Full(note_clone.clone())])
            .build()?)
    }).await {
        Ok(_) => {}
        Err(e) => {
            return Ok(StressOutcome::ExpectedFailure(
                format!("Malformed TX submission failed (expected): {e}")
            ));
        }
    }

    let serialized = serialize_note(&note)?;
    match send_to_server(
        &server_base_url(server_url),
        serialized,
        endpoint,
    ).await {
        Ok(_) => {
            info!("Malformed note ({variant:?}) accepted by server");
        }
        Err(e) => {
            return Ok(StressOutcome::ExpectedFailure(
                format!("Malformed note rejected by server (expected): {e}")
            ));
        }
    }

    Ok(StressOutcome::ExpectedFailure(
        format!("Malformed note ({variant:?}) submitted")
    ))
}

async fn execute_op(
    client: &mut MidenClient,
    account_id: AccountId,
    op: &StressOp,
    pools: &[LiquidityPoolConfig],
    pool_account_id: AccountId,
    server_url: &str,
    prices: &[PriceMetadata],
) -> StressResult {
    let result = match op {
        StressOp::Swap { pool_in_idx, pool_out_idx, amount_fraction } => {
            execute_swap(client, account_id, pools, pool_account_id, server_url, prices,
                *pool_in_idx, *pool_out_idx, *amount_fraction).await
        }
        StressOp::Deposit { pool_idx, amount_fraction } => {
            execute_deposit(client, account_id, pools, server_url,
                *pool_idx, *amount_fraction).await
        }
        StressOp::Withdraw { pool_idx, amount_fraction } => {
            execute_withdraw(client, account_id, pools, server_url,
                *pool_idx, *amount_fraction).await
        }
        StressOp::Malformed { variant, pool_idx } => {
            execute_malformed(client, account_id, pools, pool_account_id, server_url,
                variant, *pool_idx).await
        }
    };

    match result {
        Ok(outcome) => StressResult { op: op.clone(), outcome },
        Err(e) => {
            let err_msg = format!("{e:#}");
            let is_malformed = matches!(op, StressOp::Malformed { .. });
            let is_prover_overload = err_msg.contains("Server is busy")
                || err_msg.contains("Timeout expired");
            let outcome = if is_malformed || is_prover_overload {
                StressOutcome::ExpectedFailure(err_msg)
            } else {
                StressOutcome::UnexpectedFailure(err_msg)
            };
            StressResult { op: op.clone(), outcome }
        }
    }
}

async fn create_worker_client(
    config: &Config,
    store_path: &str,
    remote_prover_url: &str,
    accounts: &[Account],
) -> Result<MidenClient> {
    let timeout_ms = 30_000;
    let rpc_api = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())
        .unwrap_or_else(|err| panic!("Failed to create keystore: {err:?}"))
        .into();
    let prover: Arc<dyn TransactionProver + Send + Sync> =
        Arc::new(RemoteTransactionProver::new(remote_prover_url));
    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .authenticator(keystore)
        .prover(prover)
        .sqlite_store(store_path.into())
        .build()
        .await?;
    client.sync_state().await?;

    // Import pool account + all faucets
    client.import_account_by_id(config.pool_account_id).await?;
    client.add_note_tag(NoteTag::with_account_target(config.pool_account_id)).await?;
    for pool in &config.liquidity_pools {
        client.import_account_by_id(pool.faucet_id).await?;
    }

    // Add ALL accounts so any worker can handle any account
    for account in accounts {
        client.add_account(account, false).await?;
        client.add_note_tag(NoteTag::with_account_target(account.id())).await?;
    }

    client.sync_state().await?;
    Ok(client)
}

fn run_worker(
    worker_id: usize,
    config: Config,
    accounts: Vec<Account>,
    store_path: String,
    remote_prover_url: String,
    work_rx: async_channel::Receiver<WorkItem>,
    result_tx: async_channel::Sender<WorkResult>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build per-worker tokio runtime");

    rt.block_on(async {
        let mut client = match create_worker_client(
            &config, &store_path, &remote_prover_url, &accounts,
        ).await {
            Ok(c) => c,
            Err(e) => {
                error!("Worker {worker_id} init failed: {e:#}");
                return;
            }
        };
        info!("Worker {worker_id} initialized");

        let pools: Vec<LiquidityPoolConfig> = config.liquidity_pools.clone();

        while let Ok(item) = work_rx.recv().await {
            if let Err(e) = client.sync_state().await {
                let _ = result_tx.send(WorkResult::Error {
                    account_id: item.account_id,
                    error: format!("Sync failed: {e:#}"),
                }).await;
                continue;
            }

            let result = execute_op(
                &mut client,
                item.account_id,
                &item.op,
                &pools,
                config.pool_account_id,
                config.server_url,
                &item.prices,
            ).await;

            let _ = result_tx.send(WorkResult::OpComplete {
                account_id: item.account_id,
                result,
            }).await;
        }
    });
}

/// Ensure the server URL has a scheme. URLs that already include a scheme
/// (e.g. `https://...`) are returned as-is. Bare hostnames get `http://` for
/// localhost/127.0.0.1 and `https://` for everything else.
fn server_base_url(server_url: &str) -> String {
    if server_url.starts_with("http://") || server_url.starts_with("https://") {
        server_url.to_string()
    } else if server_url.starts_with("localhost") || server_url.starts_with("127.0.0.1") {
        format!("http://{server_url}")
    } else {
        format!("https://{server_url}")
    }
}

/// Submit a transaction with exponential backoff on prover/transport errors.
///
/// `build_request` is called on each attempt so the `TransactionRequest` can be
/// rebuilt after a failed (consumed) attempt.
async fn submit_with_retry<F>(
    client: &mut MidenClient,
    account_id: AccountId,
    build_request: F,
) -> Result<()>
where
    F: Fn() -> Result<miden_client::transaction::TransactionRequest>,
{
    let mut delays: backon::ExponentialBackoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_secs(1))
        .with_max_delay(Duration::from_secs(60))
        .with_factor(2.0)
        .build();

    loop {
        let req = build_request()?;
        match client.submit_new_transaction(account_id, req).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                let err_str = format!("{e:?}");
                let err_str_display = format!("{e:#}");
                let combined = format!("{err_str} {err_str_display}");
                let is_retryable = combined.contains("transport error")
                    || combined.contains("prover error")
                    || combined.contains("native certs")
                    || combined.contains("initial state commitment")
                    || combined.contains("transaction executor error")
                    || combined.contains("Server is busy")
                    || combined.contains("Timeout expired");
                if is_retryable {
                    if let Some(delay) = delays.next() {
                        warn!(
                            "Retryable error, retrying in {:.1}s: {err_str_display}",
                            delay.as_secs_f64()
                        );
                        tokio::time::sleep(delay).await;
                        client.sync_state().await.ok();
                        continue;
                    }
                }
                return Err(anyhow!(e));
            }
        }
    }
}

/// For measuring how long each step takes.
struct SetupTimings {
    account_creation_secs: f64,
    minting_secs: f64,
    funder_consume_secs: f64,
    distribute_secs: f64,
    consume_secs: f64,
}

/// How many P2ID output notes to include in a single distribution transaction
/// from the funder account to the specified amount of accounts to use for the
/// stress tests.
///
/// Kept conservative to avoid remote-prover timeouts on large batches.
const P2ID_BATCH_SIZE: usize = 50;

/// The server's faucet mints this fixed amount per call.
const FAUCET_MINT_AMOUNT: u64 = 10_000_000;

/// Create `n` basic accounts and fund each via P2ID distribution.
///
/// Instead of minting once per account×pool (currently N×3 pool calls to the 
/// server), we:
///   1. Create accounts locally
///   2. Mint once per pool to a single "funder" account (3 server calls for 3 pools)
///   3. Funder consumes the 3 minted notes
///   4. Funder distributes tokens to all accounts via P2ID notes (batched)
///   5. Each target account consumes its P2ID notes in parallel
async fn create_funded_accounts_via_server(
    client: &mut MidenClient,
    config: &Config,
    n: usize,
    remote_prover_urls: &[String],
) -> Result<(Vec<Account>, SetupTimings)> {
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())?;
    let http_client = reqwest::Client::new();
    let server_url = server_base_url(config.server_url);
    let num_pools = config.liquidity_pools.len();

    // Phase 1: Create accounts (local only)
    let phase_start = Instant::now();
    let mut accounts = Vec::with_capacity(n);
    for i in 0..n {
        let (account, _) = create_basic_account(client, keystore.clone()).await?;
        println!("[SETUP] Created account {}/{n}: {}", i + 1, account.id().to_bech32(config.network_id.clone()));
        accounts.push(account);
    }
    client.sync_state().await?;
    let account_creation_secs = phase_start.elapsed().as_secs_f64();
    println!("[SETUP] Account creation done in {account_creation_secs:.1}s\n");

    // Phase 2: Mint to funder (If we have 3 pools: 3 server calls, one per pool)
    // The funder is `accounts[0]`. The server mints a fixed amount per call.
    // Each account will receive `FAUCET_MINT_AMOUNT / n` tokens per pool.
    let phase_start = Instant::now();
    let funder = &accounts[0];
    let funder_address = funder.id().to_bech32(config.network_id.clone());
    let amount_per_account = FAUCET_MINT_AMOUNT / (n as u64);
    assert!(amount_per_account > 0, "Too many accounts for the fixed mint amount ({FAUCET_MINT_AMOUNT})");
    println!("[SETUP] Minting {num_pools} pools to funder ({}), {amount_per_account} per account...",
        funder.id().to_hex());

    for pool in &config.liquidity_pools {
        let faucet_bech32 = pool.faucet_id.to_bech32(config.network_id.clone());
        let resp = http_client
            .post(&format!("{server_url}/faucets/mint"))
            .json(&serde_json::json!({
                "address": funder_address,
                "faucet_id": faucet_bech32,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("Mint request failed for funder, pool {}: {e}", pool.symbol))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Mint failed for funder, pool {}: {status} - {body}", pool.symbol));
        }
        let body: serde_json::Value = resp.json().await?;
        if body.get("success").and_then(|v| v.as_bool()) != Some(true) {
            let msg = body.get("message").and_then(|v| v.as_str()).unwrap_or("unknown");
            return Err(anyhow!("Mint failed for funder, pool {}: {msg}", pool.symbol));
        }
        println!("[SETUP] Minted {} to funder", pool.symbol);
    }
    let minting_secs = phase_start.elapsed().as_secs_f64();
    println!("[SETUP] Minting done in {minting_secs:.1}s\n");

    // Phase 3: Funder consumes minted notes
    let phase_start = Instant::now();
    // Our server's `/faucets/mint` is fire-and-forget. It queues mint requests
    // via an MPSC channel and returns `{"success": true}` immediately. The actual
    // minting (prove + submit + block commitment) happens asynchronously on the
    // server and typically takes 15-40s per pool on testnet.
    let phase3_timeout = Duration::from_secs(180);
    println!("[SETUP] Funder consuming {num_pools} minted notes (timeout: {}s)...",
        phase3_timeout.as_secs());
    {
        let funder_store_path = config
            .store_path
            .replace(".sqlite3", "_funder.sqlite3");
        // Build the funder client manually instead of using `instantiate_client`
        // so the funder's note tag (`NoteTag::with_account_target(funder.id()`))
        // is registered BEFORE the first `sync_state()`. Without this, the Miden
        // node never returns matching notes because it doesn't know what tags
        // to filter for.
        let timeout_ms = 30_000;
        let rpc_api = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
        let funder_keystore: Arc<FilesystemKeyStore> =
            FilesystemKeyStore::new(config.keystore_path.into())?.into();
        let funder_prover: Arc<dyn TransactionProver + Send + Sync> =
            Arc::new(RemoteTransactionProver::new(&remote_prover_urls[0]));
        let mut funder_client = ClientBuilder::new()
            .rpc(rpc_api)
            .authenticator(funder_keystore)
            .prover(funder_prover)
            .sqlite_store(funder_store_path.into())
            .build()
            .await?;
        // Register tag BEFORE first sync so the node returns matching notes.
        funder_client.add_account(funder, false).await?;
        funder_client.add_note_tag(NoteTag::with_account_target(funder.id())).await?;
        funder_client.sync_state().await?;

        // Poll until all minted notes are visible and consumable.
        loop {
            if phase_start.elapsed() > phase3_timeout {
                return Err(anyhow!(
                    "Timeout waiting for minted notes after {}s. \
                     The server may still be processing mint transactions.",
                    phase3_timeout.as_secs()
                ));
            }

            funder_client.sync_state().await?;

            let consumable = funder_client.get_consumable_notes(Some(funder.id())).await?;
            println!(
                "[SETUP] Funder: {}/{num_pools} notes ready, elapsed: {:.0}s",
                consumable.len(),
                phase_start.elapsed().as_secs_f64(),
            );

            if consumable.len() >= num_pools {
                info!("Funder: {} notes ready, consuming", consumable.len());
                let notes = consumable;
                submit_with_retry(
                    &mut funder_client,
                    funder.id(),
                    || {
                        let input_notes: Vec<_> = notes
                            .iter()
                            .map(|(note, _)| (note.clone().try_into().unwrap(), None))
                            .collect();
                        Ok(TransactionRequestBuilder::new()
                            .input_notes(input_notes)
                            .build()?)
                    },
                ).await?;
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        // Update funder account state after consume
        let record = funder_client
            .get_account(funder.id())
            .await?
            .ok_or(anyhow!("Funder account not found after consume"))?;
        let updated_funder = match record.account_data() {
            miden_client::store::AccountRecordData::Full(a) => a.clone(),
            _ => return Err(anyhow!("Expected full funder account data after consume")),
        };
        accounts[0] = updated_funder;
    }
    let funder_consume_secs = phase_start.elapsed().as_secs_f64();
    println!("[SETUP] Funder consume done in {funder_consume_secs:.1}s\n");

    // Phase 4: Distribute via P2ID (batched, one pool at a time)
    // For each pool, create P2ID notes sending `amount_per_account` to every
    // target account (`accounts[1..n]`). The funder (`accounts[0]`) keeps its share
    // naturally, no need to send tokens to itself via P2ID.
    // Batched into groups of P2ID_BATCH_SIZE to keep TX size manageable for the
    // remote prover.
    let phase_start = Instant::now();
    let num_targets = n - 1;
    let total_p2id_notes = num_targets * num_pools;
    println!("[SETUP] Distributing {total_p2id_notes} P2ID notes ({num_targets} targets × {num_pools} pools)...");

    let distribute_store_path = config
        .store_path
        .replace(".sqlite3", "_distribute.sqlite3");
    let mut dist_client =
        instantiate_client(config.clone(), &distribute_store_path, Some(remote_prover_urls[0].clone()))
            .await
            .map_err(|e| anyhow!(e))?;
    dist_client.add_account(&accounts[0], false).await?;
    dist_client.sync_state().await?;

    let mut p2id_notes_created = 0usize;
    for pool in &config.liquidity_pools {
        let asset = FungibleAsset::new(pool.faucet_id, amount_per_account)?;

        // Build all P2ID notes for this pool, then submit in batches (skip funder at index 0)
        for batch_start in (1..n).step_by(P2ID_BATCH_SIZE) {
            let batch_end = (batch_start + P2ID_BATCH_SIZE).min(n);
            let mut batch_notes = Vec::with_capacity(batch_end - batch_start);

            for target in &accounts[batch_start..batch_end] {
                let serial_num = dist_client.rng().draw_word();
                let note = create_p2id_note(
                    accounts[0].id(),
                    target.id(),
                    vec![Asset::Fungible(asset)],
                    NoteType::Public,
                    serial_num,
                )?;
                batch_notes.push(note);
            }

            let batch_output_notes: Vec<OutputNote> = batch_notes
                .iter()
                .map(|n| OutputNote::Full(n.clone()))
                .collect();

            submit_with_retry(&mut dist_client, accounts[0].id(), || {
                Ok(TransactionRequestBuilder::new()
                    .own_output_notes(batch_output_notes.clone())
                    .build()?)
            }).await?;

            p2id_notes_created += batch_end - batch_start;
            println!("[SETUP] Distributed {p2id_notes_created}/{total_p2id_notes} P2ID notes ({})",
                pool.symbol);
        }
    }
    let distribute_secs = phase_start.elapsed().as_secs_f64();
    println!("[SETUP] Distribution done in {distribute_secs:.1}s\n");

    // Phase 5: Target accounts (1..n) consume P2ID notes (parallel threads).
    // The funder (`accounts[0]`) already has its share from Phase 3.
    let phase_start = Instant::now();
    println!("[SETUP] Waiting for notes and consuming in parallel ({num_targets} target accounts)...");
    let consume_handles: Vec<_> = accounts[1..]
        .iter()
        .enumerate()
        .map(|(target_idx, account)| {
            let acct_idx = target_idx + 1; // restore original index for logging/paths
            let config = config.clone();
            let account = account.clone();
            let store_path = config
                .store_path
                .replace(".sqlite3", &format!("_consume_{acct_idx}.sqlite3"));
            let prover_url = remote_prover_urls[acct_idx % remote_prover_urls.len()].clone();
            let expected_notes = num_pools;

            thread::Builder::new()
                .name(format!("consume-acct-{acct_idx}"))
                .spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("failed to build consume-thread tokio runtime");
                    rt.block_on(async {
                        // Build client manually. We must register the note
                        // tag BEFORE the first sync so P2ID notes from the
                        // distribution phase (already on-chain) are discovered.
                        let timeout_ms = 30_000;
                        let rpc_api = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
                        let keystore: Arc<FilesystemKeyStore> = FilesystemKeyStore::new(config.keystore_path.into())
                            .expect("Failed to create keystore")
                            .into();
                        let prover: Arc<dyn TransactionProver + Send + Sync> =
                            Arc::new(RemoteTransactionProver::new(&prover_url));
                        let mut consume_client = ClientBuilder::new()
                            .rpc(rpc_api)
                            .authenticator(keystore)
                            .prover(prover)
                            .sqlite_store(store_path.into())
                            .build()
                            .await?;

                        // Register tag and account BEFORE first sync
                        consume_client.add_account(&account, false).await?;
                        consume_client.add_note_tag(NoteTag::with_account_target(account.id())).await?;
                        consume_client.sync_state().await?;

                        // Poll until all P2ID notes are available
                        let consume_timeout = Duration::from_secs(180);
                        let consume_start = Instant::now();
                        loop {
                            if consume_start.elapsed() > consume_timeout {
                                return Err(anyhow!(
                                    "Account #{acct_idx}: timeout waiting for P2ID notes after {}s",
                                    consume_timeout.as_secs()
                                ));
                            }
                            consume_client.sync_state().await?;
                            let notes = consume_client.get_consumable_notes(Some(account.id())).await?;
                            if notes.len() >= expected_notes {
                                info!("Account #{acct_idx}: {} notes ready, consuming", notes.len());
                                submit_with_retry(
                                    &mut consume_client,
                                    account.id(),
                                    || {
                                        let input_notes: Vec<_> = notes
                                            .iter()
                                            .map(|(note, _)| (note.clone().try_into().unwrap(), None))
                                            .collect();
                                        Ok(TransactionRequestBuilder::new()
                                            .input_notes(input_notes)
                                            .build()?)
                                    },
                                ).await?;
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }

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
    updated_accounts.push(accounts[0].clone()); // funder already funded from Phase 3
    for (i, handle) in consume_handles.into_iter().enumerate() {
        let updated = handle
            .join()
            .map_err(|_| anyhow!("Consume thread panicked"))??;
        println!("[SETUP] Account #{} consumed and ready", i + 1);
        updated_accounts.push(updated);
    }
    let consume_secs = phase_start.elapsed().as_secs_f64();
    println!("[SETUP] Consume done in {consume_secs:.1}s\n");

    client.sync_state().await?;
    let timings = SetupTimings {
        account_creation_secs,
        minting_secs,
        funder_consume_secs,
        distribute_secs,
        consume_secs,
    };
    Ok((updated_accounts, timings))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // .with_env_filter("info,zoroswap=debug,zoro_miden_client=debug")
        .init();
    dotenv::dotenv().ok();

    let cli = Cli::parse();

    println!("========================================================================================");
    println!("ZoroSwap Stress Test");
    println!("========================================================================================");
    assert!(!cli.remote_prover.is_empty(), "At least one --remote-prover endpoint is required");
    let num_provers = cli.remote_prover.len();
    println!("Accounts/Workers: {}", cli.accounts);
    println!("Total ops:        {}", cli.total_ops);
    println!("Provers ({}):     {}", num_provers, cli.remote_prover.join(", "));
    println!("========================================================================================");
    if cli.accounts < num_provers {
        warn!(
            "You have {} prover(s) but only {} account(s)/worker(s). \
             Some provers will be idle. Use --accounts >= {} to utilize all provers.",
            num_provers, cli.accounts, num_provers,
        );
    }
    if num_provers > 1 && cli.accounts < num_provers * 2 {
        warn!(
            "For best throughput, use at least 2-3x as many accounts as provers \
             (recommended: --accounts {}). Each worker blocks on sync_state() and \
             network I/O between proving requests, so extra workers keep provers busy.",
            num_provers * 3,
        );
    }
    if cli.total_ops < cli.accounts {
        warn!(
            "Total ops ({}) is less than accounts ({}). \
             Some workers will never receive work.",
            cli.total_ops, cli.accounts,
        );
    }
    println!();

    let start_time = Instant::now();

    // 1. Load config (stores go into a temporary directory)
    let tmp_dir = TempDir::new()?;
    let setup_store_path = tmp_dir.path().join("setup.sqlite3");
    let setup_store_str = setup_store_path.to_str().unwrap();

    let config = Config::from_config_file(
        &cli.config,
        "masm",
        "keystore",
        setup_store_str,
    )?;
    let pools: Vec<LiquidityPoolConfig> = config.liquidity_pools.clone();
    let num_pools = pools.len();
    assert!(num_pools >= 2, "Need at least 2 liquidity pools configured");

    // 2. Create setup client and funded accounts
    println!("[SETUP] Creating {} funded accounts via server mint...", cli.accounts);
    let mut setup_client = instantiate_client(
        config.clone(),
        setup_store_str,
        Some(cli.remote_prover[0].clone()),
    ).await?;
    let (accounts, setup_timings) = create_funded_accounts_via_server(
        &mut setup_client,
        &config,
        cli.accounts,
        &cli.remote_prover,
    ).await?;
    let setup_total_secs = start_time.elapsed().as_secs_f64();
    println!("[SETUP] All accounts funded. Total setup: {setup_total_secs:.1}s\n");

    let account_ids: Vec<AccountId> = accounts.iter().map(|a| a.id()).collect();
    let num_workers = cli.accounts;

    // 3. Spawn worker threads with MPMC work-stealing channel
    println!("[WORKERS] Spawning {num_workers} workers...");
    let (work_tx, work_rx) = async_channel::unbounded::<WorkItem>();
    let (result_tx, result_rx) = async_channel::unbounded::<WorkResult>();

    let mut worker_handles: Vec<thread::JoinHandle<()>> = Vec::with_capacity(num_workers);

    for worker_id in 0..num_workers {
        let store_path = tmp_dir.path().join(format!("worker_{worker_id}.sqlite3"));
        let store_str = store_path.to_str().unwrap().to_string();

        let w_config = config.clone();
        let w_accounts = accounts.clone();
        let w_work_rx = work_rx.clone();
        let w_result_tx = result_tx.clone();
        let w_remote_prover = cli.remote_prover[worker_id % cli.remote_prover.len()].clone();
        println!("[WORKERS] Worker {worker_id} -> prover: {w_remote_prover}");

        let handle = thread::Builder::new()
            .name(format!("worker-{worker_id}"))
            .spawn(move || {
                run_worker(
                    worker_id,
                    w_config,
                    w_accounts,
                    store_str,
                    w_remote_prover,
                    w_work_rx,
                    w_result_tx,
                );
            })
            .expect("failed to spawn worker thread");

        worker_handles.push(handle);
    }

    // Drop our copy of work_rx so the channel closes when work_tx is dropped
    drop(work_rx);
    // Drop our copy of result_tx so the channel closes when all workers are done
    drop(result_tx);

    // 4. Continuous pipeline main loop
    // NOTE: We intentionally do not consume returned P2ID notes (from swaps/withdrawals).
    // This is a stress test focused on throughput, not correctness of payouts.
    // Account balances will deplete over time as assets are sent but payouts not consumed.

    let oracle_ids: Vec<&str> = pools.iter().map(|p| p.oracle_id).collect();
    let mut available: Vec<AccountId> = account_ids;
    let mut in_flight: HashSet<AccountId> = HashSet::new();
    let mut ops_dispatched: usize = 0;
    let mut ops_completed: usize = 0;
    let mut prices = get_oracle_prices(config.oracle_https, oracle_ids.clone()).await?;
    let mut last_price_refresh = Instant::now();
    let mut last_progress_report = Instant::now();
    let mut master_rng = StdRng::from_os_rng();
    let mut stats = StressStats::new();
    let ops_start = Instant::now();

    println!("[OPS] Starting main loop...\n");

    loop {
        // Refresh prices every 60s
        if last_price_refresh.elapsed() > Duration::from_secs(60) {
            prices = get_oracle_prices(config.oracle_https, oracle_ids.clone()).await?;
            last_price_refresh = Instant::now();
        }

        // Progress report every 10s
        if last_progress_report.elapsed() > Duration::from_secs(10) {
            let elapsed = ops_start.elapsed().as_secs_f64();
            let throughput = if elapsed > 0.0 { ops_completed as f64 / elapsed } else { 0.0 };
            println!(
                "[PROGRESS] {ops_completed}/{} done | {ops_dispatched} dispatched | \
                 {} in-flight | {:.1} ops/s | ok:{} exp_fail:{} unexp_fail:{}",
                cli.total_ops,
                in_flight.len(),
                throughput,
                stats.successes,
                stats.expected_failures,
                stats.unexpected_failures,
            );
            last_progress_report = Instant::now();
        }

        tokio::select! {
            // Dispatch work if accounts available and not yet dispatched enough
            _ = std::future::ready(()), if !available.is_empty() && ops_dispatched < cli.total_ops => {
                let idx = master_rng.random_range(0..available.len());
                let account_id = available.swap_remove(idx);
                in_flight.insert(account_id);
                let op = generate_single_op(&mut master_rng, num_pools);
                ops_dispatched += 1;
                work_tx.send(WorkItem {
                    account_id,
                    op,
                    prices: prices.clone(),
                }).await.map_err(|_| anyhow!("Work channel closed unexpectedly"))?;
            }
            // Collect results
            result = result_rx.recv() => {
                match result {
                    Ok(WorkResult::OpComplete { account_id, result }) => {
                        ops_completed += 1;
                        if matches!(result.outcome, StressOutcome::UnexpectedFailure(_)) {
                            error!("[{ops_completed}/{} | {}] {} => {}",
                                cli.total_ops, account_id.to_hex(), result.op, result.outcome);
                        } else {
                            println!("  [{ops_completed}/{} | {}] {} => {}",
                                cli.total_ops, account_id.to_hex(), result.op, result.outcome);
                        }
                        stats.record(&result);
                        in_flight.remove(&account_id);
                        available.push(account_id);
                    }
                    Ok(WorkResult::Error { account_id, error }) => {
                        error!("Error for {}: {error}", account_id.to_hex());
                        stats.total_ops += 1;
                        stats.unexpected_failures += 1;
                        ops_completed += 1;
                        in_flight.remove(&account_id);
                        available.push(account_id);
                    }
                    Err(_) => break,
                }
                if ops_completed >= cli.total_ops { break; }
            }
        }
    }

    // 5. Shutdown workers
    drop(work_tx);
    for handle in worker_handles {
        let _ = handle.join();
    }

    // 6. Final report
    let total_elapsed = start_time.elapsed();
    let ops_elapsed = ops_start.elapsed();
    let ops_throughput = ops_completed as f64 / ops_elapsed.as_secs_f64();
    println!("\n========================================================================================");
    println!("FINAL REPORT");
    println!("========================================================================================");
    println!("Total ops:             {}", stats.total_ops);
    println!("Successes:             {}", stats.successes);
    println!("Expected failures:     {}", stats.expected_failures);
    println!("Unexpected failures:   {}", stats.unexpected_failures);
    println!();
    println!("  Successes:           Ops that were submitted and accepted by the node.");
    println!("  Expected failures:   Ops that failed for a known reason (e.g. zero");
    println!("                       balance, computed amount is 0). These are benign.");
    println!("  Unexpected failures: Ops that failed for an unknown reason. These");
    println!("                       indicate bugs or infrastructure issues.");
    println!("----------------------------------------------------------------------------------------");
    println!("Operations (each selected uniformly at random, 25% each):");
    println!("  Swap:                Swap tokens between two pools (5% max slippage).");
    println!("  Deposit:             Deposit tokens into a pool and receive LP tokens.");
    println!("  Withdraw:            Burn LP tokens and withdraw underlying assets.");
    println!("  Malformed:           Invalid op (zero amount / expired deadline / garbage inputs).");
    println!("----------------------------------------------------------------------------------------");
    println!("Setup:");
    println!("  Account creation:    {:.1}s", setup_timings.account_creation_secs);
    println!("  Minting (funder):    {:.1}s", setup_timings.minting_secs);
    println!("  Funder consume:      {:.1}s", setup_timings.funder_consume_secs);
    println!("  P2ID distribution:   {:.1}s", setup_timings.distribute_secs);
    println!("  Target consume:      {:.1}s", setup_timings.consume_secs);
    println!("  Total setup:         {:.1}s", setup_total_secs);
    println!();
    println!("  Account creation:    Time to create basic accounts on the node.");
    println!("  Minting (funder):    Time for the funder to mint tokens from each faucet.");
    println!("  Funder consume:      Time for the funder to consume minted notes into its wallet.");
    println!("  P2ID distribution:   Time to send P2ID notes from funder to each target account.");
    println!("  Target consume:      Time for each target account to consume its P2ID notes.");
    println!("----------------------------------------------------------------------------------------");
    println!("Ops phase:             {:.1}s", ops_elapsed.as_secs_f64());
    println!("  Throughput:          {:.1} ops/s", ops_throughput);
    println!("Total elapsed:         {:.1}s", total_elapsed.as_secs_f64());
    println!();
    println!("  Ops phase:           Wall-clock time for the swap/deposit/withdraw ops only.");
    println!("  Throughput:          Ops completed / ops phase duration.");
    println!("  Total elapsed:       Wall-clock time including setup and ops.");
    println!("========================================================================================");

    Ok(())
}
