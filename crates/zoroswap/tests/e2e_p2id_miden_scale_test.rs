mod test_utils;

use std::time::{Duration, Instant};

use anyhow::Result;
use miden_client::account::AccountId;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::note::{Note, NoteAttachment, NoteType, P2idNote};
use miden_client::keystore::FilesystemKeyStore;
use miden_client::transaction::{TransactionRequest, TransactionRequestBuilder};
use miden_client::Client;
use test_utils::*;
use tracing::info;
use tracing_subscriber::EnvFilter;
use zoro_miden::account::MidenAccount;
use zoro_miden::client::MidenClient;

const N_P2IDS: usize = 20;
const MAX_CYCLES: usize = 100;
const NOTE_AMOUNT: u64 = 1_000;

fn create_private_p2ids(
    sender: AccountId,
    target: AccountId,
    faucet_id: AccountId,
    client: &mut MidenClient,
) -> Result<Vec<Note>> {
    let asset = FungibleAsset::new(faucet_id, NOTE_AMOUNT)?;
    let rng = client.client_mut().rng();
    (0..N_P2IDS)
        .map(|_| {
            P2idNote::create(
                sender,
                target,
                vec![Asset::Fungible(asset)],
                NoteType::Private,
                NoteAttachment::default(),
                rng,
            )
            .map_err(Into::into)
        })
        .collect()
}

#[derive(Clone, Copy, Default)]
struct TxPhaseDurations {
    prove: Duration,
    execute_submit: Duration,
}

impl TxPhaseDurations {
    fn add(self, other: Self) -> Self {
        Self {
            prove: self.prove + other.prove,
            execute_submit: self.execute_submit + other.execute_submit,
        }
    }
}

#[derive(Default)]
struct TimingAverages {
    prove_sum_secs: f64,
    execute_submit_sum_secs: f64,
    cycles: usize,
}

impl TimingAverages {
    fn record(&mut self, cycle: TxPhaseDurations) {
        self.prove_sum_secs += cycle.prove.as_secs_f64();
        self.execute_submit_sum_secs += cycle.execute_submit.as_secs_f64();
        self.cycles += 1;
    }

    fn avg_prove_secs(&self) -> f64 {
        self.prove_sum_secs / self.cycles as f64
    }

    fn avg_execute_submit_secs(&self) -> f64 {
        self.execute_submit_sum_secs / self.cycles as f64
    }
}

async fn submit_timed(
    client: &mut Client<FilesystemKeyStore>,
    account_id: AccountId,
    transaction_request: TransactionRequest,
) -> Result<TxPhaseDurations> {
    let prover = client.prover();

    let execute_started = Instant::now();
    let tx_result = client
        .execute_transaction(account_id, transaction_request)
        .await?;
    let execute_elapsed = execute_started.elapsed();

    let prove_started = Instant::now();
    let proven_transaction = client.prove_transaction_with(&tx_result, prover).await?;
    let prove_elapsed = prove_started.elapsed();

    let submit_started = Instant::now();
    let submission_height = client
        .submit_proven_transaction(proven_transaction, &tx_result)
        .await?;
    client
        .apply_transaction(&tx_result, submission_height)
        .await?;
    let submit_elapsed = submit_started.elapsed();

    Ok(TxPhaseDurations {
        prove: prove_elapsed,
        execute_submit: execute_elapsed + submit_elapsed,
    })
}

async fn send_and_consume_p2ids(
    client: &mut MidenClient,
    account_id: AccountId,
    notes: Vec<Note>,
) -> Result<TxPhaseDurations> {
    let send_req = TransactionRequestBuilder::new()
        .own_output_notes(notes)
        .build()?;

    let consume_inputs: Vec<_> = send_req
        .expected_output_own_notes()
        .into_iter()
        .map(|note| (note, None))
        .collect();

    let miden = client.client_mut();
    let send = submit_timed(miden, account_id, send_req).await?;
    let consume = submit_timed(
        miden,
        account_id,
        TransactionRequestBuilder::new()
            .input_notes(consume_inputs)
            .build()?,
    )
    .await?;

    Ok(send.add(consume))
}

#[tokio::test]
async fn e2e_p2id_miden_scale_swap() -> Result<()> {
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn",
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter_layer)
        .init();

    let E2ETestSetup {
        config,
        client: mut miden_client,
        ..
    } = E2ETestSetup::new().await?;
    let account = MidenAccount::deploy_new(&mut miden_client).await?;
    let account_id = *account.id();
    let faucet_id = config.liquidity_pools[0].faucet_id;

    info!(
        "account {}",
        account_id.to_bech32(config.miden_endpoint.to_network_id())
    );

    miden_client
        .mint_asset(
            faucet_id,
            account_id,
            NOTE_AMOUNT * N_P2IDS as u64 * MAX_CYCLES as u64,
        )
        .await?;

    println!(
        "p2id miden scale  private notes  n={N_P2IDS}  cycles={MAX_CYCLES}\n"
    );

    let mut timing_avgs = TimingAverages::default();
    for cycle in 0..MAX_CYCLES {
        let started = Instant::now();
        let p2ids = create_private_p2ids(account_id, account_id, faucet_id, &mut miden_client)?;
        let phases = send_and_consume_p2ids(&mut miden_client, account_id, p2ids).await?;
        timing_avgs.record(phases);

        println!(
            "cycle {:>3}/{}  {N_P2IDS} p2ids  wall {:.1}s  \
             prove {:.1}s  execute+submit {:.1}s  \
             | avg prove {:.1}s  avg execute+submit {:.1}s",
            cycle + 1,
            MAX_CYCLES,
            started.elapsed().as_secs_f64(),
            phases.prove.as_secs_f64(),
            phases.execute_submit.as_secs_f64(),
            timing_avgs.avg_prove_secs(),
            timing_avgs.avg_execute_submit_secs(),
        );
    }

    miden_client.sync_state().await?;
    Ok(())
}
