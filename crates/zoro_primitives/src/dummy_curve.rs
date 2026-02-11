use crate::traits::Curve;
use alloy::primitives::{I256, U256};

/// An identity curve implementation for development and CI testing.
///
/// This curve applies no slippage or imbalance adjustments:
/// `psi(b, l) = b`, meaning `reserve_with_slippage = reserve`.
/// It's provided as a placeholder for the proprietary curve implementation.
///
/// # Warning
///
/// This is a dummy implementation and should not be used in production.
/// To use the production curve, enable the `zoro-curve-local` feature.
#[cfg(feature = "dummy-curve")]
#[derive(Debug, Clone, Copy)]
pub struct DummyCurve;

#[cfg(feature = "dummy-curve")]
impl Curve for DummyCurve {
    const DECIMALS: u128 = 18;
    const MANTISSA: u128 = 1_000_000_000_000_000_000;

    fn psi(&self, b: U256, _l: U256, _decimals: U256) -> U256 {
        // Identity curve: reserve_with_slippage = reserve
        b
    }

    fn inverse_diagonal(&self, b: U256, _l: U256, capital_b: U256, _decimals: U256) -> U256 {
        // Identity curve: fee = capital_b - b
        capital_b.saturating_sub(b)
    }

    fn inverse_horizontal(&self, b: I256, _l: I256, capital_b: I256, _decimals: U256) -> U256 {
        // Identity curve: effective_amount_in = capital_b - b
        let result = capital_b.saturating_sub(b);
        if result < I256::ZERO {
            U256::ZERO
        } else {
            U256::from(result)
        }
    }
}

#[cfg(feature = "dummy-curve")]
impl DummyCurve {
    /// Creates a new DummyCurve instance
    pub fn new(_beta: U256, _c: U256) -> Self {
        Self
    }

}

#[cfg(feature = "dummy-curve")]
impl Default for DummyCurve {
    fn default() -> Self {
        Self::new(U256::ZERO, U256::ZERO)
    }
}
