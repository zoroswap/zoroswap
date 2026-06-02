use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};

use miden_client::{
    Client, DebugMode, Felt, RemoteTransactionProver,
    account::{
        Account, AccountBuilder, AccountId, AccountStorageMode, AccountType,
        component::{AuthControlled, BasicFungibleFaucet},
    },
    address::NetworkId,
    asset::{FungibleAsset, TokenSymbol},
    auth::{AuthScheme, AuthSecretKey, AuthSingleSig},
    builder::ClientBuilder,
    keystore::{FilesystemKeyStore, Keystore},
    note::{Note, NoteScreener, NoteTag, NoteType},
    rpc::{Endpoint, GrpcClient},
    store::{NoteFilter, TransactionFilter},
    sync::{StateSync, StateSyncInput},
    transaction::{TransactionId, TransactionRequestBuilder},
};
use miden_client_sqlite_store::{ClientBuilderSqliteExt, SqliteStore};
use rand::RngCore;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::{
    account::MidenAccount,
    note::{NoteKind, TrustedNote},
};

pub struct MidenClient {
    client: Client<FilesystemKeyStore>,
    state_sync: StateSync,
    endpoint: Endpoint,
    store_path: PathBuf,
    keystore_path: PathBuf,
    store_dir: String,
    keystore_dir: String,
}

