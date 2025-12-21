use alloy::primitives::{I256, U256};
use anyhow::{Result, anyhow};
use miden_assembly::Assembler;
use miden_client::{
    Felt,
    account::AccountId,
    transaction::{TransactionRequest, TransactionRequestBuilder, TransactionScript},
};
use miden_lib::transaction::TransactionKernel;
use std::{collections::HashMap, fs, path::Path, str::FromStr};
use tracing::info;

#[cfg(feature = "zoro-curve-local")]
use zoro_curve_local::ZoroCurve as ConfiguredCurve;
use zoro_miden_client::{MidenClient, create_library};
#[cfg(not(feature = "zoro-curve-local"))]
use zoro_primitives::dummy_curve::DummyCurve as ConfiguredCurve;
use zoro_primitives::traits::Curve;

#[derive(Clone, Debug, Copy, PartialEq, Eq)]
pub struct PoolBalances {
    pub reserve: U256,
    pub reserve_with_slippage: U256,
    pub total_liabilities: U256,
}

#[derive(Clone, Debug, Copy)]
pub struct PoolState {
    pub settings: PoolSettings,
    pub balances: PoolBalances,
    pub pool_account_id: AccountId,
    pub faucet_account_id: AccountId,
    pub lp_total_supply: u64,
}

#[derive(Clone, Debug, Copy)]
pub struct PoolSettings {
    beta: I256,
    c: I256,
    swap_fee: U256,
    backstop_fee: U256,
    protocol_fee: U256,
}

impl PoolState {
    pub fn new(pool_account_id: AccountId, faucet_account_id: AccountId) -> Self {
        Self {
            pool_account_id,
            faucet_account_id,
            ..Default::default()
        }
    }

    pub fn update_state(&mut self, new_balances: PoolBalances) -> AccountId {
        self.balances = new_balances;
        self.faucet_account_id
    }

    pub async fn sync_from_chain(
        &mut self,
        client: &mut MidenClient,
        index_in_pool: u8,
    ) -> Result<()> {
        let (new_pool_balances, new_pool_settings) =
            fetch_pool_state_from_chain(client, self.pool_account_id, index_in_pool).await?;
        let lp_total_supply =
            fetch_initial_lp_max_supply_from_chain(client, self.pool_account_id, index_in_pool)
                .await?;
        self.balances = new_pool_balances;
        self.settings = new_pool_settings;
        self.lp_total_supply = lp_total_supply;
        info!(
            "Set liq pool on account {} for asset {}\nBalances: {:?}\nSettings: {:?}\nLP total supply: {}",
            self.pool_account_id,
            self.faucet_account_id,
            self.balances,
            self.settings,
            self.lp_total_supply
        );
        Ok(())
    }
}

pub async fn fetch_initial_lp_max_supply_from_chain(
    client: &mut MidenClient,
    pool_account_id: AccountId,
    index_in_pool: u8,
) -> Result<u64> {
    client.sync_state().await?;
    let account = client.get_account(pool_account_id).await?.ok_or(anyhow!(
        "No account found on chain for account_id {}",
        pool_account_id
    ))?;
    let account_storage = account.account().storage();
    let asset_mapping_index = [
        Felt::new(index_in_pool as u64),
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
    ];
    let mut asset_address = account_storage.get_map_item(9, asset_mapping_index.into())?;
    asset_address.reverse();
    let total_supply = account_storage.get_map_item(11, asset_address)?[3].as_int();
    Ok(total_supply)
}

pub async fn fetch_pool_state_from_chain(
    client: &mut MidenClient,
    pool_account_id: AccountId,
    index_in_pool: u8,
) -> Result<(PoolBalances, PoolSettings)> {
    client.sync_state().await?;
    let account = client.get_account(pool_account_id).await?.ok_or(anyhow!(
        "No account found on chain for account_id {}",
        pool_account_id
    ))?;
    let account_storage = account.account().storage();
    let asset_mapping_index = [
        Felt::new(index_in_pool as u64),
        Felt::new(0),
        Felt::new(0),
        Felt::new(0),
    ];
    let asset_address = account_storage.get_map_item(9, asset_mapping_index.into())?;
    let pool_balances = account_storage.get_map_item(10, asset_address)?;

    // let pool_balances = account_storage.get_item(3 + index_in_pool)?;
    let pool_fees = account_storage.get_item(5 + index_in_pool)?;
    let pool_curve = account_storage.get_item(7 + index_in_pool)?;

    let pool_balances = PoolBalances {
        reserve_with_slippage: U256::from(pool_balances[1].as_int()),
        reserve: U256::from(pool_balances[2].as_int()),
        total_liabilities: U256::from(pool_balances[3].as_int()),
    };
    let pool_settings = PoolSettings {
        beta: I256::from_str(&pool_curve[0].as_int().to_string())?,
        c: I256::from_str(&pool_curve[1].as_int().to_string())?,
        swap_fee: U256::from(pool_fees[0].as_int()),
        backstop_fee: U256::from(pool_fees[1].as_int()),
        protocol_fee: U256::from(pool_fees[2].as_int()),
    };
    Ok((pool_balances, pool_settings))
}

