use std::path::PathBuf;

use anyhow::{Result, anyhow};
use miden_client::account::AccountId;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::account::MidenAccount;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TestFaucetConfig {
    pub symbol: String,
    pub max_supply: u64,
    pub decimals: u8,
}
#[derive(Debug)]
pub struct TestFaucet {
    pub miden_account: MidenAccount,
    pub meta: TestFaucetConfig,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct TestFaucetRaw {
    pub miden_account: String,
    pub meta: TestFaucetConfig,
}
#[derive(Deserialize, Serialize, Debug)]
struct TestCache {
    pub accounts: Vec<String>,
    pub faucets: Vec<TestFaucetRaw>,
}

pub struct TestUtils {
    cached_accounts: Vec<MidenAccount>,
    cached_faucets: Vec<TestFaucet>,
}

impl TestUtils {
    pub fn from_cache() -> Result<Self> {
        let cached = load_cached_accounts()?;
        let cached_accounts: Vec<MidenAccount> = cached
            .accounts
            .iter()
            .filter_map(|string_id| match AccountId::from_hex(string_id) {
                Ok(acc_id) => Some(MidenAccount::new(acc_id, None)),
                Err(e) => {
                    warn!("Error on cached account: {e:?}");
                    None
                }
            })
            .collect();
        let cached_faucets: Vec<TestFaucet> = cached
            .faucets
            .iter()
            .filter_map(
                |faucet_raw| match AccountId::from_hex(&faucet_raw.miden_account) {
                    Ok(acc_id) => Some(TestFaucet {
                        miden_account: MidenAccount::new(acc_id, None),
                        meta: faucet_raw.meta.clone(),
                    }),
                    Err(e) => {
                        warn!("Error on cached faucet: {e:?}");
                        None
                    }
                },
            )
            .collect();
        Ok(Self {
            cached_accounts,
            cached_faucets,
        })
    }
    pub fn save_cached_accounts(&self) -> Result<()> {
        save_cached_accounts(&self.to_cache())
    }
    fn to_cache(&self) -> TestCache {
        TestCache {
            accounts: self
                .cached_accounts
                .iter()
                .map(|a| a.id().to_hex())
                .collect(),
            faucets: self
                .cached_faucets
                .iter()
                .map(|f| TestFaucetRaw {
                    miden_account: f.miden_account.id().to_hex(),
                    meta: f.meta.clone(),
                })
                .collect(),
        }
    }
}

pub fn clean_cache_store() -> Result<()> {
    Ok(())
}

fn load_cached_accounts() -> Result<TestCache> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores", "test_cache.toml"]
        .iter()
        .collect();
    let s = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("Error reading cached accounts. {path:?}: {e}"))?;
    let test_cache = toml::from_str(&s)?;
    Ok(test_cache)
}

fn save_cached_accounts(test_cache: &TestCache) -> Result<()> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores", "test_cache.toml"]
        .iter()
        .collect();
    let toml_str = toml::to_string_pretty(&test_cache)
        .map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
    std::fs::write(&path, toml_str).map_err(|e| anyhow!("Failed to write {path:?}: {e}"))?;
    println!("Saved test state to {path:?}");
    Ok(())
}
