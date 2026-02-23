use std::{collections::BTreeSet, sync::Arc, time::Duration};

use anyhow::Result;
use miden_assembly::{
    Assembler, DefaultSourceManager,
    ast::{Module, ModuleKind},
};
use miden_client::{
    Client, DebugMode,
    account::{Account, AccountId},
    asset::FungibleAsset,
    builder::ClientBuilder,
    keystore::FilesystemKeyStore,
    note::{Note, NoteScreener, NoteTag, NoteType},
    rpc::{Endpoint, GrpcClient},
    store::{NoteFilter, TransactionFilter},
    sync::StateSync,
    transaction::{TransactionId, TransactionRequestBuilder},
};
use miden_client_sqlite_store::{ClientBuilderSqliteExt, SqliteStore};
use tokio::time::sleep;
use tracing::{debug, info, warn};

// --------------------------------------------------------------------------
// Type Aliases
// --------------------------------------------------------------------------

pub struct MidenClient {
    client: Client<FilesystemKeyStore>,
    partial_state_sync: Option<(AccountId, StateSync)>,
}

impl MidenClient {
    pub async fn new(
        endpoint: Endpoint,
        keystore_path: &str,
        store_path: &str,
        partial_account_sync: Option<AccountId>,
    ) -> Result<Self> {
        info!(
            keystore_path = keystore_path,
            store_path = store_path,
            "Creating a new Miden Client"
        );
        let timeout_ms = 30_000;
        let rpc_client = Arc::new(GrpcClient::new(&endpoint, timeout_ms));
        let keystore = Arc::new(
            FilesystemKeyStore::new(keystore_path.into()).unwrap_or_else(|err| {
                panic!("Failed to create keystore at {}: {err:?}", keystore_path)
            }),
        );

        let mut client = ClientBuilder::new()
            .rpc(rpc_client.clone())
            .authenticator(keystore.clone())
            .sqlite_store(store_path.into())
            .in_debug_mode(DebugMode::Enabled)
            .build()
            .await?;
        client.ensure_genesis_in_place().await?;
        client.sync_state().await?;

        let partial_state_sync = if let Some(acc_id) = partial_account_sync {
            let sqlite_store = Arc::new(SqliteStore::new(store_path.into()).await?);
            let note_screener = NoteScreener::new(sqlite_store, Some(keystore));
            let state_sync = StateSync::new(rpc_client, Arc::new(note_screener), None);
            Some((acc_id, state_sync))
        } else {
            None
        };
        Ok(Self {
            client,
            partial_state_sync,
        })
    }
    pub async fn add_note_tag(&mut self, note_tag: NoteTag) -> Result<()> {
        self.client.add_note_tag(note_tag).await?;
        Ok(())
    }
    pub async fn import_account(&mut self, account_id: &AccountId) -> Result<()> {
        if let Err(e) = self.client.import_account_by_id(*account_id).await {
            warn!("Error importing an account into client {e:?}");
        }
        Ok(())
    }
    pub async fn sync_state(&mut self) -> Result<()> {
        if let Some((account_id, state_sync)) = &self.partial_state_sync {
            let accounts = self
                .client
                .get_account_header_by_id(*account_id)
                .await?
                .map(|(header, _)| vec![header])
                .unwrap_or_default();
            let note_tags = BTreeSet::new();
            let input_notes = vec![];
            let expected_output_notes = self.client.get_output_notes(NoteFilter::Expected).await?;
            let uncommitted_transactions = self
                .client
                .get_transactions(TransactionFilter::Uncommitted)
                .await?;

            // Build current partial MMR
            let current_partial_mmr = self.client.get_current_partial_mmr().await?;

            // Get the sync update from the network
            let state_sync_update = state_sync
                .sync_state(
                    current_partial_mmr,
                    accounts,
                    note_tags,
                    input_notes,
                    expected_output_notes,
                    uncommitted_transactions,
                )
                .await?;

            // Apply received and computed updates to the store
            self.client.apply_state_sync(state_sync_update).await?;
        } else {
            self.client.sync_state().await?;
        }
        Ok(())
    }
    pub fn client(&self) -> &Client<FilesystemKeyStore> {
        &self.client
    }
    pub fn client_mut(&mut self) -> &mut Client<FilesystemKeyStore> {
        &mut self.client
    }
    pub async fn wait_for_note(&mut self, _account_id: &Account, expected: &Note) -> Result<()> {
        loop {
            self.sync_state().await?;
            let notes = self.client.get_consumable_notes(None).await?;
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

    pub async fn wait_for_notes(
        &mut self,
        account_id: &AccountId,
        expected: usize,
    ) -> Result<Vec<Note>> {
        loop {
            self.sync_state().await?;
            let consumable_notes = self.client.get_consumable_notes(Some(*account_id)).await?;
            let notes: Vec<Note> = consumable_notes
                .iter()
                .filter_map(|(rec, _)| {
                    let metadata = rec.metadata()?;
                    Some(Note::new(
                        rec.details().assets().clone(),
                        metadata.clone(),
                        rec.details().recipient().clone(),
                    ))
                })
                .collect();

            if notes.len() >= expected {
                return Ok(notes);
            }
            debug!(
                "{} consumable notes found for account {}. Waiting...",
                notes.len(),
                account_id.to_hex()
            );
            sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn fetch_new_notes_by_tag(&mut self, pool_id_tag: &NoteTag) -> Result<Vec<Note>> {
        self.sync_state().await?;
        let all_notes = self.client.get_output_notes(NoteFilter::Committed).await?;
        let notes: Vec<Note> = all_notes
            .iter()
            .filter_map(|n| {
                if n.metadata().tag().eq(pool_id_tag)
                    && let Some(recipient) = n.recipient()
                {
                    let note =
                        Note::new(n.assets().clone(), n.metadata().clone(), recipient.clone());
                    Some(note)
                } else {
                    None
                }
            })
            .collect();
        Ok(notes)
    }

    pub async fn mint_asset(
        &mut self,
        faucet_id: AccountId,
        account_id: AccountId,
        amount: u64,
    ) -> Result<String> {
        let fungible_asset = FungibleAsset::new(faucet_id, amount)?;
        let transaction_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            account_id,
            NoteType::Public,
            self.client.rng(),
        )?;
        let tx_id = self
            .client
            .submit_new_transaction(faucet_id, transaction_request)
            .await?;
        Ok(format!("{:?}", tx_id))
    }

    pub async fn consume_notes(
        &mut self,
        account_id: &AccountId,
        n_notes: usize,
    ) -> Result<String> {
        let notes = self.wait_for_notes(account_id, n_notes).await?;
        let transaction_request = TransactionRequestBuilder::new().build_consume_notes(notes)?;
        let tx_id = self
            .client_mut()
            .submit_new_transaction(*account_id, transaction_request)
            .await?;
        self.sync_state().await?;
        Self::print_transaction_info(&tx_id);
        Ok(format!("{:?}", tx_id))
    }

    pub fn print_transaction_info(tx: &TransactionId) {
        info!(
            "View transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
            tx.to_hex()
        );
    }
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
    let path = miden_assembly::Path::new(library_path);
    let module =
        Module::parser(ModuleKind::Library).parse_str(path, source_code, source_manager)?;
    let library = assembler.clone().assemble_library([module])?;
    Ok(library)
}

pub fn print_library_exports(masm_lib: &miden_assembly::Library) {
    println!("+++++Masm lib exports:");
    masm_lib.exports().for_each(|export| {
        let path = export.path();
        if let Some(root) = masm_lib.get_procedure_root_by_path(&path) {
            println!("Export: {:?} {:?} {:?}", path, root, root.to_hex());
        } else {
            println!("Export: {:?} (no procedure root)", path);
        }
    });
}

pub fn print_contract_procedures(pool_contract: &Account) {
    println!("+++++Pool contract procedures");
    pool_contract.code().procedures().iter().for_each(|proc| {
        println!("Proc root: {:?} ", proc.mast_root().to_hex());
    });
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
        debug!("Store not found for deleting: {}", store_path);
    }
}