// Used in tests and bin's.
#[allow(dead_code)]
pub async fn fetch_vault_for_account_from_chain(
    client: &mut MidenClient,
    account_id: AccountId,
) -> Result<HashMap<AccountId, u64>> {
    client.sync_state().await?;
    let account = client.get_account(account_id).await?.ok_or(anyhow!(
        "No account found on chain for account_id {}",
        account_id
    ))?;
    let mut assets: HashMap<AccountId, u64> = HashMap::new();
    let account_storage = account.account().vault();
    for asset in account_storage.assets() {
        let faucet_id = asset
            .vault_key()
            .faucet_id()
            .expect("no faucet id, this is not a fungible asset");
        let amount = account_storage.get_balance(faucet_id)?;
        assets.insert(faucet_id, amount);
    }
    Ok(assets)
}

impl Default for PoolState {
    fn default() -> Self {
        Self {
            balances: PoolBalances {
                reserve: U256::from(0),
                reserve_with_slippage: U256::from(0),
                total_liabilities: U256::from(0),
            },
            faucet_account_id: AccountId::from_hex("0x000000000000000000000000000000")
                .unwrap_or_else(|err| panic!("Failed to parse default faucet_account_id: {err:?}")),
            pool_account_id: AccountId::from_hex("0x000000000000000000000000000000")
                .unwrap_or_else(|err| panic!("Failed to parse default pool_account_id: {err:?}")),
            settings: PoolSettings {
                beta: I256::from_str("0")
                    .unwrap_or_else(|err| panic!("Failed to parse default beta: {err:?}")),
                c: I256::from_str("0")
                    .unwrap_or_else(|err| panic!("Failed to parse default c: {err:?}")),
                swap_fee: U256::from(0),
                backstop_fee: U256::from(0),
                protocol_fee: U256::from(0),
            },
            lp_total_supply: 0,
        }
    }
}

