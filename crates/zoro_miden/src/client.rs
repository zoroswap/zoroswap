use std::sync::Arc;

use miden_assembly::{
    Assembler, DefaultSourceManager,
    ast::{Module, ModuleKind},
};
use miden_client::{
    Client, ClientError,
    account::Account,
    builder::ClientBuilder,
    keystore::FilesystemKeyStore,
    rpc::{Endpoint, GrpcClient},
};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use tracing::{debug, info, warn};

// --------------------------------------------------------------------------
// Type Aliases
// --------------------------------------------------------------------------
pub type MidenClient = Client<FilesystemKeyStore>;

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
