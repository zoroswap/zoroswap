use anyhow::{Context, Result};
use miden_assembly::{
    LibraryPath,
    ast::{Module, ModuleKind},
};
use miden_client::store::TransactionFilter;
use miden_client::{
    Client, ClientError, Felt, Word,
    account::{
        Account, AccountBuilder, AccountId, AccountStorageMode, AccountType, Address, NetworkId,
    },
    asset::{Asset, FungibleAsset, TokenSymbol},
    auth::AuthSecretKey,
    builder::ClientBuilder,
    keystore::FilesystemKeyStore,
    note::{
        Note, NoteAssets, NoteError, NoteExecutionHint, NoteId, NoteMetadata, NoteRelevance,
        NoteTag, NoteType,
    },
    rpc::{Endpoint, GrpcClient},
    store::{InputNoteRecord, NoteFilter},
    transaction::{OutputNote, TransactionRequestBuilder},
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_lib::{
    account::{auth::AuthRpoFalcon512, faucets::BasicFungibleFaucet, wallets::BasicWallet},
    note::utils::build_p2id_recipient,
};
use miden_objects::assembly::{Assembler, DefaultSourceManager};
use rand::RngCore;
use rand::rngs::StdRng;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{debug, info, trace, warn};

// --------------------------------------------------------------------------
// Type Aliases
// --------------------------------------------------------------------------

pub type MidenClient = Client<FilesystemKeyStore<StdRng>>;

// --------------------------------------------------------------------------
// Client Initialization
// --------------------------------------------------------------------------

/// Creates a simple Miden client with default settings.
///
/// This is a minimal client setup without account-specific configuration.
/// For more advanced configuration, consider using `ClientBuilder` directly.
pub async fn instantiate_simple_client(
    keystore_path: &str,
    endpoint: &Endpoint,
) -> Result<MidenClient, ClientError> {
    let timeout_ms = 10_000;
    let rpc_api = Arc::new(GrpcClient::new(endpoint, timeout_ms));
    let keystore = FilesystemKeyStore::new(keystore_path.into())
        .unwrap_or_else(|err| panic!("Failed to create keystore: {err:?}"))
        .into();
    let client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .authenticator(keystore)
        .in_debug_mode(true.into())
        .sqlite_store("store.sqlite3".into())
        .build()
        .await?;
    Ok(client)
}

// --------------------------------------------------------------------------
// Note Creation Utilities
// --------------------------------------------------------------------------

/// Creates a P2ID (Pay-to-ID) note.
///
/// # Arguments
/// * `sender`: The account ID sending the note
/// * `target`: The account ID receiving the note
/// * `assets`: Assets to include in the note
/// * `note_type`: Type of the note (Public/Private)
/// * `aux`: Auxiliary data
/// * `serial_num`: Serial number for the note
pub fn create_p2id_note(
    sender: AccountId,
    target: AccountId,
    assets: Vec<Asset>,
    note_type: NoteType,
    aux: Felt,
    serial_num: Word,
) -> Result<Note, NoteError> {
    info!(
        "Creating P2ID sender: {}, target: {}, assets: {assets:?}, note_type: {note_type:?}, aux: {aux:?}, serial_num: {serial_num:?}",
        sender.to_bech32(NetworkId::Testnet),
        target.to_bech32(NetworkId::Testnet)
    );
    let recipient = build_p2id_recipient(target, serial_num)?;
    let tag = account_id_to_note_tag(target);
    let metadata = NoteMetadata::new(sender, note_type, tag, NoteExecutionHint::always(), aux)?;
    let vault = NoteAssets::new(assets)?;
    Ok(Note::new(vault, metadata, recipient))
}

/// Polls for a specific note by tag until found.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `tag`: Note tag to filter by
/// * `target_note_id`: The specific note ID to wait for
pub async fn get_note_by_tag(
    client: &mut MidenClient,
    tag: NoteTag,
    target_note_id: NoteId,
) -> Result<()> {
    debug!("Getting note by tag: {:?}", tag);
    debug!("Note ID: {:?}", target_note_id);
    loop {
        // Sync the state and add the tag
        client.sync_state().await?;
        client.add_note_tag(tag).await?;

        trace!(
            "All input notes: {:?}",
            client.get_input_notes(NoteFilter::All).await?
        );
        trace!(
            "All output notes: {:?}",
            client.get_output_notes(NoteFilter::All).await?
        );
        // Fetch notes
        let notes = client.get_consumable_notes(None).await?;
        trace!("Notes len: {:?}", notes.len());
        // Check if any note matches the target_note_id
        let found = notes.iter().any(|(note, _)| note.id() == target_note_id);

        if found {
            debug!("Found the note with ID: {:?}", target_note_id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }

    Ok(())
}

// --------------------------------------------------------------------------
// Assembly Utilities
// --------------------------------------------------------------------------

/// Creates a Miden assembly library from source code.
///
/// # Arguments
/// * `assembler`: The assembler instance to use
/// * `library_path`: Path identifier for the library
/// * `source_code`: MASM source code
pub fn create_library(
    assembler: Assembler,
    library_path: &str,
    source_code: &str,
) -> Result<miden_assembly::Library, Box<dyn std::error::Error>> {
    let source_manager = Arc::new(DefaultSourceManager::default());
    let module = Module::parser(ModuleKind::Library).parse_str(
        LibraryPath::new(library_path)?,
        source_code,
        &source_manager,
    )?;
    let library = assembler.clone().assemble_library([module])?;
    Ok(library)
}

// --------------------------------------------------------------------------
// Store Management
// --------------------------------------------------------------------------

/// Deletes the SQLite client store file.
///
/// Useful for cleaning up test environments or resetting client state.
pub async fn delete_client_store(store_path: &str) {
    if tokio::fs::metadata(store_path).await.is_ok() {
        if let Err(e) = tokio::fs::remove_file(store_path).await {
            warn!("Failed to remove {}: {}", store_path, e);
        } else {
            info!("Cleared sqlite store: {}", store_path);
        }
    } else {
        debug!("Store not found: {}", store_path);
    }
}

// --------------------------------------------------------------------------
// Account and Faucet Setup
// --------------------------------------------------------------------------

/// Creates multiple accounts and faucets with initial token balances.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore for managing account keys
/// * `num_accounts`: Number of regular accounts to create
/// * `num_faucets`: Number of faucets to create
/// * `balances`: Matrix where `balances[account_idx][faucet_idx]` specifies
///   how many tokens faucet `faucet_idx` should mint for account `account_idx`
///
/// # Returns
/// Tuple of `(accounts, faucets)` vectors
pub async fn setup_accounts_and_faucets(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore<StdRng>,
    num_accounts: usize,
    num_faucets: usize,
    balances: Vec<Vec<u64>>,
) -> Result<(Vec<Account>, Vec<Account>), ClientError> {
    // ---------------------------------------------------------------------
    // 1)  Create basic accounts
    // ---------------------------------------------------------------------
    let mut accounts = Vec::with_capacity(num_accounts);
    for i in 0..num_accounts {
        let (account, _) = create_basic_account(client, keystore.clone()).await?;
        info!("Created Account #{i} => ID: {:?}", account.id().to_hex());
        accounts.push(account);
    }

    // ---------------------------------------------------------------------
    // 2)  Create basic faucets
    // ---------------------------------------------------------------------
    let mut faucets = Vec::with_capacity(num_faucets);
    for j in 0..num_faucets {
        let faucet = create_basic_faucet(client, keystore.clone()).await?;
        info!("Created Faucet #{j} => ID: {:?}", faucet.id().to_hex());
        faucets.push(faucet);
    }

    // Tell the client about the new accounts/faucets
    client.sync_state().await?;

    // ---------------------------------------------------------------------
    // 3)  Mint tokens
    // ---------------------------------------------------------------------
    // `minted_notes[i]` collects the notes minted **for** `accounts[i]`
    let mut minted_notes: Vec<Vec<Note>> = vec![Vec::new(); num_accounts];

    for (acct_idx, account) in accounts.iter().enumerate() {
        for (faucet_idx, faucet) in faucets.iter().enumerate() {
            let amount = balances[acct_idx][faucet_idx];
            if amount == 0 {
                continue;
            }

            info!("Minting {amount} tokens from Faucet #{faucet_idx} to Account #{acct_idx}");

            // Build & submit the mint transaction
            let asset = FungibleAsset::new(faucet.id(), amount)
                .unwrap_or_else(|err| panic!("Failed to create fungible asset: {err:?}"));
            let tx_request = TransactionRequestBuilder::new()
                .build_mint_fungible_asset(asset, account.id(), NoteType::Public, client.rng())
                .unwrap_or_else(|err| {
                    panic!("Failed to build mint fungible asset request: {err:?}")
                });

            let tx_id = client
                .submit_new_transaction(faucet.id(), tx_request.clone())
                .await?;
            let transaction = client
                .get_transactions(TransactionFilter::Ids(vec![tx_id]))
                .await?
                .pop()
                .with_context(|| "failed to find transaction {tx_id:?} after submission")
                .unwrap_or_else(|err| {
                    panic!("Failed to find transaction after submission: {err:?}")
                });

            // Remember the freshly-created note so we can consume it later
            let minted_note = match transaction.details.output_notes.get_note(0) {
                OutputNote::Full(n) => n.clone(),
                _ => panic!("Expected OutputNote::Full, got something else"),
            };
            minted_notes[acct_idx].push(minted_note);
        }
    }

    // ---------------------------------------------------------------------
    // 4)  ONE wait-phase: ensure every account can now see all its notes
    // ---------------------------------------------------------------------
    for (acct_idx, account) in accounts.iter().enumerate() {
        let expected = minted_notes[acct_idx].len();
        if expected > 0 {
            wait_for_notes(client, account, expected).await?;
        }
    }
    info!("Note received.");

    client.sync_state().await?;
    debug!("Client sync succeeded.");

    // ---------------------------------------------------------------------
    // 5)  Consume notes so the tokens live in the public vaults
    // ---------------------------------------------------------------------
    for (acct_idx, account) in accounts.iter().enumerate() {
        for note in &minted_notes[acct_idx] {
            let consume_req = TransactionRequestBuilder::new()
                .authenticated_input_notes([(note.id(), None)])
                .build()
                .unwrap_or_else(|err| panic!("Failed to build consume request: {err:?}"));
            debug!("Built consume_req.");
            let _tx_id = client
                .submit_new_transaction(account.id(), consume_req)
                .await?;
        }
    }

    debug!("Client sync succeeded.");
    client.sync_state().await?;

    info!("Final client sync succeeded.");

    Ok((accounts, faucets))
}

/// Waits until a specified number of consumable notes are available for an account.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `account`: Account to check for notes
/// * `expected`: Minimum number of notes to wait for
pub async fn wait_for_notes(
    client: &mut MidenClient,
    account: &Account,
    expected: usize,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let notes = client.get_consumable_notes(Some(account.id())).await?;
        if notes.len() >= expected {
            break;
        }
        debug!(
            "{} consumable notes found for account {}. Waiting...",
            notes.len(),
            account.id().to_hex()
        );
        sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

/// Creates a basic regular account with updatable code.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore to store the account's authentication key
///
/// # Returns
/// Tuple of `(Account, AuthSecretKey)`
pub async fn create_basic_account(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore<StdRng>,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let mut init_seed = [0_u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_rpo_falcon512_with_rng(client.rng());
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::RegularAccountUpdatableCode)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key().to_commitment()))
        .with_component(BasicWallet);
    let account = builder
        .build()
        .unwrap_or_else(|err| panic!("Failed to build account: {err:?}"));
    client.add_account(&account, false).await?;
    keystore
        .add_key(&key_pair)
        .unwrap_or_else(|err| panic!("Failed to add key to keystore: {err:?}"));
    Ok((account, key_pair))
}

/// Creates a basic fungible faucet account.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `keystore`: Keystore to store the faucet's authentication key
///
/// # Returns
/// The created faucet `Account`
pub async fn create_basic_faucet(
    client: &mut MidenClient,
    keystore: FilesystemKeyStore<StdRng>,
) -> Result<Account, ClientError> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);
    let key_pair = AuthSecretKey::new_rpo_falcon512_with_rng(client.rng());
    let symbol = TokenSymbol::new("MID")
        .unwrap_or_else(|err| panic!("Failed to create token symbol: {err:?}"));
    let decimals = 8;
    let max_supply = Felt::new(1_000_000_000);
    let builder = AccountBuilder::new(init_seed)
        .account_type(AccountType::FungibleFaucet)
        .storage_mode(AccountStorageMode::Public)
        .with_auth_component(AuthRpoFalcon512::new(key_pair.public_key().to_commitment()))
        .with_component(
            BasicFungibleFaucet::new(symbol, decimals, max_supply)
                .unwrap_or_else(|err| panic!("Failed to create faucet: {err:?}")),
        );
    let account = builder
        .build()
        .unwrap_or_else(|err| panic!("Failed to build faucet account: {err:?}"));
    client.add_account(&account, false).await?;
    keystore
        .add_key(&key_pair)
        .unwrap_or_else(|err| panic!("Failed to add key to keystore: {err:?}"));
    Ok(account)
}

