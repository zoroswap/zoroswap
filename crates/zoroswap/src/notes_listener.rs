use crate::{
    amm_state::AmmState,
    common::instantiate_client,
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateDetails, OrderUpdateEvent},
};
use chrono::Utc;
use miden_client::{
    note::{Note, NoteId, NoteTag},
    store::NoteFilter,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tracing::{debug, error, warn};

pub struct NotesListener {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
}

impl NotesListener {
    pub fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self { state, broadcaster }
    }

    pub async fn start(&mut self) {
        let pool_id = self.state.config().pool_account_id;
        let tag = NoteTag::from_account_id(pool_id);
        debug!(
            "Notes listener started. Listening for notes with pool account tag: {} (pool_id: {})",
            tag,
            pool_id.to_hex()
        );

        // Create our own client for notes listening (with retry for DB contention)
        let mut client = None;
        for attempt in 1..=5 {
            match instantiate_client(self.state.config(), self.state.config().store_path).await {
                Ok(c) => {
                    client = Some(c);
                    break;
                }
                Err(e) => {
                    if attempt < 5 {
                        warn!(
                            "Notes listener client creation attempt {}/5 failed: {e}, retrying...",
                            attempt
                        );
                        tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                    } else {
                        error!("Failed to create notes listener client after 5 attempts: {e}");
                        return;
                    }
                }
            }
        }
        let mut client = client.unwrap();

        let mut failed_notes: HashSet<NoteId> = HashSet::new();
        let mut processed_notes: HashSet<NoteId> = HashSet::new();
        let tick_interval = self.state.config().amm_tick_interval;

        loop {
            // Sync state
            if let Err(e) = client.sync_state().await {
                warn!("Error on sync in notes listener: {e}");
            }

            // Fetch notes and filter by tag
            match Self::get_notes_filtered(&mut client, NoteFilter::Committed, Some(tag)).await {
                Ok(notes) => {
                    let valid_notes: Vec<&Note> = notes
                        .iter()
                        .filter(|n| {
                            !failed_notes.contains(&n.id()) && !processed_notes.contains(&n.id())
                        })
                        .collect();

                    for note in valid_notes.iter() {
                        let note_miden_id = note.id();
                        match self.state.add_order(note.clone()) {
                            Ok((note_id, order_id, order)) => {
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
                                    details: OrderUpdateDetails {
                                        amount_in: order.asset_in.amount(),
                                        amount_out: None,
                                        asset_in_faucet: order.asset_in.faucet_id().to_hex(),
                                        asset_out_faucet: order.asset_out.faucet_id().to_hex(),
                                    },
                                    timestamp: Utc::now().timestamp_millis() as u64,
                                };
                                if let Err(e) = self.broadcaster.broadcast_order_update(event) {
                                    error!("Failed to broadcast order update: {}", e);
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
                                        "Error parsing swap order from note {}: {e}",
                                        note.id().to_hex()
                                    );
                                    failed_notes.insert(note_miden_id);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error in listening for zoro swap notes: {}", e);
                }
            };

            tokio::time::sleep(Duration::from_millis(tick_interval)).await;
        }
    }

    async fn get_notes_filtered(
        client: &mut zoro_miden_client::MidenClient,
        filter: NoteFilter,
        tag: Option<NoteTag>,
    ) -> Result<Vec<Note>, anyhow::Error> {
        let all_notes = client.get_input_notes(filter).await?;
        let notes: Vec<Note> = all_notes
            .iter()
            .filter_map(|n| {
                if let Some(metadata) = n.metadata() {
                    // If tag filter provided, check it matches
                    if let Some(ref required_tag) = tag {
                        if !metadata.tag().eq(required_tag) {
                            return None;
                        }
                    }
                    Some(Note::new(
                        n.assets().clone(),
                        *metadata,
                        n.details().recipient().clone(),
                    ))
                } else {
                    None
                }
            })
            .collect();

        Ok(notes)
    }
}
