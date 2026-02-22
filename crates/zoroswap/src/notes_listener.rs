use crate::{
    amm_state::AmmState,
    common::get_script_root_for_order_type,
    order::OrderType,
    websocket::{EventBroadcaster, OrderStatus, OrderUpdateDetails, OrderUpdateEvent},
};
use anyhow::Result;
use chrono::Utc;
use miden_client::{
    Word,
    note::{Note, NoteId, NoteTag},
    store::NoteFilter,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tracing::{debug, error};
use zoro_miden::client::MidenClient;

pub struct NotesListener {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
    note_roots: NoteRoots,
}

impl NotesListener {
    pub fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        let note_roots = NoteRoots::generate_from_notes()
            .unwrap_or_else(|e| panic!("Error creating script roots from: {e}"));
        Self {
            state,
            broadcaster,
            note_roots,
        }
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
        miden_client.add_note_tag(NoteTag::with_account_target(config.pool_account_id));

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
                .get_notes_filtered(&mut miden_client, NoteFilter::Committed, Some(tag))
                .await
            {
                Ok(notes) => {
                    let valid_notes: Vec<&(Note, OrderType)> = notes
                        .iter()
                        .filter(|(n, _)| {
                            !failed_notes.contains(&n.id()) && !processed_notes.contains(&n.id())
                        })
                        .collect();

                    for (note, order_type) in valid_notes.iter() {
                        let note_miden_id = note.id();
                        match self.state.add_order(note.clone(), *order_type) {
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
        tag: Option<NoteTag>,
    ) -> Result<Vec<(Note, OrderType)>> {
        let all_notes = miden_client.client().get_input_notes(filter).await?;
        let notes: Vec<(Note, OrderType)> = all_notes
            .iter()
            .filter_map(|n| {
                if let Some(metadata) = n.metadata()
                    && let Some(order_type) =
                        self.note_roots.get_order_type(&n.details().script().root())
                {
                    // If tag filter provided, check it matches
                    if let Some(ref required_tag) = tag
                        && !metadata.tag().eq(required_tag)
                    {
                        return None;
                    }

                    Some((
                        Note::new(
                            n.assets().clone(),
                            metadata.clone(),
                            n.details().recipient().clone(),
                        ),
                        order_type,
                    ))
                } else {
                    None
                }
            })
            .collect();

        Ok(notes)
    }
}

struct NoteRoots {
    deposit: Word,
    withdraw: Word,
    swap: Word,
}

impl NoteRoots {
    pub fn generate_from_notes() -> Result<Self> {
        Ok(Self {
            deposit: get_script_root_for_order_type(OrderType::Deposit),
            withdraw: get_script_root_for_order_type(OrderType::Withdraw),
            swap: get_script_root_for_order_type(OrderType::Swap),
        })
    }

    pub fn get_order_type(&self, root: &Word) -> Option<OrderType> {
        if root.eq(&self.deposit) {
            Some(OrderType::Deposit)
        } else if root.eq(&self.withdraw) {
            Some(OrderType::Withdraw)
        } else if root.eq(&self.swap) {
            Some(OrderType::Swap)
        } else {
            None
        }
    }
}
