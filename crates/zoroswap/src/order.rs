use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use miden_client::Felt;
use miden_client::account::AccountId;
use miden_client::asset::FungibleAsset;
use miden_client::note::Note;
use tracing::{debug, info};
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
pub enum OrderType {
    Deposit,
    Withdraw,
    Swap,
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
    pub beneficiary_id: AccountId,
    pub order_type: OrderType,
}

impl Order {
    pub fn from_swap_note(note: &Note) -> Result<Self> {
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
        let beneficiary_prefix = vals[11];
        let beneficiary_suffix = vals[10];
        let beneficiary_id = AccountId::try_from([beneficiary_prefix, beneficiary_suffix])
            .or(Err(anyhow!("Couldn't parse beneficiary_id from order note")))?;

        info!(
            "Asset in: {}, asset out: {}",
            asset_in
                .faucet_id()
                .to_bech32(miden_client::address::NetworkId::Testnet),
            asset_out
                .faucet_id()
                .to_bech32(miden_client::address::NetworkId::Testnet)
        );

        let order = Order {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline,
            is_limit_order: false,
            asset_in,
            asset_out,
            beneficiary_id,
            id: Uuid::new_v4(),
            p2id_tag,
            order_type: OrderType::Swap,
        };

        info!("New swap order from {}: {order:?}", beneficiary_id.to_hex());

        Ok(order)
    }

    pub fn from_deposit_note(note: &Note) -> Result<Order> {
        let asset_in = note
            .assets()
            .iter()
            .next()
            .ok_or(anyhow!("Deposit Note has no assets!"))?;

        // have to do it like this to avoid panic
        if !asset_in.is_fungible() {
            return Err(anyhow!("Note has no fungible assets!"));
        }
        let note_inputs: &[Felt] = note.inputs().values();
        debug!("Note inputs: {:?}", note_inputs);
        debug!("confirmation");
        let vals = note.inputs().values();
        debug!("vals: {vals:?}");
        let asset_in = asset_in.unwrap_fungible();
        debug!("asset_in: {asset_in:?}");
        let min_lp_out: u64 = vals[1].into();
        let asset_out = FungibleAsset::new(asset_in.faucet_id(), min_lp_out)?;
        debug!("Asset out: {:?}", asset_in);

        let deadline: u64 = vals[2].into();
        let p2id_tag: u64 = vals[3].into();
        let deadline = DateTime::from_timestamp_millis(deadline as i64).ok_or(anyhow!(
            "Error parsing deadline for order. Timestamp: {deadline}"
        ))?;
        let beneficiary_prefix = vals[7];
        let beneficiary_suffix = vals[6];
        let beneficiary_id = AccountId::try_from([beneficiary_prefix, beneficiary_suffix])
            .or(Err(anyhow!("Couldn't parse beneficiary_id from order note")))?;

        let order = Order {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline,
            is_limit_order: false,
            asset_in,
            asset_out,
            beneficiary_id,
            id: Uuid::new_v4(),
            p2id_tag,
            order_type: OrderType::Deposit,
        };

        info!("New deposit order from {}: {order:?}", beneficiary_id.to_hex());

        Ok(order)
    }

    pub fn from_withdraw_note(note: &Note) -> Result<Order> {
        let note_inputs: &[Felt] = note.inputs().values();
        debug!("Note inputs: {:?}", note_inputs);

        let vals = note.inputs().values();
        let lp_withdraw_amount: u64 = vals[5].into();

        let requested_asset_out_id = AccountId::try_from([vals[3], vals[2]])?;
        let asset_out = FungibleAsset::new(requested_asset_out_id, vals[0].as_int())?;
        let asset_in = FungibleAsset::new(asset_out.faucet_id(), lp_withdraw_amount)?;

        let deadline: u64 = vals[6].into();
        let p2id_tag: u64 = vals[7].into();
        let deadline = DateTime::from_timestamp_millis(deadline as i64).ok_or(anyhow!(
            "Error parsing deadline for order. Timestamp: {deadline}"
        ))?;
        let beneficiary_prefix = vals[11];
        let beneficiary_suffix = vals[10];
        let beneficiary_id = AccountId::try_from([beneficiary_prefix, beneficiary_suffix])
            .or(Err(anyhow!("Couldn't parse beneficiary_id from order note")))?;

        println!("beneficiary_id: {:?}", beneficiary_id);
        println!("deadline: {:?}", deadline);
        println!("p2id_tag: {:?}", p2id_tag);
        println!("asset_in: {:?}", asset_in);
        println!("asset_out: {:?}", asset_out);
        let order = Order {
            created_at: Utc::now(),
            updated_at: Utc::now(),
            deadline,
            is_limit_order: false,
            asset_in,
            asset_out,
            beneficiary_id,
            id: Uuid::new_v4(),
            p2id_tag,
            order_type: OrderType::Withdraw,
        };

        info!("New withdraw order from {}: {order:?}", beneficiary_id.to_hex());

        Ok(order)
    }
}