impl MidenClient {
    pub async fn new(endpoint: Endpoint, keystore_dir: &str, store_dir: &str) -> Result<Self> {
        let timeout_ms = 30_000;
        let rpc_client = Arc::new(GrpcClient::new(&endpoint, timeout_ms));
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let store_path: PathBuf = [manifest_dir, store_dir].iter().collect();
        let keystore_path: PathBuf = [manifest_dir, keystore_dir].iter().collect();

        std::fs::create_dir_all(store_path.clone())
            .map_err(|e| anyhow!("Error creating new store dir for miden client: {:?}", e))?;
        std::fs::create_dir_all(keystore_path.clone())
            .map_err(|e| anyhow!("Error creating new keystore dir for miden client: {:?}", e))?;

        let name = format!("{:?}.sqlite3", Uuid::new_v4());
        let store_path = store_path.join(name);

        let keystore = Arc::new(
            FilesystemKeyStore::new(keystore_path.clone()).unwrap_or_else(|err| {
                panic!("Failed to create keystore at {:?}: {err:?}", keystore_path)
            }),
        );

        info!(
            keystore_path = ?keystore_path,
            store_path= ?store_path,
            endpoint = ?endpoint.to_network_id().to_string(),
            "Creating a new Miden Client"
        );

        let mut client = ClientBuilder::new()
            .rpc(rpc_client.clone())
            .authenticator(keystore.clone())
            .sqlite_store(store_path.clone())
            .in_debug_mode(DebugMode::Enabled);

        if endpoint.ne(&Endpoint::localhost()) {
            let url = if endpoint.eq(&Endpoint::testnet()) {
                "https://tx-prover.testnet.miden.io"
            } else {
                "https://tx-prover.devnet.miden.io"
            };
            let prover = Arc::new(RemoteTransactionProver::new(url));
            client = client.prover(prover);
        }

        let mut client = client.build().await?;
        client.ensure_genesis_in_place().await?;
        client.sync_state().await?;

        let sqlite_store = Arc::new(SqliteStore::new(store_path.clone()).await?);
        let note_screener = NoteScreener::new(sqlite_store.clone(), rpc_client.clone());
        let state_sync = StateSync::new(
            rpc_client,
            Some(sqlite_store),
            Arc::new(note_screener),
            None,
        );
        Ok(Self {
            client,
            state_sync,
            store_path,
            keystore_path,
            store_dir: store_dir.into(),
            keystore_dir: keystore_dir.into(),
            endpoint,
        })
    }
    pub async fn add_note_tag(&mut self, note_tag: NoteTag) -> Result<()> {
        self.client.add_note_tag(note_tag).await?;
        Ok(())
    }
    pub async fn import_account(&mut self, account_id: &AccountId) -> Result<()> {
        if let None = self.client.get_account(*account_id).await?
            && let Err(e) = self.client.import_account_by_id(*account_id).await
        {
            warn!("Error importing an account into client {e:?}");
        } else {
            self.client.sync_state().await?;
        }
        Ok(())
    }
    pub async fn partial_sync_state(&mut self, account_id: &AccountId) -> Result<()> {
        let accounts = match self.client.account_reader(*account_id).header().await {
            Ok((header, _)) => vec![header],
            Err(_) => vec![],
        };
        let note_tags = BTreeSet::new();
        let input_notes = vec![];
        let output_notes = self.client.get_output_notes(NoteFilter::Expected).await?;
        let uncommitted_transactions = self
            .client
            .get_transactions(TransactionFilter::Uncommitted)
            .await?;

        let mut current_partial_mmr = self.client.get_current_partial_mmr().await?;

        let input = StateSyncInput {
            accounts,
            note_tags,
            input_notes,
            output_notes,
            uncommitted_transactions,
        };

        let state_sync_update = self
            .state_sync
            .sync_state(&mut current_partial_mmr, input)
            .await?;

        self.client.apply_state_sync(state_sync_update).await?;
        Ok(())
    }
    pub async fn sync_state(&mut self) -> Result<()> {
        self.client.sync_state().await?;
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
            info!("{} notes available", consumable_notes.len());
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
                account_id.to_bech32(self.endpoint.to_network_id())
            );
            sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn wait_for_trusted_notes(
        &mut self,
        account_id: &AccountId,
        note_kind: Option<NoteKind>,
        expected: usize,
    ) -> Result<Vec<TrustedNote>> {
        loop {
            self.sync_state().await?;
            let consumable_notes = self.client.get_consumable_notes(Some(*account_id)).await?;
            let notes: Vec<TrustedNote> = consumable_notes
                .iter()
                .filter_map(|(rec, _)| {
                    let metadata = rec.metadata()?;
                    let note = Note::new(
                        rec.details().assets().clone(),
                        metadata.clone(),
                        rec.details().recipient().clone(),
                    );
                    if let Ok(trusted_note) = TrustedNote::from_note(note) {
                        if let Some(note_kind) = note_kind {
                            if note_kind.eq(trusted_note.note_kind()) {
                                Some(trusted_note)
                            } else {
                                None
                            }
                        } else {
                            Some(trusted_note)
                        }
                    } else {
                        None
                    }
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
        info!(
            "Minting asset {} for account: {}",
            faucet_id.to_bech32(self.endpoint.to_network_id()),
            account_id.to_bech32(self.endpoint.to_network_id())
        );
        self.sync_state().await?;
        self.import_account(&faucet_id).await?;
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
        Self::print_transaction_info(&tx_id);
        self.sync_state().await?;
        self.consume_simple_notes(&account_id, 1).await?;
        Ok(format!("{:?}", tx_id))
    }

    pub async fn consume_simple_notes(
        &mut self,
        account_id: &AccountId,
        n_notes: usize,
    ) -> Result<String> {
        info!(
            "Consuming {} notes for account {}",
            n_notes,
            account_id.to_bech32(self.endpoint.to_network_id())
        );
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

    pub fn network_id(&self) -> NetworkId {
        self.endpoint.to_network_id()
    }

    pub async fn send_note(
        &mut self,
        account_id: &AccountId,
        target_account_id: &AccountId,
        note: TrustedNote,
    ) -> Result<()> {
        self.import_account(target_account_id).await?;
        let note_req = TransactionRequestBuilder::new()
            .own_output_notes(vec![note.note().clone().into()])
            .build()
            .unwrap();
        let tx_id = self
            .client_mut()
            .submit_new_transaction(*account_id, note_req)
            .await?;
        MidenClient::print_transaction_info(&tx_id);
        Ok(())
    }
    pub async fn send_note_untrusted(
        &mut self,
        account_id: &AccountId,
        // target_account_id: &AccountId,
        note: Note,
    ) -> Result<()> {
        // self.import_account(target_account_id).await?;
        let note_req = TransactionRequestBuilder::new()
            .own_output_notes(vec![note.clone().into()])
            .build()
            .unwrap();
        let tx_id = self
            .client_mut()
            .submit_new_transaction(*account_id, note_req)
            .await?;
        MidenClient::print_transaction_info(&tx_id);
        Ok(())
    }

    pub fn print_transaction_info(tx: &TransactionId) {
        info!(
            "View transaction on MidenScan: https://testnet.midenscan.com/tx/{}",
            tx.to_hex()
        );
    }

    pub async fn deploy_new_faucet(
        &mut self,
        symbol: &str,
        decimals: u8,
        max_supply: u64,
    ) -> Result<MidenAccount> {
        let mut init_seed = [0u8; 32];
        let keystore: FilesystemKeyStore = FilesystemKeyStore::new(self.keystore_path())
            .map_err(|err| panic!("Failed to create keystore: {err:?}"))?;
        let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(self.client_mut().rng());
        self.client_mut().rng().fill_bytes(&mut init_seed);
        let builder = AccountBuilder::new(init_seed)
            .account_type(AccountType::FungibleFaucet)
            .storage_mode(AccountStorageMode::Public)
            .with_auth_component(AuthSingleSig::new(
                key_pair.public_key().to_commitment(),
                AuthScheme::Falcon512Poseidon2,
            ))
            .with_component(AuthControlled::allow_all())
            .with_component(
                BasicFungibleFaucet::new(
                    TokenSymbol::new(symbol)?,
                    decimals,
                    Felt::new(max_supply),
                )
                .unwrap_or_else(|err| panic!("Failed to create BasicFungibleFaucet: {err:?}")),
            );
        let faucet_account = builder
            .build()
            .unwrap_or_else(|err| panic!("Failed to build faucet account: {err:?}"));
        self.client_mut().add_account(&faucet_account, true).await?;
        keystore.add_key(&key_pair, faucet_account.id()).await?;
        let transaction_request = TransactionRequestBuilder::new().build()?;
        let _tx_id = self
            .client_mut()
            .submit_new_transaction(faucet_account.id(), transaction_request)
            .await?;
        println!(
            "Faucet account ID ({}): {:?}",
            symbol,
            faucet_account.id().to_bech32(self.endpoint.to_network_id())
        );
        self.sync_state().await?;
        Ok(MidenAccount::new(faucet_account.id(), Some(faucet_account)))
    }

    pub fn keystore_dir(&self) -> String {
        self.keystore_dir.clone()
    }
    pub fn store_dir(&self) -> String {
        self.store_dir.clone()
    }
    pub fn keystore_path(&self) -> PathBuf {
        self.keystore_path.clone()
    }
    pub fn store_path(&self) -> PathBuf {
        self.store_path.clone()
    }
}

impl Drop for MidenClient {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(self.store_path()) {
            warn!("Error deleting store on client drop: {e:?}");
        }
    }
}

// --------------------------------------------------------------------------
// Assembly Utilities
// --------------------------------------------------------------------------

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

pub async fn delete_client_store(store_dir: &str) {
    if let Err(e) = fs::remove_dir_all(store_dir) {
        warn!(e = ?e, "Warning on removing store dir");
    }
    if let Err(e) = fs::create_dir_all(store_dir) {
        warn!(e = ?e, "Warning on creating store dir");
    }
}
