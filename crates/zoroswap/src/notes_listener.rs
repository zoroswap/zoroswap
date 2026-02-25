use crate::{
    amm_state::AmmState,
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateEvent},
};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    note::{Note, NoteId, NoteTag},
    store::NoteFilter,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tracing::{debug, error};
use zoro_miden::{client::MidenClient, note::TrustedNote};

pub struct NotesListener {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
}

impl NotesListener {
    pub fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self { state, broadcaster }
    }

    pub async fn start(&mut self) -> Result<()> {
        let pool_id = self.state.config().pool_account_id;
        let tag = NoteTag::with_account_target(pool_id);
        debug!(
            "Notes listener started. Listening for notes with pool account tag: {} (pool_id: {})",
            tag,
            pool_id.to_hex()
        );

        let config = self.state.config();
        let mut miden_client = MidenClient::new(
            config.miden_endpoint,
            config.keystore_path,
            config.store_path,
            None,
        )
        .await?;
        miden_client
            .add_note_tag(NoteTag::with_account_target(config.pool_account_id))
            .await?;

        let mut failed_notes: HashSet<NoteId> = HashSet::new();
        let mut processed_notes: HashSet<NoteId> = HashSet::new();
        let tick_interval = self.state.config().amm_tick_interval;

        loop {
            // Sync state
            if let Err(e) = miden_client.sync_state().await {
                error!(
                    error = ?e,
                    "Error on sync in notes listener"
                );
            }

            // Fetch notes and filter by tag
            match self
                .get_notes_filtered(&mut miden_client, NoteFilter::Committed, tag)
                .await
            {
                Ok(notes) => {
                    let valid_notes: Vec<&Note> = notes
                        .iter()
                        .filter(|n| {
                            !failed_notes.contains(&n.id()) && !processed_notes.contains(&n.id())
                        })
                        .collect();

                    for note in valid_notes.iter() {
                        let note_miden_id = note.id();
                        match self.state.add_order(note.clone().clone()) {
                            Ok((note_id, order_id, _)) => {
                                // Track this note as processed to avoid duplicates
                                processed_notes.insert(note_miden_id);
                                // Broadcast order received event
                                debug!(
                                    "Broadcasting order update for order_id: {}, note_id: {}",
                                    order_id, note_id
                                );
                                let event = OrderUpdateEvent {
                                    order_id,
                                    note_id,
                                    status: OrderStatus::Pending,
                                    timestamp: Utc::now().timestamp_millis() as u64,
                                };
                                if let Err(e) = self.broadcaster.broadcast_order_update(event) {
                                    error!("Failed to broadcast order update: {:?}", e);
                                }
                            }
                            Err(e) => {
                                // Check if this is a parsing error (not a swap order) vs a real error
                                let error_msg = e.to_string();
                                if error_msg.contains("note has fewer than")
                                    || error_msg.contains("Note has no assets")
                                    || error_msg.contains("Note has no fungible assets")
                                {
                                    // Not a swap order note (e.g., mint note), mark as processed to skip in future
                                    processed_notes.insert(note_miden_id);
                                } else {
                                    // Real error with a malformed swap order
                                    error!(
                                        "Error parsing order from note {}: {e:?}",
                                        note.id().to_hex()
                                    );
                                    failed_notes.insert(note_miden_id);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error in listening for zoro swap notes: {:?}", e);
                }
            };

            tokio::time::sleep(Duration::from_millis(tick_interval)).await;
        }
    }

    async fn get_notes_filtered(
        &self,
        miden_client: &mut MidenClient,
        filter: NoteFilter,
        tag: NoteTag,
    ) -> Result<Vec<Note>> {
        let all_notes = miden_client.client().get_input_notes(filter).await?;
        let notes: Vec<Note> = all_notes
            .iter()
            .filter_map(|n| {
                if let Some(metadata) = n.metadata()
                    && metadata.tag().eq(&tag)
                    && let Ok(trusted_note) = TrustedNote::from_input_note(n)
                {
                    Some(trusted_note.note().clone())
                } else {
                    None
                }
            })
            .collect();
        Ok(notes)
    }
}
