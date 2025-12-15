use alloy::primitives::{I256, U256};

/// A trait for curve implementations used in automated market makers (AMMs).
///
/// This trait defines the core mathematical operations required for curve-based
/// AMM calculations. Implementations should provide high-precision arithmetic
/// for calculating curve parameters and solving inverse functions.
///
/// # Precision
///
/// Curve implementations typically use fixed-point arithmetic with a mantissa
/// (e.g., 10^18) to maintain precision in calculations. The `DECIMALS` and
/// `MANTISSA` constants define this precision.
///
/// # Core Operations
///
/// The trait provides three primary curve operations:
///
/// - [`psi`](Curve::psi): Calculates the curve value for given balances
/// - [`inverse_diagonal`](Curve::inverse_diagonal): Computes the inverse along the diagonal
/// - [`inverse_horizontal`](Curve::inverse_horizontal): Computes the inverse along the horizontal axis
///
/// # Example
///
/// ```ignore
/// use zoro_primitives::traits::Curve;
/// use alloy::primitives::U256;
///
/// fn calculate_swap<C: Curve>(curve: &C, b: U256, l: U256) -> U256 {
///     curve.psi(b, l, U256::from(18))
/// }
/// ```
pub trait Curve {
    /// The number of decimal places used for internal precision.
    ///
    /// This constant defines how many decimal places are used in the curve's
    /// internal calculations. Common values are 18 (matching Ethereum's wei)
    /// or other precision levels depending on the use case.
    const DECIMALS: u128;

    /// The mantissa used for fixed-point arithmetic scaling.
    ///
    /// This is typically `10^DECIMALS` and is used to scale values during
    /// multiplication and division operations to maintain precision.
    ///
    /// For example, with `DECIMALS = 18`, `MANTISSA = 10^18 = 1_000_000_000_000_000_000`.
    const MANTISSA: u128;

    /// Calculates the psi (ψ) value for the curve given balance parameters.
    ///
    /// The psi function represents the core curve equation that relates
    /// the balances of two assets in the pool. It's used to determine
    /// the invariant of the curve.
    ///
    /// # Arguments
    ///
    /// * `b` - The balance of the first asset
    /// * `l` - The balance of the second asset (often represents liquidity)
    /// * `decimals` - The number of decimal places used in the input values
    ///
    /// # Returns
    ///
    /// The calculated psi value, scaled according to the input `decimals`.
    ///
    /// # Notes
    ///
    /// Implementations should handle the case where both `b` and `l` are zero
    /// by returning zero to avoid division by zero errors.
    fn psi(&self, b: U256, l: U256, decimals: U256) -> U256;

    /// Calculates the inverse diagonal value for the curve.
    ///
    /// This function solves for the new balance after a swap along the diagonal
    /// direction of the curve. It's typically used when both assets in the pool
    /// are being changed proportionally.
    ///
    /// # Arguments
    ///
    /// * `b` - The current balance of the first asset
    /// * `l` - The current balance of the second asset
    /// * `capital_b` - The target balance after the operation
    /// * `decimals` - The number of decimal places used in the input values
    ///
    /// # Returns
    ///
    /// The calculated inverse diagonal value, which represents the new balance
    /// state after the operation, scaled according to the input `decimals`.
    ///
    /// # Implementation Notes
    ///
    /// This typically involves solving a quadratic equation of the form:
    /// `ax² + bx + c = 0` where the coefficients are derived from the curve
    /// parameters and current state.
    fn inverse_diagonal(&self, b: U256, l: U256, capital_b: U256, decimals: U256) -> U256;

    /// Calculates the inverse horizontal value for the curve.
    ///
    /// This function solves for the new balance after a swap along the horizontal
    /// direction of the curve. It's typically used when only one asset's balance
    /// is being changed while maintaining the curve invariant.
    ///
    /// # Arguments
    ///
    /// * `b` - The current balance of the first asset (can be negative in some contexts)
    /// * `l` - The current balance of the second asset (can be negative in some contexts)
    /// * `capital_b` - The target balance after the operation (can be negative in some contexts)
    /// * `decimals` - The number of decimal places used in the input values
    ///
    /// # Returns
    ///
    /// The calculated inverse horizontal value, which represents the new balance
    /// state after the operation, scaled according to the input `decimals`.
    ///
    /// # Notes
    ///
    /// Unlike `inverse_diagonal`, this function accepts signed integers (`I256`)
    /// for the balance parameters to handle cases where intermediate calculations
    /// may involve negative values, though the final result is unsigned.
    fn inverse_horizontal(&self, b: I256, l: I256, capital_b: I256, decimals: U256) -> U256;
}
