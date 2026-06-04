mod test_utils;

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use miden_client::asset::FungibleAsset;
use miden_client::note::NoteTag;
use miden_client::transaction::TransactionRequestBuilder;
use serde::Deserialize;
use test_utils::*;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use zoro_miden::account::MidenAccount;
use zoro_miden::note::{NoteInstructions, NoteKind, TrustedNote};
use zoroswap::server::AddPositionResponse;
use zoroswap::websocket::messages::{OrderStatus, SubscriptionChannel};

const N_NOTES: usize = 20;
const MAX_SWAP_CYCLES: usize = 100;
const BATCH_WAIT: Duration = Duration::from_secs(60);
const WS_KEEPALIVE: Duration = Duration::from_secs(20);

mod ansi {
    pub const RESET: &str = "\x1b[0m";
    pub const BOLD: &str = "\x1b[1m";
    pub const DIM: &str = "\x1b[2m";
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";

    pub fn paint(text: &str, color: &str) -> String {
        format!("{color}{text}{RESET}")
    }
}

use ansi::{BOLD, CYAN, DIM, GREEN, RED, RESET, YELLOW, paint};

#[derive(Debug, Deserialize)]
struct SubmitOrderResponse {
    success: bool,
    order_id: String,
    message: String,
}

#[derive(Clone, Default)]
struct NoteTiming {
    matching: Option<Duration>,
    matched: Option<Duration>,
    executed: Option<Duration>,
    failed: Option<Duration>,
}

#[derive(Default)]
struct TestStats {
    cycles_done: usize,
    total_executed: usize,
    total_matched: usize,
    total_failed: usize,
}

struct CycleResult {
    notes: Vec<NoteTiming>,
    executed: usize,
    matched: usize,
    failed: usize,
    pending: usize,
    avg_match_time: Option<Duration>,
    avg_match_gap: Option<Duration>,
    avg_exec_gap: Option<Duration>,
}

fn avg_gap(times: &[Duration]) -> Option<Duration> {
    if times.len() < 2 {
        return None;
    }
    let mut sorted = times.to_vec();
    sorted.sort();
    let total: Duration = sorted.windows(2).map(|w| w[1] - w[0]).sum();
    Some(total / (sorted.len() as u32 - 1))
}

fn avg_time(times: &[Duration]) -> Option<Duration> {
    if times.is_empty() {
        return None;
    }
    Some(times.iter().sum::<Duration>() / times.len() as u32)
}

fn clear_screen() {
    print!("\x1b[2J\x1b[H");
    let _ = io::stdout().flush();
}

fn note_chip_color(note: &NoteTiming) -> &'static str {
    if note.executed.is_some() {
        GREEN
    } else if note.failed.is_some() {
        RED
    } else if note.matched.is_some() {
        CYAN
    } else if note.matching.is_some() {
        YELLOW
    } else {
        DIM
    }
}

fn note_chips(notes: &[NoteTiming]) -> String {
    notes
        .iter()
        .enumerate()
        .map(|(i, note)| paint(&format!("[{:02}]", i + 1), note_chip_color(note)))
        .collect()
}

fn count_color(done: usize, total: usize) -> &'static str {
    if done == total {
        GREEN
    } else if done > 0 {
        YELLOW
    } else {
        DIM
    }
}

