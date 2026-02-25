use std::str::FromStr;

use alloy::primitives::{I256, U256};
use anyhow::Result;
use miden_client::{Felt, Word};
use serde::Serialize;
use tracing::info;
#[cfg(not(feature = "zoro-curve-local"))]
use zoro_curve_base::dummy_curve::DummyCurve as ConfiguredCurve;
use zoro_curve_base::traits::Curve;
#[cfg(feature = "zoro-curve-local")]
use zoro_curve_local::ZoroCurve as ConfiguredCurve;

use crate::pool::LiquidityPoolConfig;

fn serialize_u256<S>(value: &U256, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&value.to_string())
}

#[derive(Clone, Debug, Copy, Serialize, Eq, PartialEq, Default)]
pub struct PoolBalances {
    #[serde(serialize_with = "serialize_u256")]
    pub reserve: U256,
    #[serde(serialize_with = "serialize_u256")]
    pub reserve_with_slippage: U256,
    #[serde(serialize_with = "serialize_u256")]
    pub total_liabilities: U256,
}

#[derive(Clone, Debug, Copy)]
pub struct PoolSettings {
    pub beta: I256,
    pub c: I256,
    pub swap_fee: U256,
    pub backstop_fee: U256,
    pub protocol_fee: U256,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            beta: I256::from_str("10000000000000000").unwrap(),
            c: I256::from_str("16000000000000000000").unwrap(),
            swap_fee: U256::from(200),
            backstop_fee: U256::from(300),
            protocol_fee: U256::from(0),
        }
    }
}

#[derive(Clone, Debug, Copy)]
pub struct PoolMetadata {
    pub name: &'static str,
    pub asset_decimals: u8,
}

impl From<&LiquidityPoolConfig> for PoolMetadata {
    fn from(value: &LiquidityPoolConfig) -> Self {
        Self {
            name: value.name,
            asset_decimals: value.decimals,
        }
    }
}

impl Default for PoolMetadata {
    fn default() -> Self {
        PoolMetadata {
            name: "Default pool",
            asset_decimals: 8,
        }
    }
}

#[derive(Clone, Debug, Copy, Default)]
pub struct PoolState {
    settings: PoolSettings,
    balances: PoolBalances,
    lp_total_supply: u64,
    metadata: PoolMetadata,
}

impl PoolState {
    pub fn default_with_settings(settings: PoolSettings) -> Self {
        Self {
            settings,
            balances: PoolBalances::default(),
            lp_total_supply: 0,
            metadata: PoolMetadata::default(),
        }
    }

    pub fn new(
        settings: PoolSettings,
        balances: PoolBalances,
        lp_total_supply: u64,
        metadata: PoolMetadata,
    ) -> Self {
        Self {
            settings,
            balances,
            lp_total_supply,
            metadata,
        }
    }
    pub fn update_state(&mut self, balances: PoolBalances, lp_total_supply: u64) {
        self.balances = balances;
        self.lp_total_supply = lp_total_supply;
    }
    pub fn update_balances(&mut self, balances: PoolBalances) {
        self.balances = balances;
    }
    pub fn balances(&self) -> &PoolBalances {
        &self.balances
    }
    pub fn settings(&self) -> &PoolSettings {
        &self.settings
    }
    pub fn metadata(&self) -> &PoolMetadata {
        &self.metadata
    }
    pub fn lp_total_supply(&self) -> u64 {
        self.lp_total_supply
    }
    /// # Returns
    ///
    /// new_lp_amount, new_lp_total_supply, new_pool_balances
    pub fn get_deposit_lp_amount_out(
        &self,
        deposit_amount: U256,
    ) -> Result<(U256, u64, PoolBalances)> {
        let lp_total_supply = U256::from(self.lp_total_supply);
        let old_total_liabilities = self.balances.total_liabilities;
        let old_reserve = self.balances.reserve;
        let old_reserve_with_slippage = self.balances.reserve_with_slippage;

        let curve =
            ConfiguredCurve::new(U256::from(self.settings.beta), U256::from(self.settings.c));

        let new_reserve_with_slippage = old_reserve_with_slippage + deposit_amount;
        let mut reserve_increment = curve.inverse_diagonal(
            old_reserve,
            old_total_liabilities,
            new_reserve_with_slippage,
            U256::from(self.metadata.asset_decimals),
        );

        // fix potential numerical imprecission
        if reserve_increment < deposit_amount {
            reserve_increment = deposit_amount;
        }
        let new_lp_amount = if old_total_liabilities > 0 {
            reserve_increment * lp_total_supply / old_total_liabilities
        } else {
            reserve_increment
        };

        let new_pool_balances = PoolBalances {
            reserve: old_reserve + reserve_increment,
            reserve_with_slippage: new_reserve_with_slippage,
            total_liabilities: old_total_liabilities + reserve_increment,
        };

        let new_lp_total_supply = lp_total_supply
            .saturating_add(new_lp_amount)
            .saturating_to::<u64>();

        Ok((new_lp_amount, new_lp_total_supply, new_pool_balances))
    }
    /// # Returns
    ///
    /// new_lp_amount, new_lp_total_supply, new_pool_balances
    pub fn get_withdraw_asset_amount_out(
        &self,
        withdraw_amount: U256,
    ) -> Result<(U256, u64, PoolBalances)> {
        let lp_total_supply = U256::from(self.lp_total_supply);
        let old_total_liabilities = self.balances.total_liabilities;
        let old_reserve = self.balances.reserve;
        let old_reserve_with_slippage = self.balances.reserve_with_slippage;

        let reserve_decrement = if lp_total_supply == 0 {
            U256::ZERO
        } else {
            (withdraw_amount * old_total_liabilities) / old_total_liabilities
        };

        let curve =
            ConfiguredCurve::new(U256::from(self.settings.beta), U256::from(self.settings.c));

        let mut new_reserve_with_slippage = curve.psi(
            old_reserve - reserve_decrement,
            old_total_liabilities - reserve_decrement,
            U256::from(self.metadata.asset_decimals),
        );

        if new_reserve_with_slippage > old_reserve_with_slippage {
            new_reserve_with_slippage = old_reserve_with_slippage;
        }

        let mut payout_amount = old_reserve_with_slippage - new_reserve_with_slippage;

        // fix potential numerical imprecission
        if payout_amount > reserve_decrement {
            payout_amount = reserve_decrement;
        }

        let new_total_liabilities = old_total_liabilities - reserve_decrement;
        let new_reserve = old_reserve - reserve_decrement;
        let new_reserve_with_slippage = old_reserve_with_slippage - payout_amount;

        let new_pool_balances = PoolBalances {
            reserve: new_reserve,
            reserve_with_slippage: new_reserve_with_slippage,
            total_liabilities: new_total_liabilities,
        };

        // `withdraw_amount` is the LP tokens being redeemed
        let new_lp_total_supply = lp_total_supply
            .saturating_sub(withdraw_amount)
            .saturating_to::<u64>();

        Ok((payout_amount, new_lp_total_supply, new_pool_balances))
    }

    pub fn print_pool_state(&self) {
        info!(
            settings = ?self.settings,
            balances = ?self.balances,
            lp_total_supply = self.lp_total_supply,
            "Pool state for liquidity pool {}",
            self.metadata.name
        );
    }

    pub fn to_lp_note_args(&self, amount: u64) -> Word {
        Word::from(&[
            Felt::new(amount),
            Felt::new(self.balances.reserve_with_slippage.to::<u64>()),
            Felt::new(self.balances.reserve.to::<u64>()),
            Felt::new(self.balances.total_liabilities.to::<u64>()),
        ])
    }
}