// --------------------------------------------------------------------------
// Note Waiting Utilities
// --------------------------------------------------------------------------

/// Waits for a specific note to become consumable.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `_account_id`: Account ID (unused but kept for API compatibility)
/// * `expected`: The note to wait for
pub async fn wait_for_note(
    client: &mut MidenClient,
    _account_id: &Account,
    expected: &Note,
) -> Result<(), ClientError> {
    loop {
        client.sync_state().await?;
        let notes: Vec<(InputNoteRecord, Vec<(AccountId, NoteRelevance)>)> =
            client.get_consumable_notes(None).await?;
        let found = notes.iter().any(|(rec, _)| rec.id() == expected.id());
        if found {
            info!("Note found {}", expected.id().to_hex());
            break;
        }
        debug!("Note {} not found. Waiting...", expected.id().to_hex());
        sleep(Duration::from_secs(3)).await;
    }
    Ok(())
}

/// Waits for any consumable notes for a given account.
///
/// # Arguments
/// * `client`: Miden client instance
/// * `account_id`: Account ID to check for notes
///
/// # Returns
/// Vector of consumable note records with their relevance info
pub async fn wait_for_consumable_notes(
    client: &mut MidenClient,
    account_id: AccountId,
) -> Result<Vec<(InputNoteRecord, Vec<(AccountId, NoteRelevance)>)>> {
    loop {
        client.sync_state().await?;
        let notes = client.get_consumable_notes(Some(account_id)).await?;
        if !notes.is_empty() {
            return Ok(notes);
        }
        debug!(
            "Consumable notes for account {:?} ({}) not found. Waiting...",
            account_id,
            account_id.to_hex()
        );
        sleep(Duration::from_secs(2)).await;
    }
}

// --------------------------------------------------------------------------
// Utility Functions
// --------------------------------------------------------------------------

/// Converts an account ID to a note tag.
///
/// This is useful for filtering notes by recipient account.
pub fn account_id_to_note_tag(account_id: AccountId) -> NoteTag {
    let address = Address::new(account_id);
    address.to_note_tag()
}