fn build_cycle_line(
    cycle: usize,
    notes: &[NoteTiming],
    pending: usize,
    batch_secs: f64,
    wait_left: Option<Duration>,
) -> String {
    let n = N_NOTES;
    let matched = notes.iter().filter(|note| note.matched.is_some()).count();
    let executed = notes.iter().filter(|note| note.executed.is_some()).count();
    let failed = notes.iter().filter(|note| note.failed.is_some()).count();
    let matched_times: Vec<Duration> = notes.iter().filter_map(|note| note.matched).collect();
    let exec_times: Vec<Duration> = notes.iter().filter_map(|note| note.executed).collect();

    let mut line = format!(
        "cycle {:>3}  {}  {}  {}  {}",
        cycle + 1,
        paint(&format!("matched {matched}/{n}"), count_color(matched, n)),
        paint(&format!("executed {executed}/{n}"), count_color(executed, n)),
        paint(
            &format!("failed {failed}"),
            if failed > 0 { RED } else { DIM },
        ),
        note_chips(notes),
    );
    if let Some(t) = avg_time(&matched_times) {
        line.push_str(&format!("  avg_match {:.2}s", t.as_secs_f64()));
    }
    if let Some(gap) = avg_gap(&matched_times) {
        line.push_str(&format!("  match_gap {:.3}s", gap.as_secs_f64()));
    }
    if let Some(gap) = avg_gap(&exec_times) {
        line.push_str(&format!("  exec_gap {:.3}s", gap.as_secs_f64()));
    }
    line.push_str(&format!("  {:.1}s", batch_secs));
    if let Some(left) = wait_left {
        let secs = left.as_secs();
        let tc = if secs <= 10 { RED } else { DIM };
        line.push_str(&format!("  {tc}{secs}s left{RESET}"));
    } else if pending > 0 {
        line.push_str(&format!("  {}", paint(&format!("{pending} pending"), YELLOW)));
    }
    line
}

fn render_view(history: &[String], stats: &TestStats, live: Option<&str>) {
    clear_screen();
    println!(
        "{BOLD}{CYAN}zoroswap position scale test{RESET}  notes={N_NOTES}  cycles={MAX_SWAP_CYCLES}"
    );
    println!(
        "total {} / {} executed   {} cycles done",
        paint(
            &stats.total_executed.to_string(),
            count_color(stats.total_executed, MAX_SWAP_CYCLES * N_NOTES)
        ),
        MAX_SWAP_CYCLES * N_NOTES,
        stats.cycles_done,
    );
    println!(
        "{} {} {} {} {}",
        paint(".", DIM),
        paint("matching", YELLOW),
        paint("matched", CYAN),
        paint("executed", GREEN),
        paint("failed", RED),
    );
    if !history.is_empty() {
        println!();
        for line in history {
            println!("{line}");
        }
    }
    if let Some(line) = live {
        println!();
        println!("{line}");
    }
    let _ = io::stdout().flush();
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WsServerMessage {
    OrderUpdate {
        order_id: String,
        status: String,
    },
    #[serde(other)]
    Other,
}

fn parse_order_status(status: &str) -> Option<OrderStatus> {
    match status {
        "pending" => Some(OrderStatus::Pending),
        "matching" => Some(OrderStatus::Matching),
        "matched" => Some(OrderStatus::Matched),
        "executed" => Some(OrderStatus::Executed),
        "failed" => Some(OrderStatus::Failed),
        "expired" => Some(OrderStatus::Expired),
        _ => None,
    }
}

async fn run_order_ws_session(
    host: &str,
    tx: &mpsc::UnboundedSender<(String, OrderStatus)>,
) -> bool {
    let url = format!("ws://{host}/ws");
    let Ok((ws, _)) = connect_async(&url).await else {
        return false;
    };
    let (mut write, mut read) = ws.split();

    let subscribe = serde_json::json!({
        "type": "Subscribe",
        "channels": [SubscriptionChannel::OrderUpdates { order_id: None }],
    });
    if write
        .send(WsMessage::Text(subscribe.to_string().into()))
        .await
        .is_err()
    {
        return false;
    }

    let mut keepalive = tokio::time::interval(WS_KEEPALIVE);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await;

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                let ping = serde_json::json!({"type": "Ping"});
                if write.send(WsMessage::Text(ping.to_string().into())).await.is_err() {
                    return false;
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(WsMessage::Text(text))) => {
                        let Ok(WsServerMessage::OrderUpdate { order_id, status }) =
                            serde_json::from_str(&text)
                        else {
                            continue;
                        };
                        let Some(status) = parse_order_status(&status) else {
                            continue;
                        };
                        if tx.send((order_id, status)).is_err() {
                            return true;
                        }
                    }
                    Some(Ok(WsMessage::Ping(payload))) => {
                        if write.send(WsMessage::Pong(payload)).await.is_err() {
                            return false;
                        }
                    }
                    Some(Ok(WsMessage::Pong(_)))
                    | Some(Ok(WsMessage::Binary(_)))
                    | Some(Ok(WsMessage::Frame(_))) => {}
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => return false,
                }
            }
        }
    }
}

