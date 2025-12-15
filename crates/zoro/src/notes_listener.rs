use crate::{
    amm_state::AmmState,
    common::{ZoroStorageSettings, instantiate_client},
};
use anyhow::Result;
use miden_client::{
    note::{Note, NoteId, NoteTag},
    store::NoteFilter,
};
use std::{collections::HashSet, sync::Arc, thread, time::Duration};
use tracing::{error, warn};
use zoro_miden_client::MidenClient;

pub struct NotesListener {
    state: Arc<AmmState>,
}

impl NotesListener {
    pub fn new(state: Arc<AmmState>) -> Self {
        Self { state }
    }

    pub async fn start(&mut self) {
        let mut client = instantiate_client(
            self.state.config(),
            ZoroStorageSettings::listening_storage(self.state.config().store_path.to_string()),
        )
        .await
        .unwrap_or_else(|err| panic!("Failed to instantiate client in notes listener: {err:?}"));
        let pool_id = self.state.config().pool_account_id;
        let tag = NoteTag::from_account_id(pool_id);
        let mut failed_notes: HashSet<NoteId> = HashSet::new();
        loop {
            if let Err(e) = client.sync_state().await {
                warn!("Error on sync in notes listener: {e}");
            }
            match fetch_new_notes_by_tag(&mut client, &tag).await {
                Ok(notes) => {
                    let valid_notes: Vec<&Note> = notes
                        .iter()
                        .filter(|n| !failed_notes.contains(&n.id()))
                        .collect();
                    for note in valid_notes.iter() {
                        if let Err(e) = self.state.add_order(note.to_owned().clone()) {
                            error!("Error inserting new note: {e}");
                            failed_notes.insert(note.id());
                        };
                    }
                }
                Err(e) => {
                    error!("Error in listening for zoro swap notes: {}", e);
                }
            };
            thread::sleep(Duration::from_millis(self.state.config().amm_tick_interval));
        }
    }
}

async fn fetch_new_notes_by_tag(
    client: &mut MidenClient,
    pool_id_tag: &NoteTag,
) -> Result<Vec<Note>> {
    let all_notes = client.get_input_notes(NoteFilter::Committed).await?;
    let notes: Vec<Note> = all_notes
        .iter()
        .filter_map(|n| {
            if let Some(metadata) = n.metadata() {
                if metadata.tag().eq(pool_id_tag) {
                    let note = Note::new(
                        n.assets().clone(),
                        *metadata,
                        n.details().recipient().clone(),
                    );
                    Some(note)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect();
    Ok(notes)
}
