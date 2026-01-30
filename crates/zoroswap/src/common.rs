use anyhow::Result;
use miden_client::{
    ClientError, Felt, ScriptBuilder, Word,
    account::AccountId,
    note::{
        Note, NoteAssets, NoteError, NoteExecutionHint, NoteMetadata, NoteRecipient, NoteTag,
        NoteType,
    },
};
use miden_client::{
    DebugMode, builder::ClientBuilder, keystore::FilesystemKeyStore, rpc::GrpcClient,
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_lib::{note::utils::build_p2id_recipient, transaction::TransactionKernel};
use rusqlite::Connection;
use std::sync::Arc;
use std::{fs, path::PathBuf};
use tracing::{debug, info, warn};

use crate::{Config, order::OrderType};
use zoro_miden_client::{MidenClient, create_library};

// --------------------------------------------------------------------------
// Zoro-Specific Helper Functions
// --------------------------------------------------------------------------

/// Logs a clickable MidenScan URL for a transaction.
pub fn print_transaction_info(tx: &miden_client::transaction::TransactionId) {
    info!(
        "View transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
        tx.to_hex()
    );
}

/// Logs a clickable MidenScan URL for a note.
pub fn print_note_info(note_id: &miden_client::note::NoteId) {
    info!(
        "View note on MidenScan: https://testnet.midenscan.com/note/{}",
        note_id.to_hex()
    );
}

/// Enables WAL mode on the SQLite database for better concurrent access.
/// WAL mode allows multiple readers and one writer simultaneously.
/// This should be called once at startup before any clients are created.
pub fn enable_wal_mode(store_path: &str) -> Result<()> {
    info!("Enabling WAL mode on database: {}", store_path);
    let conn = Connection::open(store_path)?;

    // Enable WAL mode for better concurrent access
    conn.pragma_update(None, "journal_mode", "WAL")?;

    // Set busy timeout to wait for locks instead of failing immediately (5 seconds)
    conn.pragma_update(None, "busy_timeout", 5000)?;

    // Verify WAL mode was set
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.to_lowercase() != "wal" {
        warn!("Failed to enable WAL mode, current mode: {}", mode);
    } else {
        info!("SQLite WAL mode enabled successfully");
    }

    Ok(())
}

// --------------------------------------------------------------------------
// Zoro-Specific Client Initialization
// --------------------------------------------------------------------------

/// Instantiates a Miden client with Zoro-specific configuration.
///
/// This includes:
/// - Importing the pool account
/// - Importing all faucets from liquidity pools
/// - Adding note tags for pool monitoring
pub async fn instantiate_client(
    config: Config,
    store_path: &str,
) -> Result<MidenClient, ClientError> {
    info!("Creating a new Miden Client");
    info!("Keystore path: {}", config.keystore_path);
    info!("Database path: {}", store_path);
    let timeout_ms = 10_000;
    let rpc_api = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())
        .unwrap_or_else(|err| {
            panic!(
                "Failed to create keystore at {}: {err:?}",
                config.keystore_path
            )
        })
        .into();
    let mut client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .authenticator(keystore)
        .sqlite_store(store_path.into())
        .in_debug_mode(DebugMode::Enabled)
        .build()
        .await?;
    info!("Importing pool account");
    client.import_account_by_id(config.pool_account_id).await?;
    client
        .add_note_tag(NoteTag::from_account_id(config.pool_account_id))
        .await?;
    client.sync_state().await?;
    info!("Miden client synced and ready");
    Ok(client)
}

pub async fn instantiate_faucet_client(config: Config, store_path: &str) -> Result<MidenClient> {
    info!("Creating a new Faucet client");
    info!("Keystore path: {}", config.keystore_path);
    info!("Database path: {}", store_path);
    let timeout_ms = 10_000;
    let rpc_api = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(config.keystore_path.into())
        .unwrap_or_else(|err| {
            panic!(
                "Failed to create keystore at {}: {err:?}",
                config.keystore_path
            )
        })
        .into();
    let mut client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .authenticator(keystore)
        .sqlite_store(store_path.into())
        .in_debug_mode(DebugMode::Enabled)
        .build()
        .await?;
    for pool in &config.liquidity_pools {
        info!("Importing faucet: {}", pool.faucet_id.to_hex());
        client.import_account_by_id(pool.faucet_id).await?;
    }
    info!("Faucet client ready");
    Ok(client)
}

// --------------------------------------------------------------------------
// Zoro-Specific Note Creation
// --------------------------------------------------------------------------

/// Creates the P2ID recipient that will be generated by the `ZOROSWAP.masm` script.
///
/// The ZOROSWAP script creates a P2ID note with:
/// - Serial number:
///   `[swap_serial_num[0] + 1, swap_serial_num[1], swap_serial_num[2], swap_serial_num[3]]`
/// - Script: `P2ID.masm` (using the hash stored via `proc.store_p2id_script_hash`)
/// - Inputs: `[creator_id.suffix(), creator_id.prefix()]`
pub fn create_expected_p2id_recipient(
    swap_serial_num: Word,
    creator_id: AccountId,
) -> Result<NoteRecipient, NoteError> {
    // Calculate P2ID serial number (increment first element by 1)
    let p2id_serial_num: Word = [
        swap_serial_num[0] + Felt::new(1),
        swap_serial_num[1],
        swap_serial_num[2],
        swap_serial_num[3],
    ]
    .into();

    debug!("P2ID creator id: {:?}", creator_id);
    debug!("P2ID serial num: {:?}", p2id_serial_num);
    let recipient = build_p2id_recipient(creator_id, p2id_serial_num)?;
    debug!("P2ID recipient digest: {:?}", recipient.digest());
    Ok(recipient)
}