async fn spawn_order_update_listener(
    server_host: &str,
) -> Result<mpsc::UnboundedReceiver<(String, OrderStatus)>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let host = server_host.to_string();

    tokio::spawn(async move {
        loop {
            if run_order_ws_session(&host, &tx).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    Ok(rx)
}

fn apply_status(note: &mut NoteTiming, status: OrderStatus, at: Duration) -> bool {
    match status {
        OrderStatus::Matching if note.matching.is_none() => note.matching = Some(at),
        OrderStatus::Matched if note.matched.is_none() => note.matched = Some(at),
        OrderStatus::Executed if note.executed.is_none() => note.executed = Some(at),
        OrderStatus::Failed | OrderStatus::Expired if note.failed.is_none() => {
            note.failed = Some(at)
        }
        _ => return false,
    }
    true
}

async fn wait_for_batch(
    updates: &mut mpsc::UnboundedReceiver<(String, OrderStatus)>,
    order_ids: &[String],
    timeout: Duration,
    cycle: usize,
    history: &mut Vec<String>,
    stats: &TestStats,
) -> CycleResult {
    let started = Instant::now();
    let index: HashMap<String, usize> = order_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), i))
        .collect();
    let mut notes = vec![NoteTiming::default(); order_ids.len()];
    let mut awaiting_exec: HashSet<String> = order_ids.iter().cloned().collect();
    let deadline = started + timeout;
    let mut refresh = tokio::time::interval(Duration::from_millis(200));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh.tick().await;

    render_view(
        history,
        stats,
        Some(&build_cycle_line(
            cycle,
            &notes,
            awaiting_exec.len(),
            started.elapsed().as_secs_f64(),
            Some(timeout),
        )),
    );

    loop {
        if Instant::now() >= deadline {
            break;
        }
        let wait_left = deadline.saturating_duration_since(Instant::now());
        let remaining = wait_left;

        tokio::select! {
            _ = refresh.tick() => {}
            msg = tokio::time::timeout(remaining, updates.recv()) => {
                match msg {
                    Ok(Some((order_id, status))) => {
                        if let Some(&i) = index.get(&order_id) {
                            let at = started.elapsed();
                            if apply_status(&mut notes[i], status, at) && notes[i].executed.is_some()
                            {
                                awaiting_exec.remove(&order_id);
                            }
                        }
                    }
                    _ => break,
                }
            }
        }

        render_view(
            history,
            stats,
            Some(&build_cycle_line(
                cycle,
                &notes,
                awaiting_exec.len(),
                started.elapsed().as_secs_f64(),
                Some(wait_left),
            )),
        );
        if awaiting_exec.is_empty() || Instant::now() >= deadline {
            break;
        }
    }

    let matched_times: Vec<Duration> = notes.iter().filter_map(|n| n.matched).collect();
    let exec_times: Vec<Duration> = notes.iter().filter_map(|n| n.executed).collect();

    let result = CycleResult {
        executed: exec_times.len(),
        matched: matched_times.len(),
        failed: notes.iter().filter(|n| n.failed.is_some()).count(),
        pending: awaiting_exec.len(),
        avg_match_time: avg_time(&matched_times),
        avg_match_gap: avg_gap(&matched_times),
        avg_exec_gap: avg_gap(&exec_times),
        notes,
    };
    let done = build_cycle_line(
        cycle,
        &result.notes,
        result.pending,
        started.elapsed().as_secs_f64(),
        None,
    );
    history.push(done);
    render_view(history, stats, None);
    result
}