pub fn create_set_pool_state_tx(
    pool_num: usize,
    balances: PoolBalances,
    masm_path: &'static str,
) -> Result<TransactionRequest> {
    let script_path = format!("{masm_path}/scripts/set_pool{pool_num}_state.masm");
    let script_code = fs::read_to_string(Path::new(&script_path))
        .map_err(|e| anyhow!("Error opening {script_path}: {e}"))?;
    let pool_account_path = format!("{masm_path}/accounts/two_asset_pool.masm");
    let pool_code = fs::read_to_string(Path::new(&pool_account_path))
        .map_err(|e| anyhow!("Error opening {pool_account_path}: {e}"))?;
    let assembler: Assembler = TransactionKernel::assembler().with_debug_mode(true);
    let account_component_lib = create_library(
        assembler.clone(),
        "external_contract::two_pools_contract",
        &pool_code,
    )
    .unwrap_or_else(|err| panic!("Failed to create library: {err:?}"));

    let program = assembler
        .clone()
        .with_dynamic_library(&account_component_lib)
        .unwrap_or_else(|err| panic!("Failed to add dynamic library: {err:?}"))
        .assemble_program(script_code)
        .unwrap_or_else(|err| panic!("Failed to assemble program: {err:?}"));
    let tx_script = TransactionScript::new(program);

    // Build a transaction request with the custom script
    let tx_request = TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .script_arg(
            [
                Felt::new(balances.total_liabilities.to::<u64>()),
                Felt::new(balances.reserve_with_slippage.to::<u64>()),
                Felt::new(balances.reserve.to::<u64>()),
                Felt::new(0),
            ]
            .into(),
        )
        .build()?;

    Ok(tx_request)
}

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
    let fee = quote_pool.settings.backstop_fee + quote_pool.settings.protocol_fee;
    let lp_fee = quote_pool.settings.swap_fee;
    // Initialize curves by direction
    let curve_in = ConfiguredCurve::new(
        U256::from(base_pool.settings.beta),
        U256::from(base_pool.settings.c),
    );
    let curve_out = ConfiguredCurve::new(
        U256::from(quote_pool.settings.beta),
        U256::from(quote_pool.settings.c),
    );

    // COMPUTE
    // ADJUST FOR IN TOKEN POOL IMBALANCE
    println!(
        "base pool reserve: {:?} base pool total liabilities: {:?} base pool reserve with slippage: {:?} amount in: {:?}",
        base_pool.balances.reserve,
        base_pool.balances.total_liabilities,
        base_pool.balances.reserve_with_slippage,
        amount_in
    );
    let effective_amount_in = curve_in.inverse_horizontal(
        I256::from_str(&base_pool.balances.reserve.to_string())
            .unwrap_or_else(|err| panic!("Failed to parse base_pool.reserve: {err:?}")),
        I256::from_str(&base_pool.balances.total_liabilities.to_string())
            .unwrap_or_else(|err| panic!("Failed to parse base_pool.total_liabilities: {err:?}")),
        I256::from_str(&(base_pool.balances.reserve_with_slippage + amount_in).to_string())
            .unwrap_or_else(|err| {
                panic!("Failed to parse reserve_with_slippage + amount_in: {err:?}")
            }),
        asset_decimals_in,
    );

    println!(
        "basePool reserve: {:?} effective_amount_in: {:?}",
        base_pool.balances.reserve, effective_amount_in
    );
    println!(
        "base pool reserve + effective_amount_in: {:?}",
        base_pool.balances.reserve + effective_amount_in
    );
    println!(
        "base pool total liabilities: {:?}",
        base_pool.balances.total_liabilities
    );
    if (base_pool.balances.reserve + effective_amount_in)
        > (U256::from(2) * base_pool.balances.total_liabilities)
    {
        return Ok((U256::ZERO, base_pool.balances, quote_pool.balances));
    }

    // AMOUNT OUT BEFORE FEES AND OUT TOKEN POOL IMBALANCE
    println!(
        "asset_decimals_in: {:?} asset_decimals_out: {:?}, price: {:?}",
        asset_decimals_in, asset_decimals_out, price
    );
    let scaling_factor = if asset_decimals_in > asset_decimals_out {
        price_scaling_factor * U256::from(10).pow(asset_decimals_in - asset_decimals_out)
    } else {
        price_scaling_factor / U256::from(10).pow(asset_decimals_out - asset_decimals_in)
    };

    println!(
        "effective_amount_in: {:?} price: {:?} scaling_factor: {:?}",
        effective_amount_in, price, scaling_factor
    );
    let raw_amount_out = effective_amount_in * price / scaling_factor;

    // COMPUTE FEES
    let fee_amount = raw_amount_out * fee / FEE_PRECISION;
    let max_lp_fee = raw_amount_out * lp_fee / FEE_PRECISION;

    // ADJUST FOR OUT TOKEN POOL IMBALANCE

    // COMPUTE ACTUAL LP FEE
    let reduced_reserve_out = quote_pool.balances.reserve - raw_amount_out + fee_amount;

    println!(
        "LP AMOUNT out
        reduced_reserve_out {},
        quote_pool.total_liabilities: {},
        quote_pool.reserve_with_slippage : {},
        asset_decimals_out: {},
        ",
        reduced_reserve_out,
        quote_pool.balances.total_liabilities,
        quote_pool.balances.reserve_with_slippage,
        asset_decimals_out
    );

    let mut actual_lp_fee_amount = curve_out.inverse_diagonal(
        reduced_reserve_out,
        quote_pool.balances.total_liabilities,
        quote_pool.balances.reserve_with_slippage,
        asset_decimals_out,
    );

    actual_lp_fee_amount = actual_lp_fee_amount.min(max_lp_fee);

    // COMPUTE ACTUAL REDUCED RESERVE AND TOTAL LIABILITIES
    let actual_reduced_reserve_out = reduced_reserve_out + actual_lp_fee_amount;
    let actual_total_liabilities_out = quote_pool.balances.total_liabilities + actual_lp_fee_amount;

    // COMPUTE EFFECTIVE RESERVE WITH SLIPPAGE AFTER AMOUNT OUT
    let mut reserve_with_slippage_after_amount_out = curve_out.psi(
        actual_reduced_reserve_out,
        actual_total_liabilities_out,
        asset_decimals_out,
    );

    // COMPUTE ACTUAL AMOUNT OUT
    reserve_with_slippage_after_amount_out =
        reserve_with_slippage_after_amount_out.min(quote_pool.balances.reserve_with_slippage);

    if reserve_with_slippage_after_amount_out <= U256::ZERO {
        return Err(anyhow!("Amount out exceeds reserve"));
    }

    let amount_out =
        quote_pool.balances.reserve_with_slippage - reserve_with_slippage_after_amount_out;

    // Debug logging (remove in production)
    println!("Debug values:");
    println!("  effective_amount_in: {}", effective_amount_in);
    println!("  raw_amount_out: {}", raw_amount_out);
    println!(
        "  reserve_with_slippage_out: {}",
        quote_pool.balances.reserve_with_slippage
    );
    println!(
        "  reserve_with_slippage_after_amount_out: {}",
        reserve_with_slippage_after_amount_out
    );
    println!("  amount_out: {}", amount_out);

    //     reserver_with_slippage1 = reserver_with_slippage0 + amount_in
    // reserver1 =  reserver0 + effective_amount_in
    // liablities - no change

    // actual_reduced_reserve_out
    // actual_total_liabilities_out
    // reserve_with_slippage_after_amount_out

    let new_pool_balances_base = PoolBalances {
        reserve: base_pool.balances.reserve + effective_amount_in,
        reserve_with_slippage: base_pool.balances.reserve_with_slippage + amount_in,
        total_liabilities: base_pool.balances.total_liabilities,
    };

    let new_pool_balances_quote = PoolBalances {
        reserve: actual_reduced_reserve_out,
        reserve_with_slippage: reserve_with_slippage_after_amount_out,
        total_liabilities: actual_total_liabilities_out,
    };

    Ok((amount_out, new_pool_balances_base, new_pool_balances_quote))
}