/// Creates a ZOROSWAP note using the ZOROSWAP.masm script.
///
/// This is specific to the Zoro AMM protocol.
pub fn create_zoroswap_note(
    inputs: Vec<Felt>,
    assets: Vec<miden_client::asset::Asset>,
    creator: AccountId,
    swap_serial_num: Word,
    note_tag: NoteTag,
    note_type: NoteType,
) -> Result<Note, NoteError> {
    use miden_client::note::NoteInputs;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let assembler = TransactionKernel::assembler()
        .with_debug_mode(true)
        .with_warnings_as_errors(true);

    let path: PathBuf = [manifest_dir, "masm", "notes", "ZOROSWAP.masm"]
        .iter()
        .collect();
    let note_code = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));

    let pool_component_lib =
        create_library(assembler.clone(), "zoro::zoropool", &pool_code).unwrap();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    let aux = Felt::new(0);
    // build the outgoing note
    let metadata = NoteMetadata::new(
        creator,
        note_type,
        note_tag,
        NoteExecutionHint::always(),
        aux,
    )?;

    let assets = NoteAssets::new(assets)?;
    let recipient = NoteRecipient::new(swap_serial_num, note_script, inputs);
    let note = Note::new(assets, metadata, recipient);

    Ok(note)
}

/// Creates a DEPOSIT note using the DEPOSIT.masm script.
///
/// This is specific to the Zoro AMM protocol.
pub fn create_deposit_note(
    inputs: Vec<Felt>,
    assets: Vec<miden_client::asset::Asset>,
    creator: AccountId,
    swap_serial_num: Word,
    note_tag: NoteTag,
    note_type: NoteType,
) -> Result<Note, NoteError> {
    use miden_client::note::NoteInputs;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let assembler = TransactionKernel::assembler()
        .with_debug_mode(true)
        .with_warnings_as_errors(true);

    let path: PathBuf = [manifest_dir, "masm", "notes", "DEPOSIT.masm"]
        .iter()
        .collect();
    let note_code = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));

    let pool_component_lib =
        create_library(assembler.clone(), "zoro::zoropool", &pool_code).unwrap();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    let aux = Felt::new(0);
    // build the outgoing note
    let metadata = NoteMetadata::new(
        creator,
        note_type,
        note_tag,
        NoteExecutionHint::always(),
        aux,
    )?;

    let assets = NoteAssets::new(assets)?;
    let recipient = NoteRecipient::new(swap_serial_num, note_script, inputs);
    let note = Note::new(assets, metadata, recipient);

    Ok(note)
}

/// Creates a WITHDRAW note using the WITHDRAW.masm script.
///
/// This is specific to the Zoro AMM protocol.
pub fn create_withdraw_note(
    inputs: Vec<Felt>,
    assets: Vec<miden_client::asset::Asset>,
    creator: AccountId,
    swap_serial_num: Word,
    note_tag: NoteTag,
    note_type: NoteType,
) -> Result<Note, NoteError> {
    use miden_client::note::NoteInputs;

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let assembler = TransactionKernel::assembler()
        .with_debug_mode(true)
        .with_warnings_as_errors(true);

    let path: PathBuf = [manifest_dir, "masm", "notes", "WITHDRAW.masm"]
        .iter()
        .collect();
    let note_code = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));

    let pool_component_lib =
        create_library(assembler.clone(), "zoro::zoropool", &pool_code).unwrap();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    let aux = Felt::new(0);
    // build the outgoing note
    let metadata = NoteMetadata::new(
        creator,
        note_type,
        note_tag,
        NoteExecutionHint::always(),
        aux,
    )?;

    let assets = NoteAssets::new(assets)?;
    let recipient = NoteRecipient::new(swap_serial_num, note_script, inputs);
    let note = Note::new(assets, metadata, recipient);

    Ok(note)
}

pub fn get_script_root_for_order_type(order_type: OrderType) -> Word {
    let script = match order_type {
        OrderType::Deposit => "DEPOSIT.masm",
        OrderType::Withdraw => "WITHDRAW.masm",
        OrderType::Swap => "ZOROSWAP.masm",
    };
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let assembler = TransactionKernel::assembler()
        .with_debug_mode(true)
        .with_warnings_as_errors(true);

    let path: PathBuf = [manifest_dir, "masm", "notes", script].iter().collect();
    let note_code = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));

    let pool_component_lib =
        create_library(assembler.clone(), "zoro::zoropool", &pool_code).unwrap();

    let note_script = ScriptBuilder::new(true)
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    note_script.root()
}
