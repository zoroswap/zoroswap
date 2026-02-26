use std::{fs::read_to_string, path::PathBuf, sync::OnceLock};

use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use miden_client::{
    Felt, Word,
    account::AccountId,
    assembly::CodeBuilder,
    asset::{Asset, FungibleAsset},
    note::{
        Note, NoteAssets, NoteInputs, NoteMetadata, NoteRecipient, NoteTag, NoteType,
        WellKnownNote, build_p2id_recipient,
    },
    store::InputNoteRecord,
    transaction::TransactionKernel,
};
use miden_protocol::{
    crypto::rand::Randomizable,
    utils::{Deserializable, Serializable},
};
use rand::{Rng, SeedableRng, rngs::StdRng};
use tracing::info;

use crate::client::create_library;

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
            NoteKind::Swap => "SWAP.masm",
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
    pub fn new(note_instructions: NoteInstructions) -> Result<Self> {
        let note_elements: TrustedNoteElements = note_instructions.try_into()?;
        match note_elements.note_kind {
            NoteKind::P2ID => Self::new_p2id(note_elements),
            NoteKind::Deposit | NoteKind::Withdraw | NoteKind::Swap => {
                Self::new_zoro_note(note_elements)
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
            .into();
            Ok(p2id_serial_num)
        } else {
            Self::random_word()
        }?;
        let recipient = build_p2id_recipient(target, serial_number)?;
        let note = Note::new(note_elements.assets, note_elements.metadata, recipient);
        Ok(TrustedNote {
            note,
            note_kind: NoteKind::P2ID,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
        })
    }

    fn new_zoro_note(note_elements: TrustedNoteElements) -> Result<Self> {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let assembler = TransactionKernel::assembler().with_warnings_as_errors(true);
        let note_path = PathBuf::from_iter(&[
            manifest_dir,
            "masm",
            "notes",
            note_elements.note_kind.masm_name(),
        ]);
        let pool_path = PathBuf::from_iter(&[manifest_dir, "masm", "accounts", "zoropool.masm"]);
        let note_code = read_to_string(&note_path).map_err(|e| anyhow!("{e:?}"))?;
        let pool_code = read_to_string(&pool_path).map_err(|e| anyhow!("{e:?}"))?;
        let pool_component_lib =
            create_library(assembler.clone(), "zoroswap::zoropool", &pool_code)
                .map_err(|e| anyhow!("{e:?}"))?;
        let note_script = CodeBuilder::new()
            .with_dynamically_linked_library(&pool_component_lib)?
            .compile_note_script(note_code)?;
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
        asset_in: AccountId,
        amount_in: u64,
        referential_serial_number: Option<Word>,
    ) -> Result<Self> {
        let p2id_note = TrustedNote::new(NoteInstructions::P2ID(P2IDInstructions {
            asset_in,
            amount_in,
            target,
            referential_serial_number,
            note_type: NoteType::Public,
        }))?;
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
        let mut seed = [0; 32];
        // default from os to get initial seed
        let mut std_rng = StdRng::from_os_rng();
        std_rng.fill(&mut seed);
        // regenerate with a seed
        // TODO: maybe not needed?
        let mut rng = StdRng::from_seed(seed);
        let mut seed = [0u8; 32];
        rng.fill(&mut seed);
        let random_word = Word::from_random_bytes(&seed)
            .ok_or(anyhow!("Error generating new word, no word was produced"))?;
        Ok(random_word)
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
}

#[derive(Clone, Copy, Debug)]
pub struct DepositInstructions {
    pub asset_in: AccountId,
    pub amount_in: u64,
    pub min_lp_amount_out: u64,
    pub creator: AccountId,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct WithdrawInstructions {
    pub asset_out: AccountId,
    pub lp_amount_in: u64,
    pub min_amount_out: u64,
    pub creator: AccountId,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct SwapInstructions {
    pub asset_in: AccountId,
    pub amount_in: u64,
    pub asset_out: AccountId,
    pub min_amount_out: u64,
    pub creator: AccountId,
    pub beneficiary: Option<AccountId>,
    pub note_type: NoteType,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub pool_tag: NoteTag,
}

#[derive(Clone, Copy, Debug)]
pub struct P2IDInstructions {
    pub asset_in: AccountId,
    pub amount_in: u64,
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
                        asset_in: asset_in.faucet_id(),
                        amount_in: asset_in.amount(),
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
                let vals = note.note().inputs().values();
                let asset_in = asset_in.unwrap_fungible();
                let min_lp_amount_out: u64 = vals[0].into();
                let deadline: u64 = vals[1].into();
                let p2id_tag: u64 = vals[2].into();
                let creator_prefix = vals[7];
                let creator_suffix = vals[6];
                let creator = AccountId::try_from([creator_prefix, creator_suffix])
                    .or(Err(anyhow!("Couldn't parse creator_id from order note")))?;
                Ok(Self::Deposit(DepositInstructions {
                    asset_in: asset_in.faucet_id(),
                    amount_in: asset_in.amount(),
                    min_lp_amount_out,
                    creator,
                    note_type: note.note().metadata().note_type(),
                    deadline,
                    p2id_tag: NoteTag::new(p2id_tag.try_into()?),
                    pool_tag: note.note().metadata().tag(),
                }))
            }
            NoteKind::Withdraw => {
                let vals = note.note().inputs().values();
                let requested_asset_out_id = AccountId::try_from([vals[3], vals[2]])?;
                let asset_out = FungibleAsset::new(requested_asset_out_id, vals[0].as_int())?;
                let lp_withdraw_amount: u64 = vals[5].into();
                let deadline: u64 = vals[6].into();
                let p2id_tag: u64 = vals[7].into();
                let creator_prefix = vals[11];
                let creator_suffix = vals[10];
                let creator = AccountId::try_from([creator_prefix, creator_suffix])
                    .or(Err(anyhow!("Couldn't parse creator_id from order note")))?;

                Ok(Self::Withdraw(WithdrawInstructions {
                    asset_out: asset_out.faucet_id(),
                    lp_amount_in: lp_withdraw_amount,
                    min_amount_out: asset_out.amount(),
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
                let vals: &[Felt] = note.note().inputs().values();
                let requested: &[Felt] = vals
                    .get(..4)
                    .ok_or(anyhow!("note has fewer than 4 inputs"))?;
                let requested_asset_out_id = AccountId::try_from([requested[3], requested[2]])?;
                let asset_out = FungibleAsset::new(requested_asset_out_id, requested[0].as_int())?;
                let deadline: u64 = vals[4].into();
                let p2id_tag: u64 = vals[5].into();
                let beneficiary_prefix = vals[9];
                let beneficiary_suffix = vals[8];
                let beneficiary_id = AccountId::try_from([beneficiary_prefix, beneficiary_suffix])
                    .or(Err(anyhow!(
                        "Couldn't parse beneficiary_id from order note"
                    )))?;
                let creator_prefix = vals[11];
                let creator_suffix = vals[10];
                let creator_id = AccountId::try_from([creator_prefix, creator_suffix])
                    .or(Err(anyhow!("Couldn't parse creator_id from order note")))?;
                Ok(Self::Swap(SwapInstructions {
                    asset_in: asset_in.faucet_id(),
                    amount_in: asset_in.amount(),
                    asset_out: asset_out.faucet_id(),
                    min_amount_out: asset_out.amount(),
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
    pub inputs: NoteInputs,
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
        let metadata = NoteMetadata::new(instructions.target, instructions.note_type, tag);
        let assets = NoteAssets::new(vec![
            FungibleAsset::new(instructions.asset_in, instructions.amount_in)?.into(),
        ])?;
        Ok(Self {
            assets,
            metadata,
            inputs: NoteInputs::default(),
            target: Some(instructions.target),
            referential_serial_number: instructions.referential_serial_number,
            note_kind: NoteKind::P2ID,
        })
    }

    pub fn from_swap_instructions(instructions: SwapInstructions) -> Result<Self> {
        let requested_asset: Word =
            FungibleAsset::new(instructions.asset_out, instructions.min_amount_out)?.into();
        let beneficiary = if let Some(beneficiary) = instructions.beneficiary {
            beneficiary
        } else {
            instructions.creator
        };
        let inputs = NoteInputs::new(vec![
            requested_asset[0],
            requested_asset[1],
            requested_asset[2],
            requested_asset[3],
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            beneficiary.suffix(),
            beneficiary.prefix().into(),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::new(vec![
            FungibleAsset::new(instructions.asset_in, instructions.amount_in)?.into(),
        ])?;
        let metadata = NoteMetadata::new(
            instructions.creator,
            instructions.note_type,
            instructions.pool_tag,
        );
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
        let inputs = NoteInputs::new(vec![
            Felt::new(instructions.min_lp_amount_out),
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            Felt::new(0),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::new(vec![
            FungibleAsset::new(instructions.asset_in, instructions.amount_in)?.into(),
        ])?;
        let metadata = NoteMetadata::new(
            instructions.creator,
            instructions.note_type,
            instructions.pool_tag,
        );
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
        let asset_out: Word =
            FungibleAsset::new(instructions.asset_out, instructions.min_amount_out)?.into();
        let inputs = NoteInputs::new(vec![
            asset_out[0],
            asset_out[1],
            asset_out[2],
            asset_out[3],
            Felt::new(0),
            Felt::new(instructions.lp_amount_in),
            Felt::new(instructions.deadline),
            instructions.p2id_tag.into(),
            Felt::new(0),
            Felt::new(0),
            instructions.creator.suffix(),
            instructions.creator.prefix().into(),
        ])?;
        let assets = NoteAssets::default();
        let metadata = NoteMetadata::new(
            instructions.creator,
            instructions.note_type,
            instructions.pool_tag,
        );
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
