use alloy::primitives::{I256, U256};
use anyhow::{Result, anyhow};
use std::str::FromStr;
use tracing::debug;

#[cfg(not(feature = "zoro-curve-local"))]
use zoro_curve_base::dummy_curve::DummyCurve as ConfiguredCurve;
use zoro_curve_base::traits::Curve;
#[cfg(feature = "zoro-curve-local")]
use zoro_curve_local::ZoroCurve as ConfiguredCurve;

use crate::pool_state::{PoolBalances, PoolState};

/// Constants for fee calculations
const FEE_PRECISION: U256 = U256::from_limbs([1_000_000, 0, 0, 0]); // 10^6
const _PRICE_SCALING_FACTOR: i128 = 1e12 as i128;

/// Calculates the amount out for a swap.
///
/// This function implements our protocol's swap calculation logic,
/// taking into account pool imbalances, fees, and slippage.
///
/// # Arguments
/// * `amount_in` - The amount of input tokens
/// * `reserve_in` - Current reserve of input tokens
/// * `reserve_out` - Current reserve of output tokens
/// * `fee` - Protocol fee (in basis points)
/// * `lp_fee` - LP fee (in basis points)
/// * `total_liabilities_in` - Total liabilities for input token
/// * `total_liabilities_out` - Total liabilities for output token
/// * `reserve_with_slippage_in` - Reserve with slippage for input token
/// * `reserve_with_slippage_out` - Reserve with slippage for output token
/// * `beta_in` - Beta parameter for input token curve
/// * `beta_out` - Beta parameter for output token curve
/// * `c_in` - C parameter for input token curve
/// * `c_out` - C parameter for output token curve
/// * `asset_decimals_in` - Decimal places for input token
/// * `asset_decimals_out` - Decimal places for output token
/// * `price` - Oracle price for the token pair
///
/// # Returns
/// The calculated amount out.
///
/// # Errors
/// Returns an error if the amount out exceeds the reserve.
pub fn get_curve_amount_out(
    base_pool: &PoolState,
    quote_pool: &PoolState,
    asset_decimals_in: U256,
    asset_decimals_out: U256,
    amount_in: U256,
    price: U256,
) -> Result<(U256, PoolBalances, PoolBalances)> {
    // log::info!("base pool: {base_pool:?}\nquote_pool: {quote_pool:?}");
    let price_scaling_factor = U256::from(_PRICE_SCALING_FACTOR);
    let fee = quote_pool.settings().backstop_fee + quote_pool.settings().protocol_fee;
    let lp_fee = quote_pool.settings().swap_fee;
    // Initialize curves by direction
    let curve_in = ConfiguredCurve::new(
        U256::from(base_pool.settings().beta),
        U256::from(base_pool.settings().c),
    );
    let curve_out = ConfiguredCurve::new(
        U256::from(quote_pool.settings().beta),
        U256::from(quote_pool.settings().c),
    );

    // COMPUTE
    // ADJUST FOR IN TOKEN POOL IMBALANCE
    debug!(
        base_pool_reserve = %base_pool.balances().reserve,
        base_pool_total_liabilities = %base_pool.balances().total_liabilities,
        base_pool_reserve_with_slippage = %base_pool.balances().reserve_with_slippage,
        amount_in = %amount_in,
        "Curve swap input pool state"
    );
    let effective_amount_in = curve_in.inverse_horizontal(
        I256::from_str(&base_pool.balances().reserve.to_string())
            .unwrap_or_else(|err| panic!("Failed to parse base_pool.reserve: {err:?}")),
        I256::from_str(&base_pool.balances().total_liabilities.to_string())
            .unwrap_or_else(|err| panic!("Failed to parse base_pool.total_liabilities: {err:?}")),
        I256::from_str(&(base_pool.balances().reserve_with_slippage + amount_in).to_string())
            .unwrap_or_else(|err| {
                panic!("Failed to parse reserve_with_slippage + amount_in: {err:?}")
            }),
        asset_decimals_in,
    );

    debug!(
        base_pool_reserve = %base_pool.balances().reserve,
        effective_amount_in = %effective_amount_in,
        reserve_plus_effective = %(base_pool.balances().reserve + effective_amount_in),
        total_liabilities = %base_pool.balances().total_liabilities,
        "Curve swap effective amount calculation"
    );
    if (base_pool.balances().reserve + effective_amount_in)
        > (U256::from(2) * base_pool.balances().total_liabilities)
    {
        return Ok((U256::ZERO, *base_pool.balances(), *quote_pool.balances()));
    }

    // AMOUNT OUT BEFORE FEES AND OUT TOKEN POOL IMBALANCE
    debug!(
        asset_decimals_in = %asset_decimals_in,
        asset_decimals_out = %asset_decimals_out,
        price = %price,
        "Curve swap decimals and price"
    );
    let scaling_factor = if asset_decimals_in > asset_decimals_out {
        price_scaling_factor * U256::from(10).pow(asset_decimals_in - asset_decimals_out)
    } else {
        price_scaling_factor / U256::from(10).pow(asset_decimals_out - asset_decimals_in)
    };

    debug!(
        effective_amount_in = %effective_amount_in,
        price = %price,
        scaling_factor = %scaling_factor,
        "Curve swap scaling"
    );
    let raw_amount_out = effective_amount_in * price / scaling_factor;

    // COMPUTE FEES
    let fee_amount = raw_amount_out * fee / FEE_PRECISION;
    let max_lp_fee = raw_amount_out * lp_fee / FEE_PRECISION;

    // ADJUST FOR OUT TOKEN POOL IMBALANCE

    // COMPUTE ACTUAL LP FEE
    let reduced_reserve_out = quote_pool.balances().reserve - raw_amount_out + fee_amount;

    debug!(
        reduced_reserve_out = %reduced_reserve_out,
        quote_pool_total_liabilities = %quote_pool.balances().total_liabilities,
        quote_pool_reserve_with_slippage = %quote_pool.balances().reserve_with_slippage,
        asset_decimals_out = %asset_decimals_out,
        "Curve swap output pool state"
    );

    let mut actual_lp_fee_amount = curve_out.inverse_diagonal(
        reduced_reserve_out,
        quote_pool.balances().total_liabilities,
        quote_pool.balances().reserve_with_slippage,
        asset_decimals_out,
    );

    actual_lp_fee_amount = actual_lp_fee_amount.min(max_lp_fee);

    // COMPUTE ACTUAL REDUCED RESERVE AND TOTAL LIABILITIES
    let actual_reduced_reserve_out = reduced_reserve_out + actual_lp_fee_amount;
    let actual_total_liabilities_out =
        quote_pool.balances().total_liabilities + actual_lp_fee_amount;

    // COMPUTE EFFECTIVE RESERVE WITH SLIPPAGE AFTER AMOUNT OUT
    let mut reserve_with_slippage_after_amount_out = curve_out.psi(
        actual_reduced_reserve_out,
        actual_total_liabilities_out,
        asset_decimals_out,
    );

    // COMPUTE ACTUAL AMOUNT OUT
    reserve_with_slippage_after_amount_out =
        reserve_with_slippage_after_amount_out.min(quote_pool.balances().reserve_with_slippage);

    if reserve_with_slippage_after_amount_out <= U256::ZERO {
        return Err(anyhow!("Amount out exceeds reserve"));
    }

    let amount_out =
        quote_pool.balances().reserve_with_slippage - reserve_with_slippage_after_amount_out;

    debug!(
        effective_amount_in = %effective_amount_in,
        raw_amount_out = %raw_amount_out,
        reserve_with_slippage_out = %quote_pool.balances().reserve_with_slippage,
        reserve_with_slippage_after_amount_out = %reserve_with_slippage_after_amount_out,
        amount_out = %amount_out,
        "Curve swap calculation"
    );

    //     reserver_with_slippage1 = reserver_with_slippage0 + amount_in
    // reserver1 =  reserver0 + effective_amount_in
    // liablities - no change

    // actual_reduced_reserve_out
    // actual_total_liabilities_out
    // reserve_with_slippage_after_amount_out

    let new_pool_balances_base = PoolBalances {
        reserve: base_pool.balances().reserve + effective_amount_in,
        reserve_with_slippage: base_pool.balances().reserve_with_slippage + amount_in,
        total_liabilities: base_pool.balances().total_liabilities,
    };

    let new_pool_balances_quote = PoolBalances {
        reserve: actual_reduced_reserve_out,
        reserve_with_slippage: reserve_with_slippage_after_amount_out,
        total_liabilities: actual_total_liabilities_out,
    };

    Ok((amount_out, new_pool_balances_base, new_pool_balances_quote))
}

