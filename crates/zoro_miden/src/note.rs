use std::{fs::read_to_string, path::PathBuf, str::FromStr, sync::OnceLock};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use miden_client::{
    Felt, Word,
    account::AccountId,
    assembly::CodeBuilder,
    asset::Asset,
    note::{
        Note, NoteAssets, NoteInputs, NoteMetadata, NoteRecipient, NoteTag, NoteType,
        WellKnownNote, build_p2id_recipient,
    },
    store::InputNoteRecord,
    transaction::TransactionKernel,
};
use miden_protocol::utils::{Deserializable, Serializable};
use tracing::info;

use crate::client::create_library;

static NOTE_ROOTS: OnceLock<NoteRoots> = OnceLock::new();

fn get_note_roots() -> &'static NoteRoots {
    NOTE_ROOTS.get_or_init(|| {
        info!("Initializing Note roots ...");
        NoteRoots::generate_from_notes().expect("Generating Note Roots failed.")
    })
}

#[derive(Copy, Clone, Debug)]
pub enum NoteKind {
    P2ID,
    Deposit,
    Withdraw,
    Swap,
}

pub struct TrustedNote {
    note: Note,
    note_kind: NoteKind,
}

impl TrustedNote {
    pub fn new_p2id(
        sender: AccountId,
        target: AccountId,
        assets: Vec<Asset>,
        note_type: NoteType,
        serial_num: Word,
    ) -> Result<Self> {
        let recipient = build_p2id_recipient(target, serial_num)?;
        let tag = NoteTag::with_account_target(target);
        let metadata = NoteMetadata::new(sender, note_type, tag);
        let vault = NoteAssets::new(assets)?;
        let note = Note::new(vault, metadata, recipient);
        Ok(TrustedNote {
            note,
            note_kind: NoteKind::P2ID,
        })
    }

    pub fn new_deposit(
        inputs: Vec<Felt>,
        assets: Vec<Asset>,
        creator: AccountId,
        swap_serial_num: Word,
        note_tag: NoteTag,
        note_type: NoteType,
    ) -> Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);
        let path: PathBuf = [manifest_dir, "masm", "notes", "DEPOSIT.masm"]
            .iter()
            .collect();
        let note_code = read_to_string(&path)
            .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
        let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
            .iter()
            .collect();
        let pool_code = read_to_string(&pool_code_path)
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
        Ok(Self {
            note,
            note_kind: NoteKind::Deposit,
        })
    }

    pub async fn new_withdraw(
        inputs: Vec<Felt>,
        assets: Vec<Asset>,
        creator: AccountId,
        swap_serial_num: Word,
        note_tag: NoteTag,
        note_type: NoteType,
    ) -> Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);
        let path: PathBuf = [manifest_dir, "masm", "notes", "WITHDRAW.masm"]
            .iter()
            .collect();
        let note_code = read_to_string(&path)
            .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
        let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
            .iter()
            .collect();
        let pool_code = read_to_string(&pool_code_path)
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
        Ok(Self {
            note,
            note_kind: NoteKind::Withdraw,
        })
    }
    pub async fn new_swap(
        inputs: Vec<Felt>,
        assets: Vec<Asset>,
        creator: AccountId,
        swap_serial_num: Word,
        note_tag: NoteTag,
        note_type: NoteType,
    ) -> Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);
        let path: PathBuf = [manifest_dir, "masm", "notes", "ZOROSWAP.masm"]
            .iter()
            .collect();
        let note_code = read_to_string(&path)
            .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
        let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
            .iter()
            .collect();
        let pool_code = read_to_string(&pool_code_path)
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
        Ok(Self {
            note,
            note_kind: NoteKind::Swap,
        })
    }

    pub fn print_note_info(&self) {
        info!(
            "View note on MidenScan: https://testnet.midenscan.com/note/{}",
            self.note.id().to_hex()
        );
    }

    pub fn from_note(note: Note) -> Result<Self> {
        let root = note.script().root();
        let known_roots = get_note_roots();
        let note_kind = known_roots.get_order_type(&root)?;
        Ok(Self { note, note_kind })
    }
    pub fn from_input_note(input_note_record: &InputNoteRecord) -> Result<Self> {
        let note = Note::new(
            input_note_record.assets().clone(),
            input_note_record
                .metadata()
                .ok_or(anyhow!("Missing note metadata"))?
                .clone(),
            input_note_record.details().recipient().clone(),
        );
        let root = note.script().root();
        let known_roots = get_note_roots();
        let note_kind = known_roots.get_order_type(&root)?;
        Ok(Self { note, note_kind })
    }

    pub fn from_base64(encoded: &str) -> Result<Self> {
        // base64 -> bytes
        let note_bytes = general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| anyhow!("Failed to decode base64: {}", e))?;
        // bytes -> note
        let note = Note::read_from_bytes(&note_bytes)
            .map_err(|e| anyhow!("Failed to deserialize note: {}", e))?;
        let trusted_note = Self::from_note(note)?;
        Ok(trusted_note)
    }

    pub fn note(&self) -> &Note {
        &self.note
    }

    pub fn note_kind(&self) -> &NoteKind {
        &self.note_kind
    }

    pub fn serialize_to_string(&self) -> Result<String> {
        let note_bytes: Vec<u8> = self.note.to_bytes();
        let encoded = general_purpose::STANDARD.encode(note_bytes);
        Ok(encoded)
    }
}

struct NoteRoots {
    deposit: Word,
    withdraw: Word,
    swap: Word,
    p2id: Word,
}

impl NoteRoots {
    pub fn generate_from_notes() -> Result<Self> {
        Ok(Self {
            deposit: get_script_root_for_local_script("DEPOSIT.masm")?,
            withdraw: get_script_root_for_local_script("WITHDRAW.masm")?,
            swap: get_script_root_for_local_script("ZOROSWAP.masm")?,
            p2id: WellKnownNote::P2ID.script_root(),
        })
    }

    pub fn get_order_type(&self, root: &Word) -> Result<NoteKind> {
        if root.eq(&self.deposit) {
            Ok(NoteKind::Deposit)
        } else if root.eq(&self.withdraw) {
            Ok(NoteKind::Withdraw)
        } else if root.eq(&self.swap) {
            Ok(NoteKind::Swap)
        } else if root.eq(&self.p2id) {
            Ok(NoteKind::P2ID)
        } else {
            Err(anyhow!(
                "Passed note root does not belong to a known note kind."
            ))
        }
    }
}

pub fn get_script_root_for_local_script(masm_name: &str) -> Result<Word> {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);
    let path: PathBuf = [manifest_dir, "masm", "notes", masm_name].iter().collect();
    let note_code = read_to_string(&path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", path.display(), err));
    let pool_code_path: PathBuf = [manifest_dir, "masm", "accounts", "zoropool.masm"]
        .iter()
        .collect();
    let pool_code = std::fs::read_to_string(&pool_code_path)
        .unwrap_or_else(|err| panic!("Error reading {}: {}", pool_code_path.display(), err));
    let pool_component_lib =
        create_library(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();
    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();
    Ok(note_script.root())
}
