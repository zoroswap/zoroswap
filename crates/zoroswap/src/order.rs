use anyhow::Result;
use chrono::{DateTime, Utc};
use miden_client::{Felt, note::NoteId};
use tracing::info;
use uuid::Uuid;
use zoro_miden::note::TrustedNote;

#[derive(Clone, Debug)]
pub struct Order {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub note_id: NoteId,
    pub additional_details: Option<Vec<Felt>>,
}

impl Order {
    pub fn from_trusted_note(
        note: TrustedNote,
        additional_details: Option<Vec<Felt>>,
    ) -> Result<Self> {
        let note_id = note.note().id();
        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            note_id,
            additional_details,
        })
    }

    pub fn print_info(&self) {
        info!(
            id =? self.id,
            created = self.created_at.to_rfc3339(),
            note_id = self.note_id.to_hex(),
            "Order summary"
        );
    }
}
