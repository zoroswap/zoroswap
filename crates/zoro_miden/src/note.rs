use std::time::Duration;

use anyhow::Result;
use miden_client::{
    ClientError, Word,
    account::{Account, AccountId},
    address::NetworkId,
    asset::Asset,
    note::{
        Note, NoteAssets, NoteError, NoteId, NoteMetadata, NoteTag, NoteType, build_p2id_recipient,
    },
    store::NoteFilter,
};
use tokio::time::sleep;
use tracing::{debug, info};

use crate::client::MidenClient;

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
    client.add_note_tag(tag).await?;
    loop {
        client.sync_state().await?;
        debug!(
            "All input notes: {:?}",
            client.get_input_notes(NoteFilter::All).await?
        );
        debug!(
            "All output notes: {:?}",
            client.get_output_notes(NoteFilter::All).await?
        );
        let notes = client.get_consumable_notes(None).await?;
        debug!("Notes len: {:?}", notes.len());
        let found = notes.iter().any(|(note, _)| note.id() == target_note_id);
        if found {
            debug!("Found the note with ID: {:?}", target_note_id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
    Ok(())
}

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
        let notes = client.get_consumable_notes(None).await?;
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
) -> Result<Vec<Note>> {
    loop {
        client.sync_state().await?;
        let notes = client.get_consumable_notes(Some(account_id)).await?;
        if !notes.is_empty() {
            let notes = notes
                .iter()
                .map(|(note, _)| note.clone().try_into())
                .collect::<Result<Vec<_>, _>>()?;
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

pub async fn fetch_new_notes_by_tag(
    client: &mut MidenClient,
    pool_id_tag: &NoteTag,
) -> Result<Vec<Note>> {
    client.sync_state().await?;
    let all_notes = client.get_output_notes(NoteFilter::Committed).await?;
    let notes: Vec<Note> = all_notes
        .iter()
        .filter_map(|n| {
            if n.metadata().tag().eq(pool_id_tag)
                && let Some(recipient) = n.recipient()
            {
                let note = Note::new(n.assets().clone(), n.metadata().clone(), recipient.clone());
                Some(note)
            } else {
                None
            }
        })
        .collect();
    Ok(notes)
}
