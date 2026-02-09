use anyhow::Result;
use miden_client::{
    ClientError, Felt, Word,
    account::AccountId,
    note::{
        Note, NoteAssets, NoteError, NoteMetadata, NoteRecipient, NoteScreener, NoteTag, NoteType,
    },
    sync::StateSync,
};
use miden_client::{
    DebugMode, builder::ClientBuilder, keystore::FilesystemKeyStore, rpc::GrpcClient,
};
use miden_client_sqlite_store::{ClientBuilderSqliteExt, SqliteStore};
use miden_protocol::transaction::TransactionKernel;
use miden_standards::code_builder::CodeBuilder;
use miden_standards::note::utils::build_p2id_recipient;
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
    let timeout_ms = 30_000;
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
    // Sync first so the client knows the latest block height before importing accounts.
    // Without this, import_account_by_id may fail because the client is at block 0.
    client.sync_state().await?;
    let existing = client.get_account(config.pool_account_id).await?;
    if existing.is_none() {
        info!("Pool account not in local store, importing from node");
        client.import_account_by_id(config.pool_account_id).await?;
    } else {
        info!("Pool account already in local store, skipping import");
    }
    client
        .add_note_tag(NoteTag::with_account_target(config.pool_account_id))
        .await?;
    info!("Miden client synced and ready");
    Ok(client)
}

pub async fn instantiate_faucet_client(
    config: Config,
    store_path: &str,
) -> Result<(MidenClient, StateSync)> {
    info!("Creating a new Faucet client");
    info!("Keystore path: {}", config.keystore_path);
    info!("Database path: {}", store_path);
    let timeout_ms = 30_000;
    let rpc_client = Arc::new(GrpcClient::new(&config.miden_endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(config.keystore_path.into()).unwrap_or_else(|err| {
        panic!(
            "Failed to create keystore at {}: {err:?}",
            config.keystore_path
        )
    });
    let keystore = Arc::new(keystore);
    let mut client = ClientBuilder::new()
        .rpc(rpc_client.clone())
        .authenticator(keystore.clone())
        .sqlite_store(store_path.into())
        .in_debug_mode(DebugMode::Enabled)
        .build()
        .await?;
    client.ensure_genesis_in_place().await?;
    let store_path = PathBuf::from(store_path);

    let sqlite_store = Arc::new(SqliteStore::new(store_path).await?);
    let note_screener = NoteScreener::new(sqlite_store.clone(), Some(keystore));
    let state_sync = StateSync::new(rpc_client.clone(), Arc::new(note_screener), None);

    for pool in &config.liquidity_pools {
        info!("Importing faucet: {}", pool.faucet_id.to_hex());
        client.import_account_by_id(pool.faucet_id).await?;
    }
    info!("Faucet client ready");
    Ok((client, state_sync))
}

// --------------------------------------------------------------------------
// Zoro-Specific Note Creation
// --------------------------------------------------------------------------

/// Derives a unique P2ID serial number from a note's serial number.
///
/// Matches the MASM `get_p2id_serial_num` procedure which does `add.1` on
/// top of the stack (so on `serial_num[3]`).
pub fn p2id_serial_num(serial_num: Word) -> Word {
    [
        serial_num[0],
        serial_num[1],
        serial_num[2],
        serial_num[3] + Felt::new(1),
    ]
    .into()
}

/// Creates the P2ID recipient that will be generated by the `ZOROSWAP.masm` script.
///
/// The ZOROSWAP script creates a P2ID note with:
/// - Serial number: `p2id_serial_num(swap_serial_num)`
/// - Script: `P2ID.masm` (using the hash stored via `proc.store_p2id_script_hash`)
/// - Inputs: `[beneficiary_id.suffix(), beneficiary_id.prefix()]`
pub fn create_expected_p2id_recipient(
    swap_serial_num: Word,
    beneficiary_id: AccountId,
) -> Result<NoteRecipient, NoteError> {
    let p2id_serial_num = p2id_serial_num(swap_serial_num);

    debug!("P2ID beneficiary id: {:?}", beneficiary_id);
    debug!("P2ID serial num: {:?}", p2id_serial_num);
    let recipient = build_p2id_recipient(beneficiary_id, p2id_serial_num)?;
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
    let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);

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
        create_library(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();

    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    // build the outgoing note
    let metadata = NoteMetadata::new(creator, note_type, note_tag);

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
    let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);

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
        create_library(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();

    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    // build the outgoing note
    let metadata = NoteMetadata::new(creator, note_type, note_tag);

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
    let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);

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
        create_library(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();

    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    let inputs = NoteInputs::new(inputs)?;
    // build the outgoing note
    let metadata = NoteMetadata::new(creator, note_type, note_tag);

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
    let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);

    let path: PathBuf = [manifest_dir, "masm", "notes", script].iter().collect();
    let note_code = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));

    let pool_component_lib =
        create_library(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();

    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();

    note_script.root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;

    /// Verifies that `create_expected_p2id_recipient` increments `serial_num[3]`,
    /// matching the MASM `get_p2id_serial_num` which does `add.1` on top of the
    /// stack (so on `serial_num[3]`).
    #[test]
    fn p2id_serial_num_increments_word_index_3() {
        // Given
        let creator_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let serial_num: Word = [
            Felt::new(10),
            Felt::new(20),
            Felt::new(30),
            Felt::new(40),
        ]
        .into();

        // When
        let result = p2id_serial_num(serial_num);
        let recipient = create_expected_p2id_recipient(serial_num, creator_id).unwrap();

        // Then
        let expected_serial: Word = [
            Felt::new(10),
            Felt::new(20),
            Felt::new(30),
            Felt::new(41),
        ]
        .into();
        assert_eq!(
            result, expected_serial,
            "p2id_serial_num must increment serial_num[3] to match MASM add.1"
        );

        let expected = build_p2id_recipient(creator_id, expected_serial).unwrap();
        assert_eq!(
            recipient.digest(),
            expected.digest(),
            "create_expected_p2id_recipient must produce the same recipient"
        );
    }
}
