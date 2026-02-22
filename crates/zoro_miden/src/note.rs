use anyhow::Result;
use miden_client::{
    Word,
    account::AccountId,
    address::NetworkId,
    asset::Asset,
    note::{Note, NoteAssets, NoteError, NoteMetadata, NoteTag, NoteType, build_p2id_recipient},
};
use tracing::info;

/// Creates a P2ID (Pay-to-ID) note.
///
/// # Arguments
/// * `sender`: The account ID sending the note
/// * `target`: The account ID receiving the note
/// * `assets`: Assets to include in the note
/// * `note_type`: Type of the note (Public/Private)
/// * `serial_num`: Serial number for the note
pub fn create_p2id_note(
    sender: AccountId,
    target: AccountId,
    assets: Vec<Asset>,
    note_type: NoteType,
    serial_num: Word,
) -> Result<Note, NoteError> {
    info!(
        "Creating P2ID sender: {}, target: {}, assets: {assets:?}, note_type: {note_type:?}, serial_num: {serial_num:?}",
        sender.to_bech32(NetworkId::Testnet),
        target.to_bech32(NetworkId::Testnet)
    );
    let recipient = build_p2id_recipient(target, serial_num)?;
    let tag = NoteTag::with_account_target(target);
    let metadata = NoteMetadata::new(sender, note_type, tag);
    let vault = NoteAssets::new(assets)?;
    Ok(Note::new(vault, metadata, recipient))
}
