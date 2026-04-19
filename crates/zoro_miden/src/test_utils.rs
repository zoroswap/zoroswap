use std::path::PathBuf;

use anyhow::{Result, anyhow};
use dotenv::dotenv;
use miden_client::{account::AccountId, rpc::Endpoint};
use rand::{Rng, distr::Alphabetic};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::{account::MidenAccount, client::MidenClient};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TestFaucetMeta {
    pub symbol: String,
    pub max_supply: u64,
    pub decimals: u8,
}
#[derive(Debug, Clone)]
pub struct TestFaucet {
    pub miden_account: MidenAccount,
    pub meta: TestFaucetMeta,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct TestFaucetRaw {
    pub miden_account: String,
    pub meta: TestFaucetMeta,
}
#[derive(Deserialize, Serialize, Debug)]
struct TestCache {
    pub accounts: Vec<String>,
    pub faucets: Vec<TestFaucetRaw>,
}

pub struct TestUtils {
    cached_accounts: Vec<MidenAccount>,
    cached_faucets: Vec<TestFaucet>,
    miden_client: MidenClient,
    keystore_path: String,
}

impl TestUtils {
    pub async fn init_env_and_tracing() -> Result<()> {
        let filter_layer = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,server=info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter_layer)
            .try_init();
        dotenv().ok();
        Ok(())
    }

    pub async fn from_cache() -> Result<Self> {
        Self::init_env_and_tracing().await?;
        let cached = load_cached_accounts_from_file()?;
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
        let endpoint = std::env::var("MIDEN_NODE_ENDPOINT")
            .map_err(|e| anyhow!("Missing MIDEN_NODE_ENDPOINT in .env file.: {e:?}"))?;
        let keystore_path = "keystore";
        let store_path = "testing_stores";
        let miden_endpoint = match endpoint.as_str() {
            "testnet" => Endpoint::testnet(),
            "devnet" => Endpoint::devnet(),
            _ => Endpoint::localhost(),
        };

        let miden_client = MidenClient::new(miden_endpoint, keystore_path, store_path).await?;
        Ok(Self {
            cached_accounts,
            cached_faucets,
            miden_client,
            keystore_path: keystore_path.to_string(),
        })
    }
    pub fn save_cached_accounts(&self) -> Result<()> {
        save_cached_accounts_to_file(&self.to_raw_format())
    }
    pub async fn add_cached_accounts(&mut self, n: usize) -> Result<()> {
        info!("Deploying {n} new accounts.");
        for _ in 0..n {
            let acc = MidenAccount::deploy_new(&mut self.miden_client, &self.keystore_path).await?;
            self.cached_accounts.push(acc);
        }
        self.save_cached_accounts()?;
        Ok(())
    }
    pub async fn add_cached_faucets(&mut self, n: usize) -> Result<()> {
        info!("Deploying {n} new faucets.");
        for _ in 0..n {
            let meta = generate_random_faucet_metadata();
            let acc = self
                .miden_client
                .deploy_new_faucet(
                    &self.keystore_path,
                    &meta.symbol,
                    meta.decimals,
                    meta.max_supply,
                )
                .await?;
            self.cached_faucets.push(TestFaucet {
                miden_account: acc,
                meta,
            });
        }
        self.save_cached_accounts()?;
        Ok(())
    }
    fn to_raw_format(&self) -> TestCache {
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
    pub async fn get_accounts(&mut self, n: usize) -> Result<Vec<MidenAccount>> {
        if self.cached_accounts.len() < n {
            self.add_cached_accounts(n - self.cached_accounts.len())
                .await?;
        }
        let accounts = &self.cached_accounts[..n];
        Ok(accounts.to_vec())
    }
    pub async fn get_faucets(&mut self, n: usize) -> Result<Vec<TestFaucet>> {
        if self.cached_faucets.len() < n {
            self.add_cached_faucets(n - self.cached_faucets.len())
                .await?;
        }
        let accounts = &self.cached_faucets[..n];
        Ok(accounts.to_vec())
    }
    pub async fn get_two_accounts_two_faucets(
        &mut self,
    ) -> Result<((MidenAccount, MidenAccount), (TestFaucet, TestFaucet))> {
        let mut accounts = self.get_accounts(2).await?.into_iter();
        let mut faucets = self.get_faucets(2).await?.into_iter();
        let (Some(acc0), Some(acc1)) = (accounts.next(), accounts.next()) else {
            return Err(anyhow!("Failed getting accounts"));
        };
        let (Some(faucet0), Some(faucet1)) = (faucets.next(), faucets.next()) else {
            return Err(anyhow!("Failed getting faucets"));
        };
        Ok(((acc0, acc1), (faucet0, faucet1)))
    }
}

pub fn clean_cache_store() -> Result<()> {
    Ok(())
}

fn create_fresh_cache_file() -> Result<TestCache> {
    let test_cache = TestCache {
        accounts: vec![],
        faucets: vec![],
    };
    save_cached_accounts_to_file(&test_cache)?;
    Ok(test_cache)
}

fn load_cached_accounts_from_file() -> Result<TestCache> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores", "test_cache.toml"]
        .iter()
        .collect();
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            let test_cache = toml::from_str(&s)?;
            Ok(test_cache)
        }
        Err(e) => {
            info!(
                "Cached accounts not initialized or corrupt. {path:?}: {e}. Creating a new cache file."
            );
            let test_cache = create_fresh_cache_file()?;
            Ok(test_cache)
        }
    }
}

fn save_cached_accounts_to_file(test_cache: &TestCache) -> Result<()> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let mut path: PathBuf = [manifest_dir, "testing_stores"].iter().collect();
    println!("{path:?} exists {:?}", path.exists());
    if !path.exists() {
        std::fs::create_dir(&path)?;
    }
    path.push(PathBuf::from("test_cache.toml"));
    let toml_str = toml::to_string_pretty(&test_cache)
        .map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
    std::fs::write(&path, toml_str).map_err(|e| anyhow!("Failed to write {path:?}: {e}"))?;
    println!("Saved test state to {path:?}");
    Ok(())
}

fn generate_random_faucet_metadata() -> TestFaucetMeta {
    let mut rng = rand::rng();
    let symbol: String = rand::rng()
        .sample_iter(&Alphabetic)
        .take(6)
        .map(char::from)
        .collect();
    TestFaucetMeta {
        decimals: rng.random_range(0..12),
        max_supply: rng.random_range(10_000_000..5_000_000_000),
        symbol: symbol.to_ascii_uppercase(),
    }
}
