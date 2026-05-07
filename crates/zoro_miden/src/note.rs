use std::{collections::HashSet, fs::read_to_string, path::PathBuf, sync::OnceLock};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use chrono::{TimeZone, Utc};
use miden_client::{
    Deserializable, Felt, Serializable, Word,
    account::AccountId,
    address::NetworkId,
    assembly::CodeBuilder,
    asset::{Asset, FungibleAsset},
    note::{Note, NoteAssets, NoteMetadata, NoteRecipient, NoteStorage, NoteTag, NoteType},
    store::InputNoteRecord,
    transaction::TransactionKernel,
};
use miden_protocol::note::NoteScript;
use miden_standards::note::{P2idNoteStorage, StandardNote};
use rand::{Rng, SeedableRng, rngs::StdRng};
use tracing::info;

use crate::assembly_utils::{create_library_with_assembler, link_all_libraries};
use crate::asset_utils::{asset_to_word, word_to_asset};

static NOTE_ROOTS: OnceLock<NoteRoots> = OnceLock::new();

fn get_note_roots() -> &'static NoteRoots {
    NOTE_ROOTS.get_or_init(|| {
        info!("Initializing Note roots ...");
        NoteRoots::generate_from_notes().expect("Generating Note Roots failed.")
    })
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NoteKind {
    P2ID,
    Deposit,
    Withdraw,
    Swap,
}