#[tokio::test]
async fn e2e_position_scale_swap() -> Result<()> {
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .init();

    println!("\n\t[STEP 0] Init client and config\n");
    let E2ETestSetup {
        config,
        client: mut miden_client,
        mut zoro_pool,
        prices,
    } = E2ETestSetup::new().await?;
    let mut account = MidenAccount::deploy_new(&mut miden_client).await?;
    let pool0 = config.liquidity_pools[0];
    let pool1 = config.liquidity_pools[1];

    info!(
        "Testing with account {} with tag {}",
        account
            .id()
            .to_bech32(config.miden_endpoint.to_network_id()),
        NoteTag::with_account_target(*account.id())
    );

    let initial_vault = zoro_pool.vault().await?;
    let initial_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let initial_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 1] Fund user wallet\n");
    let amount0 = 500_000;
    let amount = amount0 * 11 / 10;
    miden_client
        .mint_asset(pool0.faucet_id, *account.id(), amount * N_NOTES as u64)
        .await?;

    // tokio::time::sleep(Duration::from_millis(4100)).await;

    let user_balance0 = account.get_balance(&pool0.faucet_id).await?;
    let user_balance1 = account.get_balance(&pool1.faucet_id).await?;
    info!("Minted: {amount} to the test account. New balance: {user_balance0}");

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 2] Create position\n");

    let notes = (0..N_NOTES)
        .map(|_| {
            TrustedNote::new(
                NoteInstructions {
                    note_kind: NoteKind::Position,
                    attached_assets: vec![FungibleAsset::new(pool0.faucet_id, amount)?],
                    asset_input: None,
                    beneficiary: *account.id(),
                    amount_input: amount,
                    note_type: miden_client::note::NoteType::Public,
                    deadline: Utc::now().timestamp_millis() as u64 + 999_999_999,
                    p2id_tag: account.tag(),
                    pool_tag: zoro_pool.miden_account().tag(),
                },
                miden_client.client().code_builder(),
            )
        })
        .collect::<Result<Vec<_>>>()?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 3] Init position on server\n");

    miden_client
        .send_notes(account.id(), &config.pool_account_id, notes.clone())
        .await?;
    let res = send_to_server(
        &format!("http://{}", config.server_url),
        notes
            .iter()
            .map(|note| note.serialize_to_string().unwrap())
            .collect(),
        "positions/new",
    )
    .await?;

    let positions: Vec<AddPositionResponse> = res
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    let position_ids: Vec<Uuid> = positions.iter().map(|p| p.position_id).collect();
    info!(
        "Opened {} positions: {:?}",
        position_ids.len(),
        position_ids
    );

    let pool0_price = prices.get(&pool0.faucet_id).unwrap().price;
    let pool1_price = prices.get(&pool1.faucet_id).unwrap().price;
    let swap_amount = amount / 100;
    let max_slippage = 0.005;
    let min_amount_out = |amount_in: u64| -> u64 {
        (((pool0_price as f64) / (pool1_price as f64)) * (amount_in as f64) * (1.0 - max_slippage))
            as u64
    };

    let asset_in = pool0.faucet_id.to_bech32(config.network_id.clone());
    let asset_out = pool1.faucet_id.to_bech32(config.network_id.clone());
    let server_http = format!("http://{}", config.server_url);

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 4] WebSocket listener and repeated position swaps\n");
    let mut order_updates = spawn_order_update_listener(&config.server_url).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(position_ids.len(), N_NOTES);

    let mut history: Vec<String> = Vec::new();
    let mut stats = TestStats::default();
    render_view(&history, &stats, None);

    for cycle in 0..MAX_SWAP_CYCLES {
        let responses =
            futures_util::future::try_join_all(position_ids.iter().map(|position_id| {
                send_position_swap_to_server(
                    server_http.clone(),
                    "positions/swap".to_string(),
                    *position_id,
                    asset_in.clone(),
                    asset_out.clone(),
                    swap_amount,
                    min_amount_out(swap_amount),
                )
            }))
            .await?;

        let orders: Vec<SubmitOrderResponse> = responses
            .iter()
            .enumerate()
            .map(|(i, resp)| {
                let order: SubmitOrderResponse = serde_json::from_str(resp)?;
                if !order.success {
                    bail!("swap cycle {cycle} position {i} failed: {}", order.message);
                }
                Ok(order)
            })
            .collect::<Result<_>>()?;

        let order_ids: Vec<String> = orders.iter().map(|o| o.order_id.clone()).collect();
        let result = wait_for_batch(
            &mut order_updates,
            &order_ids,
            BATCH_WAIT,
            cycle,
            &mut history,
            &stats,
        )
        .await;
        stats.total_executed += result.executed;
        stats.total_matched += result.matched;
        stats.total_failed += result.failed;
        stats.cycles_done += 1;
    }
    clear_screen();
    render_view(&history, &stats, None);
    let summary = format!("{} / {}", stats.total_executed, MAX_SWAP_CYCLES * N_NOTES);
    println!(
        "{BOLD}{GREEN}finished{RESET}: {}",
        paint(&summary, if stats.total_executed > 0 { GREEN } else { RED })
    );

    tokio::time::sleep(Duration::from_millis(5000)).await;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 5] Get note back from server\n");

    let reclaimed_notes =
        futures_util::future::try_join_all(position_ids.iter().map(|position_id| {
            get_position_note(
                format!("{}", config.server_url),
                "positions/get_note".to_string(),
                *position_id,
            )
        }))
        .await?;
    println!(
        "Reclaim note ids: {}",
        reclaimed_notes
            .iter()
            .map(|r| r.note().id().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 6] Reclaim the note\n");
    miden_client.sync_state().await?;

    let reclaim_transaction_request = TransactionRequestBuilder::new().build_consume_notes(
        reclaimed_notes
            .into_iter()
            .map(|r| r.note().clone())
            .collect::<Vec<_>>(),
    )?;
    miden_client
        .client_mut()
        .submit_new_transaction(*account.id(), reclaim_transaction_request)
        .await?;

    // ---------------------------------------------------------------------------------
    println!("\n\t[STEP 7] Confirm pool states updated accordingly\n");
    tokio::time::sleep(Duration::from_millis(4100)).await;
    zoro_pool.update_pool_state_from_chain().await?;
    let end_vault = zoro_pool.vault().await?;
    let end_pool0 = *zoro_pool.pool_states().get(&pool0.faucet_id).unwrap();
    let end_pool1 = *zoro_pool.pool_states().get(&pool1.faucet_id).unwrap();
    let end_user_balance0 = account.get_balance(&pool0.faucet_id).await?;
    let end_user_balance1 = account.get_balance(&pool1.faucet_id).await?;

    zoro_pool.print_pool_states();

    assert!(
        end_pool0.balances() != initial_pool0.balances(),
        "Balances for pool 0 havent changed"
    );
    assert!(
        end_pool1.balances() != initial_pool1.balances(),
        "Balances for pool 1 havent changed"
    );
    assert!(end_vault != initial_vault, "Vault hasn't changed");
    assert!(
        end_user_balance0 != user_balance0,
        "Balances for user for faucet0 havent changed"
    );
    assert!(
        end_user_balance1 != user_balance1,
        "Balances for user for faucet1 havent changed"
    );
    assert!(
        stats.total_executed > 0,
        "expected at least one swap to execute over {MAX_SWAP_CYCLES} cycles"
    );

    Ok(())
}
