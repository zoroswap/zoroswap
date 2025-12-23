use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use miden_client::Felt;
use miden_client::account::AccountId;
use miden_client::asset::FungibleAsset;
use miden_client::note::Note;
use tracing::debug;
use uuid::Uuid;

pub type AssetSymbol = &'static str;
pub type OracleId = &'static str;

#[derive(Hash, PartialEq, Eq, Clone, Debug, Copy)]
pub struct Asset {
    pub name: &'static str,
    pub symbol: AssetSymbol,
    pub oracle_id: OracleId,
    pub account_id: AccountId,
    pub decimals: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct Order {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    pub is_limit_order: bool,
    pub asset_in: FungibleAsset,
    pub asset_out: FungibleAsset,
    pub p2id_tag: u64,
    pub creator_id: AccountId,
}

impl Order {
    pub fn from_note(note: &Note) -> Result<Self> {
        let asset_in = note
            .assets()
            .iter()
            .next()
            .ok_or(anyhow!("Note has no assets!"))?;

        // have to do it like this to avoid panic
        if !asset_in.is_fungible() {
            return Err(anyhow!("Note has no fungible assets!"));
        }
        let asset_in = asset_in.unwrap_fungible();

        let note_inputs: &[Felt] = note.inputs().values();

        debug!("Note inputs: {:?}", note_inputs);

        let requested: &[Felt] = note_inputs
            .get(..4)
            .ok_or(anyhow!("note has fewer than 4 inputs"))?;
        let requested_asset_out_id = AccountId::try_from([requested[3], requested[2]])?;
        let asset_out = FungibleAsset::new(requested_asset_out_id, requested[0].as_int())?;
        let vals = note.inputs().values();
        let deadline: u64 = vals[4].into();
        let p2id_tag: u64 = vals[5].into();
        let deadline = DateTime::from_timestamp_millis(deadline as i64).ok_or(anyhow!(
            "Error parsing deadline for order. Timestamp: {deadline}"
        ))?;
        let creator_prefix = vals[11];
        let creator_suffix = vals[10];
        let creator_id = AccountId::try_from([creator_prefix, creator_suffix])
            .or(Err(anyhow!("Couldn't parse creator_id from order note")))?;

        let order = Order {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline,
            is_limit_order: false,
            asset_in,
            asset_out,
            creator_id,
            id: Uuid::new_v4(),
            p2id_tag,
        };

        debug!("New order from {}: {order:?}", creator_id.to_hex());

        Ok(order)
    }
}