#[cfg(test)]
mod tests {

    use crate::pool_state::{PoolMetadata, PoolSettings, PoolState};

    use super::*;
    use alloy::primitives::utils::parse_ether;

    #[test]
    fn test_get_curve_amount_out_basic() {
        let base_pool = PoolState::new(
            PoolSettings {
                beta: I256::from_str("5000000000000000").unwrap(),
                c: I256::from_str("17075887234393789126").unwrap(),
                swap_fee: U256::from(200),
                backstop_fee: U256::from(300),
                protocol_fee: U256::from(300),
            },
            PoolBalances {
                reserve: parse_ether("1000").unwrap(),
                reserve_with_slippage: parse_ether("1000").unwrap(),
                total_liabilities: parse_ether("1000").unwrap(),
            },
            parse_ether("1000").unwrap().to::<u64>(),
            PoolMetadata::default(),
        );
        let quote_pool = base_pool;
        let result = get_curve_amount_out(
            &base_pool,
            &quote_pool,
            U256::from(18), // asset_decimals_in
            U256::from(18), // asset_decimals_out
            parse_ether("10").unwrap(),
            U256::from(10).pow(U256::from(12)), // price = 1.0
        );

        assert!(result.is_ok());
        let amount_out = result.unwrap().0;
        println!("final amount_out: {}", amount_out);
        let expected_amount_out = U256::from(9994944708456040182u64);
        assert_eq!(amount_out, expected_amount_out);
    }

    #[test]
    fn test_get_curve_amount_out_zero_input() {
        let base_pool = PoolState::new(
            PoolSettings {
                beta: I256::from_str("1000").unwrap(),
                c: I256::from_str("2000").unwrap(),
                swap_fee: U256::from(200),
                backstop_fee: U256::from(300),
                protocol_fee: U256::from(300),
            },
            PoolBalances {
                reserve: U256::from(5_000_000_000_000_000_000u64),
                reserve_with_slippage: U256::from(9_000_000_000_000_000_000u64),
                total_liabilities: U256::from(5_000_000_000_000_000_000u64),
            },
            parse_ether("1000").unwrap().to::<u64>(),
            PoolMetadata::default(),
        );
        let quote_pool = base_pool;
        let result = get_curve_amount_out(
            &base_pool,
            &quote_pool,
            U256::from(18),
            U256::from(18),
            U256::ZERO, // 0 input
            U256::from(1_000_000_000_000_000_000u64),
        );

        assert!(result.is_ok());
        let amount_out = result.unwrap().0;
        assert_eq!(amount_out, U256::ZERO);
    }

    #[test]
    fn test_constants() {
        assert_eq!(FEE_PRECISION, U256::from(1_000_000_000_000_000_000u64));
        assert_eq!(_PRICE_SCALING_FACTOR, 1_000_000_000_000_000_000i128);
    }
}
