use std::{collections::HashSet, fs::read_to_string, path::PathBuf};

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
    store::{InputNoteRecord, OutputNoteRecord},
};
use miden_protocol::note::NoteScript;
use miden_standards::note::P2idNoteStorage;
use rand::{Rng, SeedableRng, rngs::StdRng};
use tracing::info;

use crate::asset_utils::{asset_to_word, word_to_asset};
use crate::{assembly_utils::link_all_libraries, note_roots::get_note_roots};

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NoteKind {
    P2ID,
    Deposit,
    Withdraw,
    Swap,
    Position,
}

impl NoteKind {
    pub fn masm_name(&self) -> &str {
        match self {
            NoteKind::P2ID => "P2ID.masm",
            NoteKind::Deposit => "DEPOSIT.masm",
            NoteKind::Withdraw => "WITHDRAW.masm",
            NoteKind::Swap => "ZOROSWAP.masm",
            NoteKind::Position => "POSITION.masm",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PositionOptions {
    asset_in: FungibleAsset,
    asset_out: FungibleAsset,
}

#[derive(Clone, Debug)]
pub struct TrustedNote {
    note: Note,
    note_kind: NoteKind,
    serial_number: Word,
    created_at: i64,
    position_options: Option<PositionOptions>,
}

impl TrustedNote {
    pub fn new_p2id_from_instructions(p2id_instructions: P2IDInstructions) -> Result<Self> {
        let note_elements: TrustedNoteElements = TrustedNoteElements::try_from(p2id_instructions)?;
        Self::new_p2id(note_elements)
    }

    pub fn new(note_instructions: NoteInstructions, code_builder: CodeBuilder) -> Result<Self> {
        let note_elements: TrustedNoteElements = TrustedNoteElements::try_from(note_instructions)?;
        Self::new_zoro_note(note_elements, code_builder)
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
        let recipient = P2idNoteStorage::new(target).into_recipient(serial_number);
        let note = Note::new(
            note_elements.note_assets,
            note_elements.note_metadata,
            recipient,
        );
        Ok(TrustedNote {
            note,
            note_kind: NoteKind::P2ID,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
            position_options: None,
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
        let recipient = NoteRecipient::new(serial_number, note_script, note_elements.note_storage);
        let note = Note::new(
            note_elements.note_assets,
            note_elements.note_metadata,
            recipient,
        );
        Ok(Self {
            note,
            note_kind: note_elements.note_kind,
            serial_number,
            created_at: Utc::now().timestamp_millis(),
            position_options: None,
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
            position_options: None,
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
            position_options: None,
        })
    }
    pub fn from_output_note(output_note_record: &OutputNoteRecord) -> Result<Self> {
        let note = Note::new(
            output_note_record.assets().clone(),
            output_note_record.metadata().clone(),
            output_note_record
                .recipient()
                .ok_or(anyhow!("Note missing recipient"))?
                .clone(),
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
            position_options: None,
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
        let p2id_note = TrustedNote::new_p2id_from_instructions(P2IDInstructions {
            asset_in,
            target,
            referential_serial_number,
            note_type: NoteType::Public,
        })?;
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

impl NoteInstructions {
    pub fn note_kind(&self) -> NoteKind {
        self.note_kind
    }

    pub fn to_pretty_info(&self, network_id: NetworkId) -> String {
        let deadline = match Utc.timestamp_millis_opt(self.deadline as i64) {
            chrono::LocalResult::Single(r) => r.to_rfc3339(),
            chrono::LocalResult::Ambiguous(r, _t) => r.to_rfc3339(),
            chrono::LocalResult::None => "Invalid time".to_string(),
        };
        let assets = self
            .attached_assets
            .iter()
            .map(|a| a.faucet_id().to_bech32(network_id.clone()))
            .collect::<Vec<String>>()
            .join(", ");
        format!(
            "{:?} of {:?} -> min LP: {} for faucet {}. From user {} (tag {}) with deadline {}. Type: {}.",
            self.note_kind,
            self.asset,
            self.min_amount,
            assets,
            self.beneficiary.to_bech32(network_id.clone()),
            self.p2id_tag.as_u32(),
            deadline,
            self.note_type,
        )
    }

    pub fn involves_faucets(&self, faucets: &HashSet<AccountId>) -> bool {
        let mut faucets_involved: Vec<AccountId> =
            self.attached_assets.iter().map(|a| a.faucet_id()).collect();
        if let Some(asset) = self.asset {
            faucets_involved.push(asset.faucet_id());
        }
        let faucets_involved: HashSet<AccountId> = HashSet::from_iter(faucets_involved);
        faucets_involved.is_subset(faucets)
    }
}

#[derive(Clone, Debug)]
pub struct NoteInstructions {
    pub attached_assets: Vec<FungibleAsset>,
    pub asset: Option<FungibleAsset>,
    pub min_amount: u64,
    pub beneficiary: AccountId,
    pub deadline: u64,
    pub p2id_tag: NoteTag,
    pub note_type: NoteType,
    pub pool_tag: NoteTag,
    pub note_kind: NoteKind,
}

#[derive(Clone, Copy, Debug)]
pub struct P2IDInstructions {
    pub asset_in: FungibleAsset,
    pub target: AccountId,
    pub referential_serial_number: Option<Word>,
    pub note_type: NoteType,
}

impl TryFrom<TrustedNote> for P2IDInstructions {
    type Error = anyhow::Error;
    fn try_from(note: TrustedNote) -> std::result::Result<Self, Self::Error> {
        let asset_in = note
            .note()
            .assets()
            .iter()
            .next()
            .ok_or(anyhow!("Deposit Note has no assets!"))?;

        match asset_in {
            Asset::Fungible(asset_in) => Ok(Self {
                asset_in: *asset_in,
                target: note.note().metadata().sender(),
                referential_serial_number: None,
                note_type: note.note().metadata().note_type(),
            }),
            _ => Err(anyhow!(
                "P2id note contains unfungible asset, cannot be turned into p2id instruction"
            )),
        }
    }
}

impl TryFrom<TrustedNote> for NoteInstructions {
    type Error = anyhow::Error;
    fn try_from(note: TrustedNote) -> std::result::Result<Self, Self::Error> {
        let assets: Vec<FungibleAsset> = note.note().assets().iter_fungible().collect();
        let vals: &[Felt] = note.note().storage().items();
        let asset = word_to_asset(Word::new(vals[..4].try_into()?)).ok();
        let deadline: u64 = vals[4].as_canonical_u64();
        let p2id_tag: u64 = vals[5].as_canonical_u64();
        let min_amount: u64 = vals[6].as_canonical_u64();
        let beneficiary_suffix = vals[8];
        let beneficiary_prefix = vals[9];
        let beneficiary_id =
            AccountId::try_from_elements(beneficiary_suffix, beneficiary_prefix)
                .map_err(|_| anyhow!("Couldn't parse beneficiary_id from order note"))?;
        Ok(Self {
            attached_assets: assets,
            min_amount,
            asset,
            beneficiary: beneficiary_id,
            note_type: note.note().metadata().note_type(),
            deadline,
            p2id_tag: NoteTag::new(p2id_tag.try_into()?),
            pool_tag: note.note().metadata().tag(),
            note_kind: *note.note_kind(),
        })
    }
}

#[derive(Debug)]
pub struct TrustedNoteElements {
    pub note_storage: NoteStorage,
    pub note_assets: NoteAssets,
    pub note_metadata: NoteMetadata,
    pub target: Option<AccountId>,
    pub referential_serial_number: Option<Word>, // None = will be generated
    pub note_kind: NoteKind,
}

impl TryFrom<NoteInstructions> for TrustedNoteElements {
    type Error = anyhow::Error;
    fn try_from(value: NoteInstructions) -> std::result::Result<Self, Self::Error> {
        let note_elements = Self::from_note_instructions(value)?;
        Ok(note_elements)
    }
}

impl TryFrom<P2IDInstructions> for TrustedNoteElements {
    type Error = anyhow::Error;
    fn try_from(value: P2IDInstructions) -> std::result::Result<Self, Self::Error> {
        let note_elements = Self::from_p2id_instructions(value)?;
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
        let note_metadata =
            NoteMetadata::new(instructions.target, instructions.note_type).with_tag(tag);
        let note_assets = NoteAssets::new(vec![instructions.asset_in.into()])?;
        Ok(Self {
            note_assets,
            note_metadata,
            note_storage: NoteStorage::default(),
            target: Some(instructions.target),
            referential_serial_number: instructions.referential_serial_number,
            note_kind: NoteKind::P2ID,
        })
    }
    pub fn from_note_instructions(instructions: NoteInstructions) -> Result<Self> {
        let mut note_storage = NoteStorageBuilder::new(instructions.beneficiary)
            .with_min_amount(instructions.min_amount)?
            .with_deadline(instructions.deadline)?
            .with_p2id_tag(instructions.p2id_tag);
        if let Some(asset) = instructions.asset {
            note_storage = note_storage.with_asset(asset)
        }
        let note_storage = note_storage.build()?;
        let note_metadata = NoteMetadata::new(instructions.beneficiary, instructions.note_type)
            .with_tag(instructions.pool_tag);
        let note_kind = instructions.note_kind();
        let note_assets = NoteAssets::new(
            instructions
                .attached_assets
                .into_iter()
                .map(Asset::from)
                .collect(),
        )?;
        Ok(Self {
            note_assets,
            note_metadata,
            note_storage,
            target: None,
            referential_serial_number: None,
            note_kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::TestUtils;

    use super::*;

    #[tokio::test]
    async fn test_swap_instructions_to_trusted_note() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 100_000)?],
                asset: Some(FungibleAsset::new(test_utils.faucet_2, 100_000)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0, // TODO: ZOROSWAP should use this instead of asset in?
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Swap,
            },
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_swap_same_asset_in() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 100_000)?],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 100_000)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0, // TODO: ZOROSWAP should use this instead of asset in?
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Swap,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_zero_amounts() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 0)?],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 100_000)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0, // TODO: ZOROSWAP should use this instead of asset in?
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Swap,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 100_000)?],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 0)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0, // TODO: ZOROSWAP should use this instead of asset in?
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Swap,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_deposit_instructions_to_trusted_note() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 100_000)?],
                asset: None,
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 100_000,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Deposit,
            },
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_deposit_instructions_zero_amounts() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 0)?],
                asset: None,
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 100_000,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Deposit,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![FungibleAsset::new(test_utils.faucet_1, 100_000)?],
                asset: None,
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Deposit,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }

    #[tokio::test]
    async fn test_withdraw_instructions_to_trusted_note() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 10_000)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 10_000,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Withdraw,
            },
            test_utils.miden_client().client().code_builder(),
        )?;
        Ok(())
    }

    #[tokio::test]
    async fn test_withdraw_instructions_zero_amounts() -> Result<()> {
        let test_utils = TestUtils::from_cache().await?;
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 0)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 10_000,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Withdraw,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        let res = TrustedNote::new(
            NoteInstructions {
                attached_assets: vec![],
                asset: Some(FungibleAsset::new(test_utils.faucet_1, 10_000)?),
                beneficiary: test_utils.user_1,
                note_type: NoteType::Public,
                min_amount: 0,
                deadline: Utc::now().timestamp_millis() as u64,
                p2id_tag: NoteTag::with_account_target(test_utils.user_2),
                pool_tag: NoteTag::with_account_target(test_utils.pool_1),
                note_kind: NoteKind::Withdraw,
            },
            test_utils.miden_client().client().code_builder(),
        );
        assert!(res.is_err(), "Should have rejected constructing the note.");
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct NoteStorageBuilder {
    beneficiary: AccountId,
    asset: Option<Word>,
    metadata: Option<Word>,
}

