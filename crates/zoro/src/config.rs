use anyhow::{Result, anyhow};
use miden_client::{account::AccountId, address::NetworkId, rpc::Endpoint};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self},
};

#[derive(Deserialize, Serialize)]
pub struct RawLiquidityPoolConfig {
    name: String,
    symbol: String,
    decimals: u8,
    faucet_id: String,
    oracle_id: String,
}

#[derive(Debug, Clone, Copy)]
pub struct LiquidityPoolConfig {
    pub name: &'static str,
    pub symbol: &'static str,
    pub decimals: u8,
    pub faucet_id: AccountId,
    pub oracle_id: &'static str,
}

impl LiquidityPoolConfig {
    pub fn to_raw_config(self, network_id: NetworkId) -> RawLiquidityPoolConfig {
        RawLiquidityPoolConfig {
            name: self.name.into(),
            symbol: self.symbol.into(),
            decimals: self.decimals,
            faucet_id: self.faucet_id.to_bech32(network_id),
            oracle_id: self.oracle_id.into(),
        }
    }
}

#[derive(Deserialize)]
struct RawConfig {
    pool_account_id: String,
    liquidity_pools: Vec<RawLiquidityPoolConfig>,
}

#[derive(Clone)]
struct ParsedConfig {
    pub pool_account_id: AccountId,
    pub liquidity_pools: Vec<LiquidityPoolConfig>,
}

impl RawConfig {
    fn into_pool_config(self) -> ParsedConfig {
        ParsedConfig {
            pool_account_id: AccountId::from_bech32(&self.pool_account_id)
                .unwrap_or_else(|err| {
                    panic!("Failed to parse pool_account_id from bech32: {err:?}")
                })
                .1,
            liquidity_pools: self
                .liquidity_pools
                .iter()
                .map(|lp| LiquidityPoolConfig {
                    name: Box::new(lp.name.clone()).leak(),
                    symbol: Box::new(lp.symbol.clone()).leak(),
                    decimals: lp.decimals,
                    faucet_id: AccountId::from_bech32(&lp.faucet_id)
                        .unwrap_or_else(|err| {
                            panic!("Failed to parse faucet_id from bech32: {err:?}")
                        })
                        .1,
                    oracle_id: Box::new(lp.oracle_id.clone()).leak(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub oracle_sse: &'static str,
    pub oracle_https: &'static str,
    pub pool_account_id: AccountId,
    pub miden_endpoint: Endpoint,
    pub server_url: &'static str,
    pub liquidity_pools: Vec<LiquidityPoolConfig>,
    pub amm_tick_interval: u64,
    pub network_id: NetworkId,
    pub masm_path: &'static str,
    pub keystore_path: &'static str,
    pub store_path: &'static str,
}

impl Config {
    pub fn from_config_file(
        config_path: &str,
        masm_path: &str,
        keystore_path: &str,
        store_path: &str,
    ) -> Result<Self> {
        let contents = fs::read_to_string(config_path)
            .map_err(|e| anyhow!("Error opening {config_path}: {e}"))?;
        let parsed: RawConfig = toml::from_str(&contents)?;
        let parsed_config = parsed.into_pool_config();
        let oracle_sse =
            Box::new(env::var("ORACLE_SSE").expect("Missing ORACLE_SSE in .env file.")).leak();
        let oracle_https =
            Box::new(env::var("ORACLE_HTTPS").expect("Missing ORACLE_HTTPS in .env file.")).leak();
        let server_url =
            Box::new(env::var("SERVER_URL").expect("Missing SERVER_URL in .env file.")).leak();
        let miden_endpoint =
            env::var("MIDEN_NODE_ENDPOINT").expect("Missing MIDEN_NODE_ENDPOINT in .env file.");
        let amm_tick_interval: u64 = env::var("AMM_TICK_INTERVAL_MILLIS")
            .expect("Missing AMM_TICK_INTERVAL_MILLIS in .env file.")
            .parse()
            .expect("AMM_TICK_INTERVAL_MILLIS is not a valid u64");
        let masm_path = Box::new(masm_path.to_string()).leak();
        let keystore_path = Box::new(keystore_path.to_string()).leak();
        let store_path = Box::new(store_path.to_string()).leak();
        let miden_endpoint = match miden_endpoint.as_str() {
            "testnet" => Endpoint::testnet(),
            "devnet" => Endpoint::devnet(),
            _ => Endpoint::localhost(),
        };
        let network_id = miden_endpoint.to_network_id();
        let config = Config {
            pool_account_id: parsed_config.pool_account_id,
            liquidity_pools: parsed_config.liquidity_pools,
            oracle_sse,
            oracle_https,
            network_id,
            miden_endpoint,
            server_url,
            amm_tick_interval,
            masm_path,
            keystore_path,
            store_path,
        };
        Ok(config)
    }

    pub fn oracle_id_to_faucet_id(&self, oracle_id: &str) -> Result<AccountId> {
        self.liquidity_pools
            .iter()
            .find_map(|liq_pool| {
                if liq_pool.oracle_id.eq(oracle_id) {
                    Some(liq_pool.faucet_id)
                } else {
                    None
                }
            })
            .ok_or(anyhow!(
                "No faucet found connected to oracle id: {oracle_id}"
            ))
    }
}
