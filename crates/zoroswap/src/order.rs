use anyhow::Result;
use chrono::{DateTime, Utc};
use miden_client::address::NetworkId;
use tracing::info;
use uuid::Uuid;
use zoro_miden::note::{NoteInstructions, TrustedNote};

pub type OracleId = &'static str;

#[derive(Clone, Copy, Debug)]
pub struct Order {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub instructions: NoteInstructions,
}

impl Order {
    pub fn from_trusted_note(note: TrustedNote) -> Result<Self> {
        let instructions = note.try_into()?;
        let now = Utc::now();
        Ok(Self {
            id: Uuid::new_v4(),
            created_at: now,
            updated_at: now,
            instructions,
        })
    }

    pub fn print_info(&self, network_id: NetworkId) {
        info!(
            "Order {}, created at {}. {}",
            self.id,
            self.created_at.to_rfc3339(),
            self.instructions.to_pretty_info(network_id)
        );
    }
}