impl NoteStorageBuilder {
    pub fn new(beneficiary: AccountId) -> Self {
        Self {
            beneficiary,
            asset: None,
            metadata: None,
        }
    }

    pub fn with_asset(mut self, asset: FungibleAsset) -> Self {
        self.asset = Some(asset_to_word(asset));
        self
    }
    pub fn with_asset_compact(mut self, asset: Word) -> Self {
        self.asset = Some(asset);
        self
    }

    pub fn with_metadata(mut self, metadata: Word) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_deadline(mut self, deadline: u64) -> Result<Self> {
        if let Some(metadata) = self.metadata {
            self.metadata =
                Some([deadline.try_into()?, metadata[1], metadata[2], metadata[3]].into())
        } else {
            self.metadata = Some([deadline.try_into()?, Felt::ZERO, Felt::ZERO, Felt::ZERO].into())
        }
        Ok(self)
    }

    pub fn with_p2id_tag(mut self, tag: NoteTag) -> Self {
        if let Some(metadata) = self.metadata {
            self.metadata = Some([metadata[0], tag.into(), metadata[2], metadata[3]].into())
        } else {
            self.metadata = Some([Felt::ZERO, tag.into(), Felt::ZERO, Felt::ZERO].into())
        }
        self
    }

    pub fn with_min_amount(mut self, min_amount: u64) -> Result<Self> {
        if let Some(metadata) = self.metadata {
            self.metadata = Some(
                [
                    metadata[0],
                    metadata[1],
                    min_amount.try_into()?,
                    metadata[3],
                ]
                .into(),
            )
        } else {
            self.metadata =
                Some([Felt::ZERO, Felt::ZERO, min_amount.try_into()?, Felt::ZERO].into())
        }
        Ok(self)
    }

    pub fn build(self) -> Result<NoteStorage> {
        let asset = self.asset.unwrap_or(Word::new([Felt::ZERO; 4]));
        let metadata = self.metadata.unwrap_or(Word::new([Felt::ZERO; 4]));
        let beneficiary = self.beneficiary;

        Ok(NoteStorage::new(vec![
            asset[0],
            asset[1],
            asset[2],
            asset[3],
            metadata[0],
            metadata[1],
            metadata[2],
            metadata[3],
            beneficiary.suffix(),
            beneficiary.prefix().into(),
            Felt::ZERO,
            Felt::ZERO,
        ])?)
    }
}
