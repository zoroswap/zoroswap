use crate::{
    amm_state::AmmState,
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateEvent},
};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    note::{Note, NoteId, NoteTag, NoteType},
    store::NoteFilter,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
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
        let mut miden_client =
            MidenClient::new(config.miden_endpoint, "keystore", "stores").await?;

        miden_client
            .add_note_tag(NoteTag::with_account_target(config.pool_account_id))
            .await?;

        let mut failed_notes: HashSet<NoteId> = HashSet::new();
        let mut processed_notes: HashSet<NoteId> = HashSet::new();
        let tick_interval = self.state.config().amm_tick_interval;

        loop {
            tokio::time::sleep(Duration::from_millis(tick_interval)).await;

            // Sync state
            if let Err(e) = miden_client.sync_state().await {
                error!(
                    error = ?e,
                    "Error on sync in notes listener"
                );
            }

            // Fetch notes and filter by tag
            match self
                .get_notes_filtered(&mut miden_client, NoteFilter::Committed)
                .await
            {
                Ok(notes) => {
                    if notes.is_empty() {
                        continue;
                    }

                    let mut valid_notes: Vec<TrustedNote> = Vec::new();
                    for (n, trusted_n) in notes {
                        let has_failed = failed_notes.contains(&n.id());
                        let is_already_processed = processed_notes.contains(&n.id());
                        let is_public = n.metadata().note_type().eq(&NoteType::Public);

                        // info!(
                        //     has_failed = has_failed,
                        //     is_already_processed = is_already_processed,
                        //     is_public,
                        //     id = n.id().to_hex(),
                        //     "Validating note"
                        // );

                        if !has_failed
                            && !is_already_processed
                            && is_public
                            && let Some(trusted_n) = trusted_n
                        {
                            valid_notes.push(trusted_n)
                        }
                    }

                    if valid_notes.is_empty() {
                        continue;
                    }

                    info!("Adding {} public notes to processing.", valid_notes.len());

                    for note in valid_notes.iter() {
                        let note_miden_id = note.note().id();
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
                                        note.note().id().to_hex()
                                    );
                                    failed_notes.insert(note_miden_id);
                                }
                                let event = OrderUpdateEvent {
                                    order_id: Uuid::new_v4(),
                                    note_id: note.note().id().to_hex(),
                                    status: OrderStatus::Failed,
                                    timestamp: Utc::now().timestamp_millis() as u64,
                                };
                                if let Err(e) = self.broadcaster.broadcast_order_update(event) {
                                    error!("Failed to broadcast order update: {:?}", e);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error in listening for zoro swap notes: {:?}", e);
                }
            };
        }
    }

    async fn get_notes_filtered(
        &self,
        miden_client: &mut MidenClient,
        filter: NoteFilter,
    ) -> Result<Vec<(Note, Option<TrustedNote>)>> {
        let all_input_notes = miden_client.client().get_input_notes(filter).await?;
        let mut all_notes: Vec<(Note, Option<TrustedNote>)> = Vec::new();
        for n in &all_input_notes {
            let trusted_note = TrustedNote::from_input_note(n).inspect_err(|e|
                warn!(error=?e, note_id=n.id().to_hex(), "Error parsing note on public notes listener")).ok();
            let note: Note = n
                .try_into()
                .inspect_err(|e| warn!(error=?e, "Error parsing input note record into a note"))?;
            all_notes.push((note, trusted_note))
        }
        Ok(all_notes)
    }
}