pub fn get_deposit_lp_amount_out(
    pool: &PoolState,
    deposit_amount: U256,
    old_total_supply: U256,
    asset_decimals: U256,
) -> Result<(U256, PoolBalances)> {
    // Cache to save some sloads
    let old_total_liabilities = pool.balances.total_liabilities;
    let old_reserve = pool.balances.reserve;
    let old_reserve_with_slippage = pool.balances.reserve_with_slippage;

    let curve = ConfiguredCurve::new(U256::from(pool.settings.beta), U256::from(pool.settings.c));

    let new_reserve_with_slippage = old_reserve_with_slippage + deposit_amount;
    let mut reserve_increment = curve.inverse_diagonal(
        old_reserve,
        old_total_liabilities,
        new_reserve_with_slippage,
        asset_decimals,
    );

    // fix potential numerical imprecission
    if reserve_increment < deposit_amount {
        reserve_increment = deposit_amount;
    }
    let new_lp_amount = if old_total_liabilities > 0 {
        reserve_increment * old_total_supply / old_total_liabilities
    } else {
        reserve_increment
    };

    let new_pool_balances = PoolBalances {
        reserve: old_reserve + reserve_increment,
        reserve_with_slippage: new_reserve_with_slippage,
        total_liabilities: old_total_liabilities + reserve_increment,
    };

    Ok((new_lp_amount, new_pool_balances))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::utils::parse_ether;

    #[test]
    fn test_get_curve_amount_out_basic() {
        let base_pool = PoolState {
            settings: PoolSettings {
                beta: I256::from_str("5000000000000000").unwrap(),
                c: I256::from_str("17075887234393789126").unwrap(),
                swap_fee: U256::from(200),
                backstop_fee: U256::from(300),
                protocol_fee: U256::from(300),
            },
            balances: PoolBalances {
                reserve: parse_ether("1000").unwrap(),
                reserve_with_slippage: parse_ether("1000").unwrap(),
                total_liabilities: parse_ether("1000").unwrap(),
            },
            pool_account_id: AccountId::from_hex("0x000000000000000000000000000000").unwrap(),
            faucet_account_id: AccountId::from_hex("0x000000000000000000000000000000").unwrap(),
        };
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
        println!("final   amount_out: {}", amount_out);
        let expected_amount_out = U256::from(9994944708456040182u64);
        assert_eq!(amount_out, expected_amount_out);
    }

    #[test]
    fn test_get_curve_amount_out_zero_input() {
        let base_pool = PoolState {
            settings: PoolSettings {
                beta: I256::from_str("1000").unwrap(),
                c: I256::from_str("2000").unwrap(),
                swap_fee: U256::from(200),
                backstop_fee: U256::from(300),
                protocol_fee: U256::from(300),
            },
            balances: PoolBalances {
                reserve: U256::from(5_000_000_000_000_000_000u64),
                reserve_with_slippage: U256::from(9_000_000_000_000_000_000u64),
                total_liabilities: U256::from(5_000_000_000_000_000_000u64),
            },
            pool_account_id: AccountId::from_hex("0x000000000000000000000000000000").unwrap(),
            faucet_account_id: AccountId::from_hex("0x000000000000000000000000000000").unwrap(),
        };
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
