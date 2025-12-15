use crate::traits::Curve;
use alloy::primitives::{I256, U256};

/// A simple linear curve implementation for demonstration purposes.
///
/// This is a basic curve that returns linear values. It's provided as a reference
/// implementation and placeholder for the proprietary curve implementation.
///
/// # Warning
///
/// This is a dummy implementation and should not be used in production.
/// To use the production curve, enable the `zoro-curve` feature.
#[cfg(feature = "dummy-curve")]
#[derive(Debug, Clone, Copy)]
pub struct DummyCurve;

#[cfg(feature = "dummy-curve")]
impl Curve for DummyCurve {
    const DECIMALS: u128 = 18;
    const MANTISSA: u128 = 1_000_000_000_000_000_000;

    fn psi(&self, b: U256, l: U256, decimals: U256) -> U256 {
        let i_b = self.convert_to_internal_decimals(b, decimals);
        let i_l = self.convert_to_internal_decimals(l, decimals);

        if i_b == U256::ZERO && i_l == U256::ZERO {
            return U256::ZERO;
        }

        // Simple linear combination: b + l
        let result = i_b.saturating_add(i_l);
        self.convert_to_external_decimals(result, decimals)
    }

    fn inverse_diagonal(&self, b: U256, l: U256, capital_b: U256, decimals: U256) -> U256 {
        let i_b = self.convert_to_internal_decimals(b, decimals);
        let i_l = self.convert_to_internal_decimals(l, decimals);
        let i_capital_b = self.convert_to_internal_decimals(capital_b, decimals);

        // Simple linear: maintain sum, so new_l = (b + l) - capital_b
        let sum = i_b.saturating_add(i_l);
        let result = if i_capital_b > sum {
            U256::ZERO
        } else {
            sum - i_capital_b
        };

        self.convert_to_external_decimals(result, decimals)
    }

    fn inverse_horizontal(&self, b: I256, l: I256, capital_b: I256, decimals: U256) -> U256 {
        let i_b = I256::try_from(self.convert_to_internal_decimals(U256::from(b), decimals))
            .unwrap_or(I256::ZERO);
        let i_l = I256::try_from(self.convert_to_internal_decimals(U256::from(l), decimals))
            .unwrap_or(I256::ZERO);
        let i_capital_b =
            I256::try_from(self.convert_to_internal_decimals(U256::from(capital_b), decimals))
                .unwrap_or(I256::ZERO);

        // Simple linear: maintain sum, so new_l = (b + l) - capital_b
        let sum = i_b.saturating_add(i_l);
        let new_l = sum.saturating_sub(i_capital_b);

        let result = if new_l < I256::ZERO {
            U256::ZERO
        } else {
            U256::from(new_l)
        };

        self.convert_to_external_decimals(result, decimals)
    }
}

#[cfg(feature = "dummy-curve")]
impl DummyCurve {
    /// Creates a new DummyCurve instance
    pub fn new(_beta: U256, _c: U256) -> Self {
        Self
    }

    fn convert_to_external_decimals(&self, value: U256, decimals: U256) -> U256 {
        if decimals > U256::from(Self::DECIMALS) {
            value * U256::from(10).pow(decimals - U256::from(Self::DECIMALS))
        } else {
            value / U256::from(10).pow(U256::from(Self::DECIMALS) - decimals)
        }
    }

    fn convert_to_internal_decimals(&self, value: U256, decimals: U256) -> U256 {
        if decimals > U256::from(Self::DECIMALS) {
            value / U256::from(10).pow(decimals - U256::from(Self::DECIMALS))
        } else {
            value * U256::from(10).pow(U256::from(Self::DECIMALS) - decimals)
        }
    }
}

#[cfg(feature = "dummy-curve")]
impl Default for DummyCurve {
    fn default() -> Self {
        Self::new(U256::ZERO, U256::ZERO)
    }
}