impl NoteKind {
    pub fn masm_name(&self) -> &str {
        match self {
            NoteKind::P2ID => "P2ID.masm",
            NoteKind::Deposit => "DEPOSIT.masm",
            NoteKind::Withdraw => "WITHDRAW.masm",
            NoteKind::Swap => "ZOROSWAP.masm",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrustedNote {
    note: Note,
    note_kind: NoteKind,
    serial_number: Word,
    created_at: i64,
}

impl TrustedNote {
    pub fn new(note_instructions: NoteInstructions, code_builder: CodeBuilder) -> Result<Self> {
        let note_elements: TrustedNoteElements = note_instructions.try_into()?;
        match note_elements.note_kind {
            NoteKind::P2ID => Self::new_p2id(note_elements),
            NoteKind::Deposit | NoteKind::Withdraw | NoteKind::Swap => {
                Self::new_zoro_note(note_elements, code_builder)
            }
        }
    }

    fn new_p2id(note_elements: TrustedNoteElements) -> Result<Self> {
        let target = note_elements
            .target
            .ok_or(anyhow!("Missing target for p2id note."))?;
        let serial_number = if let Some(serial_num) = note_elements.referential_serial_number {
            // when returnin p2ids for zoro notes
            let p2id_serial_num: Word = [
                serial_num[0] + Felt::new(1),
                serial_num[1],
                serial_num[2],
                serial_num[3],
            ]
            // let p2id_serial_num: Word = [
            //     serial_num[3],
            //     serial_num[2],
            //     serial_num[1],
            //     serial_num[0] + Felt::new(1),
            // ]
            .into();
            Ok(p2id_serial_num)
        } else {
            Self::random_word()
        }?;
        let recipient = P2idNoteStorage::new(target).into_recipient(serial_number);
        let note = Note::new(note_elements.assets, note_elements.metadata, recipient);
        Ok(TrustedNote {
            note,
            note_kind: NoteKind::P2ID,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
        })
    }
    pub fn get_note_script(code_builder: CodeBuilder, note_file_name: &str) -> Result<NoteScript> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let note_path = PathBuf::from_iter(&[manifest_dir, "masm", "notes", note_file_name]);
        // let pool_path = PathBuf::from_iter(&[manifest_dir, "masm", "accounts", "zoropool.masm"]);
        let note_code = read_to_string(&note_path)
            .map_err(|e| anyhow!("Error parsing note code at path {note_path:?}: {e:?}"))?;

        let code_builder = link_all_libraries(code_builder.clone())?;
        code_builder
            .compile_note_script(note_code)
            .map_err(|e| anyhow!("Failed to compile note script: {}", e))
    }

    fn new_zoro_note(
        note_elements: TrustedNoteElements,
        code_builder: CodeBuilder,
    ) -> Result<Self> {
        let note_script = Self::get_note_script(code_builder, note_elements.note_kind.masm_name())?;
        let serial_number = Self::random_word()?;
        let recipient = NoteRecipient::new(serial_number, note_script, note_elements.inputs);
        let note = Note::new(note_elements.assets, note_elements.metadata, recipient);
        Ok(Self {
            note,
            note_kind: note_elements.note_kind,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
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
        let serial_number = note.serial_num();
        Ok(Self {
            note,
            note_kind,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
        })
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
        let serial_number = note.serial_num();
        Ok(Self {
            note,
            note_kind,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
        })
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
    pub fn build_p2id(
        target: AccountId,
        asset_in: FungibleAsset,
        referential_serial_number: Option<Word>,
    ) -> Result<Self> {
        let p2id_note = TrustedNote::new(
            NoteInstructions::P2ID(P2IDInstructions {
                asset_in,
                target,
                referential_serial_number,
                note_type: NoteType::Public,
            }),
            CodeBuilder::new(),
        )?;
        Ok(p2id_note)
    }
    pub fn note(&self) -> &Note {
        &self.note
    }
    pub fn note_kind(&self) -> &NoteKind {
        &self.note_kind
    }
    pub fn serial_number(&self) -> Word {
        self.serial_number
    }
    pub fn created_at(&self) -> i64 {
        self.created_at
    }

    pub fn serialize_to_string(&self) -> Result<String> {
        let note_bytes: Vec<u8> = self.note.to_bytes();
        let encoded = general_purpose::STANDARD.encode(note_bytes);
        Ok(encoded)
    }

    pub fn random_word() -> Result<Word> {
        let mut rng = StdRng::from_os_rng();
        let felts = [
            Felt::new(rng.random::<u64>() >> 1),
            Felt::new(rng.random::<u64>() >> 1),
            Felt::new(rng.random::<u64>() >> 1),
            Felt::new(rng.random::<u64>() >> 1),
        ];
        Ok(Word::new(felts))
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
            p2id: StandardNote::P2ID.script_root(),
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
        create_library_with_assembler(assembler.clone(), "zoroswap::zoropool", &pool_code).unwrap();
    let note_script = CodeBuilder::new()
        .with_dynamically_linked_library(&pool_component_lib)
        .unwrap()
        .compile_note_script(note_code)
        .unwrap();
    Ok(note_script.root())
}

#[derive(Clone, Copy, Debug)]
pub enum NoteInstructions {
    Deposit(DepositInstructions),
    Withdraw(WithdrawInstructions),
    Swap(SwapInstructions),
    P2ID(P2IDInstructions),
}

impl NoteInstructions {
    pub fn note_kind(&self) -> NoteKind {
        match self {
            NoteInstructions::Deposit(_) => NoteKind::Deposit,
            NoteInstructions::Withdraw(_) => NoteKind::Withdraw,
            NoteInstructions::Swap(_) => NoteKind::Swap,
            NoteInstructions::P2ID(_) => NoteKind::P2ID,
        }
    }

    pub fn to_pretty_info(&self, network_id: NetworkId) -> String {
        match self {
            NoteInstructions::Deposit(i) => {
                let deadline = match Utc.timestamp_millis_opt(i.deadline as i64) {
                    chrono::LocalResult::Single(r) => r.to_rfc3339(),
                    chrono::LocalResult::Ambiguous(r, _t) => r.to_rfc3339(),
                    chrono::LocalResult::None => "Invalid time".to_string(),
                };
                format!(
                    "Deposit of {} -> min LP: {} for faucet {}. From user {} (tag {}) with deadline {}. Type: {}.",
                    i.asset_in.amount(),
                    i.min_lp_amount_out,
                    i.asset_in.faucet_id().to_bech32(network_id.clone()),
                    i.creator.to_bech32(network_id.clone()),
                    i.p2id_tag.as_u32(),
                    deadline,
                    i.note_type,
                )
            }
            NoteInstructions::Withdraw(i) => {
                let deadline = match Utc.timestamp_millis_opt(i.deadline as i64) {
                    chrono::LocalResult::Single(r) => r.to_rfc3339(),
                    chrono::LocalResult::Ambiguous(r, _t) => r.to_rfc3339(),
                    chrono::LocalResult::None => "Invalid time".to_string(),
                };
                format!(
                    "Withdraw of {} LP -> min {} for faucet {}. From user {} (tag {}) with deadline {}. Type: {}.",
                    i.lp_amount_in,
                    i.min_asset_out.amount(),
                    i.min_asset_out.faucet_id().to_bech32(network_id.clone()),
                    i.creator.to_bech32(network_id.clone()),
                    i.p2id_tag.as_u32(),
                    deadline,
                    i.note_type
                )
            }
            NoteInstructions::Swap(i) => {
                let deadline = match Utc.timestamp_millis_opt(i.deadline as i64) {
                    chrono::LocalResult::Single(r) => r.to_rfc3339(),
                    chrono::LocalResult::Ambiguous(r, _t) => r.to_rfc3339(),
                    chrono::LocalResult::None => "Invalid time".to_string(),
                };
                format!(
                    "Swap of {} -> min {} for faucets {} -> {}. From user {} (tag {}) with deadline {}. Type: {}.",
                    i.asset_in.amount(),
                    i.min_asset_out.amount(),
                    i.asset_in.faucet_id().to_bech32(network_id.clone()),
                    i.min_asset_out.faucet_id().to_bech32(network_id.clone()),
                    i.creator.to_bech32(network_id.clone()),
                    i.p2id_tag.as_u32(),
                    deadline,
                    i.note_type,
                )
            }
            NoteInstructions::P2ID(i) => {
                format!(
                    "P2ID with amount {} for faucet {}. Target user {}. Type: {}.",
                    i.asset_in.amount(),
                    i.asset_in.faucet_id().to_bech32(network_id.clone()),
                    i.target.to_bech32(network_id),
                    i.note_type,
                )
            }
        }
    }

    pub fn involves_faucets(&self, faucets: &HashSet<AccountId>) -> bool {
        let faucets_involved = match self {
            NoteInstructions::P2ID(i) => vec![i.asset_in.faucet_id()],
            NoteInstructions::Deposit(i) => vec![i.asset_in.faucet_id()],
            NoteInstructions::Withdraw(i) => vec![i.min_asset_out.faucet_id()],
            NoteInstructions::Swap(i) => vec![i.asset_in.faucet_id(), i.min_asset_out.faucet_id()],
        };
        let faucets_involved: HashSet<AccountId> = HashSet::from_iter(faucets_involved);
        faucets_involved.is_subset(faucets)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DepositInstructions {
    pub asset_in: FungibleAsset,
    pub min_lp_amount_out: u64,
    pub creator: AccountId,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct WithdrawInstructions {
    pub min_asset_out: FungibleAsset,
    pub lp_amount_in: u64,
    pub creator: AccountId,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapInstructions {
    pub asset_in: FungibleAsset,
    pub min_asset_out: FungibleAsset,
    pub creator: AccountId,
    pub beneficiary: Option<AccountId>,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct P2IDInstructions {
    pub asset_in: FungibleAsset,
    pub target: AccountId,
    pub referential_serial_number: Option<Word>,
    pub note_type: NoteType,
}

impl TryFrom<TrustedNote> for NoteInstructions {
    type Error = anyhow::Error;
    fn try_from(note: TrustedNote) -> std::result::Result<Self, Self::Error> {
        match note.note_kind {
            NoteKind::P2ID => {
                let asset_in = note
                    .note()
                    .assets()
                    .iter()
                    .next()
                    .ok_or(anyhow!("Deposit Note has no assets!"))?;

                match asset_in {
                    Asset::Fungible(asset_in) => Ok(Self::P2ID(P2IDInstructions {
                        asset_in: *asset_in,
                        target: AccountId::from_hex("0x0")?,
                        referential_serial_number: None,
                        note_type: note.note().metadata().note_type(),
                    })),
                    _ => Err(anyhow!(
                        "P2id note contains unfungible asset, cannot be turned into p2id instruction"
                    )),
                }
            }
            NoteKind::Deposit => {
                let asset_in = note
                    .note()
                    .assets()
                    .iter()
                    .next()
                    .ok_or(anyhow!("Deposit Note has no assets!"))?;

                // have to do it like this to avoid panic
                if !asset_in.is_fungible() {
                    return Err(anyhow!("Note has no fungible assets!"));
                }
                let vals = note.note().storage().items();
                let asset_in = asset_in.unwrap_fungible();
                let min_lp_amount_out: u64 = vals[0].as_canonical_u64();
                let deadline: u64 = vals[1].as_canonical_u64();
                let p2id_tag: u64 = vals[2].as_canonical_u64();
                let creator_suffix = vals[6];
                let creator_prefix = vals[7];
                let creator = AccountId::try_from_elements(creator_suffix, creator_prefix)
                    .map_err(|_| anyhow!("Couldn't parse creator_id from order note"))?;
                Ok(Self::Deposit(DepositInstructions {
                    asset_in,
                    min_lp_amount_out,
                    creator,
                    note_type: note.note().metadata().note_type(),
                    deadline,
                    p2id_tag: NoteTag::new(p2id_tag.try_into()?),
                    pool_tag: note.note().metadata().tag(),
                }))
            }
            NoteKind::Withdraw => {
                let vals = note.note().storage().items();
                let min_asset_out = word_to_asset(Word::new(vals[..4].try_into()?))?;
                let lp_withdraw_amount: u64 = vals[5].as_canonical_u64();
                let deadline: u64 = vals[6].as_canonical_u64();
                let p2id_tag: u64 = vals[7].as_canonical_u64();
                let creator_suffix = vals[10];
                let creator_prefix = vals[11];
                let creator = AccountId::try_from_elements(creator_suffix, creator_prefix)
                    .map_err(|_| anyhow!("Couldn't parse creator_id from order note"))?;

                Ok(Self::Withdraw(WithdrawInstructions {
                    min_asset_out,
                    lp_amount_in: lp_withdraw_amount,
                    creator,
                    note_type: note.note().metadata().note_type(),
                    deadline,
                    p2id_tag: NoteTag::new(p2id_tag.try_into()?),
                    pool_tag: note.note().metadata().tag(),
                }))
            }
            NoteKind::Swap => {
                let asset_in = note
                    .note()
                    .assets()
                    .iter()
                    .next()
                    .ok_or(anyhow!("Note has no assets!"))?;
                if !asset_in.is_fungible() {
                    return Err(anyhow!("Note has no fungible assets!"));
                }
                let asset_in = asset_in.unwrap_fungible();
                let vals: &[Felt] = note.note().storage().items();
                let min_asset_out = word_to_asset(Word::new(vals[..4].try_into()?))?;
                let deadline: u64 = vals[4].as_canonical_u64();
                let p2id_tag: u64 = vals[5].as_canonical_u64();
                let beneficiary_suffix = vals[8];
                let beneficiary_prefix = vals[9];
                let beneficiary_id =
                    AccountId::try_from_elements(beneficiary_suffix, beneficiary_prefix)
                        .map_err(|_| anyhow!("Couldn't parse beneficiary_id from order note"))?;
                let creator_suffix = vals[10];
                let creator_prefix = vals[11];
                let creator_id = AccountId::try_from_elements(creator_suffix, creator_prefix)
                    .map_err(|_| anyhow!("Couldn't parse creator_id from order note"))?;
                Ok(Self::Swap(SwapInstructions {
                    asset_in,
                    min_asset_out,
                    creator: creator_id,
                    beneficiary: Some(beneficiary_id),
                    note_type: note.note().metadata().note_type(),
                    deadline,
                    p2id_tag: NoteTag::new(p2id_tag.try_into()?),
                    pool_tag: note.note().metadata().tag(),
                }))
            }
        }
    }
}

#[derive(Debug)]
pub struct TrustedNoteElements {
    pub inputs: NoteStorage,
    pub assets: NoteAssets,
    pub metadata: NoteMetadata,
    pub target: Option<AccountId>,
    pub referential_serial_number: Option<Word>, // None = will be generated
    pub note_kind: NoteKind,
}

impl TryFrom<NoteInstructions> for TrustedNoteElements {
    type Error = anyhow::Error;
    fn try_from(value: NoteInstructions) -> std::result::Result<Self, Self::Error> {
        let note_elements = match value {
            NoteInstructions::Deposit(instructions) => {
                Self::from_deposit_instructions(instructions)
            }
            NoteInstructions::Withdraw(instructions) => {
                Self::from_withdraw_instructions(instructions)
            }
            NoteInstructions::Swap(instructions) => Self::from_swap_instructions(instructions),
            NoteInstructions::P2ID(instructions) => Self::from_p2id_instructions(instructions),
        }?;
        Ok(note_elements)
    }
}

impl TrustedNoteElements {
    pub fn from_p2id_instructions(instructions: P2IDInstructions) -> Result<Self> {
        let tag = if instructions.note_type.eq(&NoteType::Public) {
            NoteTag::with_account_target(instructions.target)
        } else {
            NoteTag::new(123)
        };
        let metadata = NoteMetadata::new(instructions.target, instructions.note_type).with_tag(tag);
        let assets = NoteAssets::new(vec![instructions.asset_in.into()])?;
        Ok(Self {
            assets,
            metadata,
            inputs: NoteStorage::default(),
            target: Some(instructions.target),
            referential_serial_number: instructions.referential_serial_number,
            note_kind: NoteKind::P2ID,
        })
    }

    pub fn from_swap_instructions(instructions: SwapInstructions) -> Result<Self> {
        if instructions.asset_in.amount().eq(&0) {
            return Err(anyhow!("Amount in is zero"));
        }
        if instructions.min_asset_out.amount().eq(&0) {
            return Err(anyhow!("Min amount out is zero"));
        }
        if instructions
            .asset_in
            .faucet_id()
            .eq(&instructions.min_asset_out.faucet_id())
        {
            return Err(anyhow!("Asset in cant be the same as asset out"));
        }
        if instructions.deadline.eq(&0) {
            return Err(anyhow!("Deadline is zero"));
        }
        let min_asset_out = asset_to_word(instructions.min_asset_out);
        let beneficiary = if let Some(beneficiary) = instructions.beneficiary {
            beneficiary
        } else {
            instructions.creator
        };
        let inputs = NoteStorage::new(vec![
            min_asset_out[0],
            min_asset_out[1],
            min_asset_out[2],
            min_asset_out[3],
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            beneficiary.suffix(),
            beneficiary.prefix().into(),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::new(vec![instructions.asset_in.into()])?;
        let metadata = NoteMetadata::new(instructions.creator, instructions.note_type)
            .with_tag(instructions.pool_tag);
        Ok(Self {
            assets,
            metadata,
            inputs,
            target: None,
            referential_serial_number: None,
            note_kind: NoteKind::Swap,
        })
    }

    pub fn from_deposit_instructions(instructions: DepositInstructions) -> Result<Self> {
        // TODO: should make such DespositInstruction impossible to make rather than checking here?
        if instructions.asset_in.amount().eq(&0) {
            return Err(anyhow!("Amount in is zero"));
        }
        if instructions.min_lp_amount_out.eq(&0) {
            return Err(anyhow!("Lp amount out is zero"));
        }
        if instructions.deadline.eq(&0) {
            return Err(anyhow!("Deadline is zero"));
        }
        let inputs = NoteStorage::new(vec![
            Felt::new(instructions.min_lp_amount_out),
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            Felt::new(0),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::new(vec![instructions.asset_in.into()])?;
        let metadata = NoteMetadata::new(instructions.creator, instructions.note_type)
            .with_tag(instructions.pool_tag);
        Ok(Self {
            assets,
            metadata,
            inputs,
            target: None,
            referential_serial_number: None,
            note_kind: NoteKind::Deposit,
        })
    }

    pub fn from_withdraw_instructions(instructions: WithdrawInstructions) -> Result<Self> {
        if instructions.lp_amount_in.eq(&0) {
            return Err(anyhow!("Lp Amount in is zero"));
        }
        if instructions.min_asset_out.amount().eq(&0) {
            return Err(anyhow!("Min amount out is zero"));
        }
        if instructions.deadline.eq(&0) {
            return Err(anyhow!("Deadline is zero"));
        }
        let min_asset_out = asset_to_word(instructions.min_asset_out.into());
        let inputs = NoteStorage::new(vec![
            min_asset_out[0],
            min_asset_out[1],
            min_asset_out[2],
            min_asset_out[3],
            Felt::new(instructions.lp_amount_in),
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            Felt::new(0),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::default();
        let metadata = NoteMetadata::new(instructions.creator, instructions.note_type)
            .with_tag(instructions.pool_tag);
        Ok(Self {
            assets,
            metadata,
            inputs,
            target: None,
            referential_serial_number: None,
            note_kind: NoteKind::Withdraw,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::TestUtils;

    use super::*;

    #[tokio::test]
    async fn test_swap_instructions_to_trusted_note() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, faucet1)) = test_utils.get_two_accounts_two_faucets().await?;
        TrustedNote::new(
            NoteInstructions::Swap(SwapInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 100_000)?,
                min_asset_out: FungibleAsset::new(*faucet1.miden_account.id(), 100_000)?,
                creator: *user.id(),
                beneficiary: None,
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_swap_same_asset_in() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, _)) = test_utils.get_two_accounts_two_faucets().await?;
        let res = TrustedNote::new(
            NoteInstructions::Swap(SwapInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 100_000)?,
                min_asset_out: FungibleAsset::new(*faucet0.miden_account.id(), 100_000)?,
                creator: *user.id(),
                beneficiary: None,
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_zero_amounts() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, faucet1)) = test_utils.get_two_accounts_two_faucets().await?;
        let res = TrustedNote::new(
            NoteInstructions::Swap(SwapInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 0)?,
                min_asset_out: FungibleAsset::new(*faucet1.miden_account.id(), 100_000)?,
                creator: *user.id(),
                beneficiary: None,
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,

                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions::Swap(SwapInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 100_000)?,
                min_asset_out: FungibleAsset::new(*faucet1.miden_account.id(), 0)?,
                creator: *user.id(),
                beneficiary: None,
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_deposit_instructions_to_trusted_note() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, _)) = test_utils.get_two_accounts_two_faucets().await?;
        TrustedNote::new(
            NoteInstructions::Deposit(DepositInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 10_000)?,
                min_lp_amount_out: 10_000,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_deposit_instructions_zero_amounts() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, _)) = test_utils.get_two_accounts_two_faucets().await?;
        let res = TrustedNote::new(
            NoteInstructions::Deposit(DepositInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 0)?,
                min_lp_amount_out: 10_000,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions::Deposit(DepositInstructions {
                asset_in: FungibleAsset::new(*faucet0.miden_account.id(), 10_000)?,
                min_lp_amount_out: 0,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_withdraw_instructions_to_trusted_note() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, _)) = test_utils.get_two_accounts_two_faucets().await?;
        TrustedNote::new(
            NoteInstructions::Withdraw(WithdrawInstructions {
                min_asset_out: FungibleAsset::new(*faucet0.miden_account.id(), 10_000)?,
                lp_amount_in: 10_000,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_withdraw_instructions_zero_amounts() -> Result<()> {
        let mut test_utils = TestUtils::from_cache().await?;
        let ((user, pool), (faucet0, _)) = test_utils.get_two_accounts_two_faucets().await?;
        let res = TrustedNote::new(
            NoteInstructions::Withdraw(WithdrawInstructions {
                min_asset_out: FungibleAsset::new(*faucet0.miden_account.id(), 10_000)?,
                lp_amount_in: 0,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions::Withdraw(WithdrawInstructions {
                min_asset_out: FungibleAsset::new(*faucet0.miden_account.id(), 0)?,
                lp_amount_in: 10_000,
                creator: *user.id(),
                note_type: NoteType::Public,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: user.tag(),
                pool_tag: pool.tag(),
            }),
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }
}
