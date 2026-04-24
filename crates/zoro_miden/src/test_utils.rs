use std::{collections::HashMap, path::PathBuf};

use anyhow::{Result, anyhow};
use chrono::Utc;
use dotenv::dotenv;
use miden_client::{account::AccountId, rpc::Endpoint};
use rand::{Rng, distr::Alphabetic};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::{
    account::MidenAccount,
    client::MidenClient,
    note::{DepositInstructions, NoteInstructions, TrustedNote},
    pool::{LiquidityPoolConfig, ZoroPool},
};

// TODO: Move actual config module here perhaps, this is the same as `RawLiquidityPoolConfig`
#[derive(Deserialize, Serialize, Debug)]
pub struct TestRawLiquidityPoolConfig {
    name: String,
    symbol: String,
    decimals: u8,
    faucet_id: String,
    oracle_id: String,
}

impl From<&TestRawLiquidityPoolConfig> for LiquidityPoolConfig {
    fn from(value: &TestRawLiquidityPoolConfig) -> Self {
        LiquidityPoolConfig {
            name: format!("Test pool {}", &value.symbol).leak(),
            symbol: value.symbol.clone().leak(),
            decimals: value.decimals,
            faucet_id: AccountId::from_hex(&value.faucet_id).unwrap(),
            oracle_id: value.oracle_id.clone().leak(),
        }
    }
}

impl From<&LiquidityPoolConfig> for TestRawLiquidityPoolConfig {
    fn from(value: &LiquidityPoolConfig) -> Self {
        Self {
            name: value.name.to_string(),
            symbol: value.symbol.to_string(),
            decimals: value.decimals,
            faucet_id: value.faucet_id.to_hex(),
            oracle_id: value.oracle_id.to_string(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct TestFaucetMeta {
    pub symbol: String,
    pub max_supply: u64,
    pub decimals: u8,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct TestFaucetRaw {
    pub id: Uuid,
    pub miden_account: String,
    pub meta: TestFaucetMeta,
}
#[derive(Debug, Clone)]
pub struct TestFaucet {
    pub id: Uuid,
    pub miden_account: MidenAccount,
    pub meta: TestFaucetMeta,
}
impl TryFrom<&TestFaucetRaw> for TestFaucet {
    type Error = anyhow::Error;
    fn try_from(value: &TestFaucetRaw) -> std::result::Result<Self, Self::Error> {
        match AccountId::from_hex(&value.miden_account) {
            Ok(acc_id) => Ok(Self {
                id: value.id,
                miden_account: MidenAccount::new(acc_id, None),
                meta: value.meta.clone(),
            }),
            Err(e) => Err(anyhow!("Error on cached faucet: {e:?}")),
        }
    }
}
impl From<&TestFaucet> for LiquidityPoolConfig {
    fn from(value: &TestFaucet) -> Self {
        LiquidityPoolConfig {
            name: format!("Test pool {}", &value.meta.symbol).leak(),
            symbol: value.meta.symbol.clone().leak(),
            decimals: value.meta.decimals,
            faucet_id: *value.miden_account.id(),
            oracle_id: "missing",
        }
    }
}
#[derive(Debug, Clone)]
pub struct TestPool {
    pub id: Uuid,
    pub miden_account: MidenAccount,
    pub faucets: Vec<TestFaucet>,
    pub pool_configs: Vec<LiquidityPoolConfig>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct TestPoolRaw {
    pub id: Uuid,
    pub miden_account: String,
    pub faucets: Vec<Uuid>,
    pub pool_configs: Vec<TestRawLiquidityPoolConfig>,
}

#[derive(Debug, Clone)]
pub struct TestAccount {
    pub id: Uuid,
    pub miden_account: MidenAccount,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct TestAccountRaw {
    pub id: Uuid,
    pub miden_account: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct TestCache {
    pub accounts: Vec<TestAccountRaw>,
    pub faucets: Vec<TestFaucetRaw>,
    pub pools: Vec<TestPoolRaw>,
}

pub struct PoolWithMeta {
    pub zoro_pool: ZoroPool,
    pub test_pool: TestPool,
}

pub struct TestUtils {
    cached_accounts: Vec<TestAccount>,
    cached_faucets: Vec<TestFaucet>,
    cached_pools: Vec<TestPool>,
    miden_client: MidenClient,
    miden_endpoint: Endpoint,
}

impl TestUtils {
    pub async fn init_env_and_tracing() -> Result<()> {
        let filter_layer = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,server=info,miden_client=warn,rusqlite_migration=warn,h2=warn,rustls=warn,hyper=warn,miden_prover=warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter_layer)
            .try_init();
        dotenv().ok();
        Ok(())
    }

    pub fn miden_client(&self) -> &MidenClient {
        &self.miden_client
    }
    pub fn miden_client_mut(&mut self) -> &mut MidenClient {
        &mut self.miden_client
    }

    pub async fn from_cache() -> Result<Self> {
        Self::init_env_and_tracing().await?;
        let cached = load_test_cache_from_files()?;
        let cached_accounts: Vec<TestAccount> = cached
            .accounts
            .iter()
            .filter_map(|acc| match AccountId::from_hex(&acc.miden_account) {
                Ok(acc_id) => Some(TestAccount {
                    id: acc.id,
                    miden_account: MidenAccount::new(acc_id, None),
                }),
                Err(e) => {
                    warn!("Error on cached account: {e:?}");
                    None
                }
            })
            .collect();
        let cached_faucets: Vec<TestFaucet> = cached
            .faucets
            .iter()
            .map(TestFaucet::try_from)
            .filter_map(Result::ok)
            .collect();
        let cached_pools: Vec<TestPool> = cached
            .pools
            .iter()
            .filter_map(
                |pool_raw| match AccountId::from_hex(&pool_raw.miden_account) {
                    Ok(acc_id) => {
                        let faucets: Vec<TestFaucetRaw> = pool_raw
                            .faucets
                            .iter()
                            .map(|uuid| load_faucet_by_uuid(*uuid).unwrap())
                            .collect();
                        Some(TestPool {
                            id: pool_raw.id,
                            miden_account: MidenAccount::new(acc_id, None),
                            faucets: faucets
                                .iter()
                                .map(TestFaucet::try_from)
                                .filter_map(Result::ok)
                                .collect(),
                            pool_configs: pool_raw
                                .pool_configs
                                .iter()
                                .map(LiquidityPoolConfig::from)
                                .collect(),
                        })
                    }
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

        let miden_client =
            MidenClient::new(miden_endpoint.clone(), keystore_path, store_path).await?;
        Ok(Self {
            cached_accounts,
            cached_faucets,
            cached_pools,
            miden_client,
            miden_endpoint,
        })
    }
    pub async fn add_cached_accounts(&mut self, n: usize) -> Result<()> {
        info!("Deploying {n} new accounts.");
        for _ in 0..n {
            let acc = MidenAccount::deploy_new(&mut self.miden_client).await?;
            self.cached_accounts.push(TestAccount {
                id: Uuid::new_v4(),
                miden_account: acc,
            });
        }
        save_cached_accounts_to_files(&self.cached_accounts)?;
        Ok(())
    }
    pub async fn add_cached_faucets(&mut self, n: usize) -> Result<()> {
        info!("Deploying {n} new faucets.");
        for _ in 0..n {
            let meta = generate_random_faucet_metadata();
            let acc = self
                .miden_client
                .deploy_new_faucet(&meta.symbol, meta.decimals, meta.max_supply)
                .await?;
            self.cached_faucets.push(TestFaucet {
                id: Uuid::new_v4(),
                miden_account: acc,
                meta,
            });
        }
        save_cached_faucets_to_files(&self.cached_faucets)?;
        Ok(())
    }
    pub async fn add_cached_pools(&mut self, n: usize) -> Result<Vec<TestPool>> {
        info!("Deploying {n} new pools.");
        let faucets = &self.get_faucets(n * 2).await?[..];
        let pools = Vec::new();
        for i in 0..n {
            let faucet0 = faucets[i].clone();
            let faucet1 = faucets[i + 1].clone();
            let new_pool = ZoroPool::new_deployment(
                vec![(&faucet0).into(), (&faucet1).into()],
                self.miden_endpoint(),
                self.miden_client()
                    .keystore_path()
                    .to_str()
                    .ok_or(anyhow!("Missing keystore path in client"))?,
                "testing_stores",
            )
            .await?;
            self.cached_pools.push(TestPool {
                id: Uuid::new_v4(),
                miden_account: new_pool.miden_account().clone(),
                faucets: vec![faucet0.clone(), faucet1.clone()],
                pool_configs: vec![(&faucet0).into(), (&faucet1).into()],
            })
        }
        save_cached_pools_to_files(&self.cached_pools)?;
        Ok(pools)
    }
    pub async fn get_accounts(&mut self, n: usize) -> Result<Vec<TestAccount>> {
        if self.cached_accounts.len() < n {
            self.add_cached_accounts(n - self.cached_accounts.len())
                .await?;
        }
        let acc_len = self.cached_accounts.len();
        let accounts = &self.cached_accounts[acc_len - n..].to_vec();
        for acc in accounts.iter() {
            self.miden_client_mut()
                .import_account(acc.miden_account.id())
                .await?;
        }
        Ok(accounts.to_vec())
    }
    pub async fn get_funded_accounts(
        &mut self,
        n: usize,
        desired_minimal_amounts: Vec<(AccountId, u64, u64)>,
    ) -> Result<Vec<TestAccount>> {
        info!("[get_funded_accounts] {n} {desired_minimal_amounts:?}");
        let mut accounts = self.get_accounts(n).await?;
        for acc in accounts.iter_mut() {
            for (faucet_id, min_amount, mint_amount) in &desired_minimal_amounts {
                if acc.miden_account.get_balance(faucet_id).await? < *min_amount {
                    info!(
                        "[get_funded_accounts] minting for acc {}",
                        acc.miden_account.id().to_hex()
                    );
                    self.miden_client_mut()
                        .mint_asset(*faucet_id, *acc.miden_account.id(), *mint_amount)
                        .await?;
                    info!(
                        "[get_funded_accounts] minted {mint_amount} for acc {}",
                        acc.miden_account.id().to_hex()
                    );
                }
            }
        }
        info!("Funded accounts ready.");
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
    pub async fn get_pools(&mut self, n: usize) -> Result<Vec<TestPool>> {
        if self.cached_faucets.len() < n {
            self.add_cached_pools(n - self.cached_faucets.len()).await?;
        }
        let accounts = &self.cached_pools[..n];
        Ok(accounts.to_vec())
    }
    pub async fn get_funded_pools(&mut self, n: usize) -> Result<Vec<PoolWithMeta>> {
        let mut pools = self.get_pools(n).await?;
        let keystore_path = self.miden_client().keystore_path();
        let mut res = Vec::new();
        for pool in pools.iter_mut() {
            let mut zoro_pool = ZoroPool::new_from_existing_pool(
                self.miden_endpoint(),
                keystore_path.to_str().unwrap(),
                "testing_stores",
                pool.miden_account.id(),
                pool.pool_configs.clone(),
            )
            .await?;
            for (liq_config, test_faucet) in pool.pool_configs.iter().zip(&pool.faucets) {
                if zoro_pool
                    .pool_states()
                    .get(&liq_config.faucet_id)
                    .unwrap()
                    .balances()
                    .reserve
                    .eq(&0)
                {
                    info!("Initial deposit to pool");
                    let mint_amount = test_faucet.meta.max_supply / 10;
                    let acc = &self
                        .get_funded_accounts(
                            n,
                            vec![(liq_config.faucet_id, mint_amount, mint_amount)],
                        )
                        .await?[..][0];
                    self.miden_client_mut()
                        .mint_asset(liq_config.faucet_id, *acc.miden_account.id(), mint_amount)
                        .await?;
                    let deposit_note = TrustedNote::new(
                        NoteInstructions::Deposit(DepositInstructions {
                            asset_in: liq_config.faucet_id,
                            amount_in: mint_amount,
                            min_lp_amount_out: mint_amount - 100,
                            creator: *acc.miden_account.id(),
                            note_type: miden_client::note::NoteType::Public,
                            deadline: Utc::now().timestamp_millis() as u64 + 120_000,
                            p2id_tag: acc.miden_account.tag(),
                            pool_tag: pool.miden_account.tag(),
                        }),
                        self.miden_client.client().code_builder(),
                    )?;
                    self.miden_client_mut()
                        .send_note(
                            acc.miden_account.id(),
                            pool.miden_account.id(),
                            deposit_note.clone(),
                        )
                        .await?;
                    zoro_pool
                        .execute_notes(vec![deposit_note], HashMap::default())
                        .await?;
                    info!("Successfully deposited into new pool.");
                };
            }
            res.push(PoolWithMeta {
                zoro_pool,
                test_pool: pool.clone(),
            });
        }
        Ok(res)
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
        Ok(((acc0.miden_account, acc1.miden_account), (faucet0, faucet1)))
    }
    pub fn miden_endpoint(&self) -> Endpoint {
        self.miden_endpoint.clone()
    }
}

fn load_test_cache_from_files() -> Result<TestCache> {
    let accounts = load_cached_accounts_from_files()?;
    let faucets = load_cached_faucets_from_files()?;
    let pools = load_cached_pools_from_files()?;
    Ok(TestCache {
        accounts,
        faucets,
        pools,
    })
}

fn load_cached_accounts_from_files() -> Result<Vec<TestAccountRaw>> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores"].iter().collect();
    let res = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let metadata = e.metadata().unwrap();
            metadata.is_file()
                && (e.file_name().to_os_string().to_str())
                    .unwrap()
                    .starts_with("account_")
        })
        .filter_map(|f| {
            if let Ok(file) = std::fs::read_to_string(f.path())
                && let Ok(acc) = toml::from_str::<TestAccountRaw>(&file)
            {
                Some(acc)
            } else {
                None
            }
        })
        .collect();
    Ok(res)
}

fn load_cached_faucets_from_files() -> Result<Vec<TestFaucetRaw>> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores"].iter().collect();
    let res = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let metadata = e.metadata().unwrap();
            metadata.is_file()
                && (e.file_name().to_os_string().to_str())
                    .unwrap()
                    .starts_with("faucet_")
        })
        .filter_map(|f| {
            if let Ok(file) = std::fs::read_to_string(f.path())
                && let Ok(acc) = toml::from_str::<TestFaucetRaw>(&file)
            {
                Some(acc)
            } else {
                None
            }
        })
        .collect();
    Ok(res)
}

fn load_cached_pools_from_files() -> Result<Vec<TestPoolRaw>> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores"].iter().collect();
    let res = WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let metadata = e.metadata().unwrap();
            metadata.is_file()
                && (e.file_name().to_os_string().to_str())
                    .unwrap()
                    .starts_with("pool_")
        })
        .filter_map(|f| {
            if let Ok(file) = std::fs::read_to_string(f.path())
                && let Ok(acc) = toml::from_str::<TestPoolRaw>(&file)
            {
                Some(acc)
            } else {
                None
            }
        })
        .collect();
    Ok(res)
}

fn save_cached_accounts_to_files(entities: &Vec<TestAccount>) -> Result<()> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    for entity in entities {
        let filepath: PathBuf = [
            manifest_dir,
            "testing_stores",
            &format!("account_{}", entity.id),
        ]
        .iter()
        .collect();
        if !std::fs::exists(filepath.clone())? {
            let raw = TestAccountRaw {
                id: entity.id,
                miden_account: entity.miden_account.id().to_hex(),
            };
            let toml_str = toml::to_string_pretty(&raw)
                .map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
            std::fs::write(filepath, toml_str)?;
        }
    }
    Ok(())
}

fn save_cached_faucets_to_files(entities: &Vec<TestFaucet>) -> Result<()> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    for entity in entities {
        let filepath: PathBuf = [
            manifest_dir,
            "testing_stores",
            &format!("faucet_{}", entity.id),
        ]
        .iter()
        .collect();
        if !std::fs::exists(filepath.clone())? {
            let raw = TestFaucetRaw {
                id: entity.id,
                miden_account: entity.miden_account.id().to_hex(),
                meta: entity.meta.clone(),
            };
            let toml_str = toml::to_string_pretty(&raw)
                .map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
            std::fs::write(filepath, toml_str)?;
        }
    }
    Ok(())
}

fn save_cached_pools_to_files(entities: &Vec<TestPool>) -> Result<()> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    for entity in entities {
        let filepath: PathBuf = [
            manifest_dir,
            "testing_stores",
            &format!("pool_{}", entity.id),
        ]
        .iter()
        .collect();
        if !std::fs::exists(filepath.clone())? {
            let raw = TestPoolRaw {
                id: entity.id,
                miden_account: entity.miden_account.id().to_hex(),
                faucets: entity.faucets.iter().map(|f| f.id).collect(),
                pool_configs: entity
                    .pool_configs
                    .iter()
                    .map(TestRawLiquidityPoolConfig::from)
                    .collect(),
            };
            let toml_str = toml::to_string_pretty(&raw)
                .map_err(|e| anyhow!("Failed to serialize state: {e}"))?;
            std::fs::write(filepath, toml_str)?;
        }
    }
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

fn load_faucet_by_uuid(uuid: Uuid) -> Result<TestFaucetRaw> {
    let manifest_dir: &str = env!("CARGO_MANIFEST_DIR");
    let path: PathBuf = [manifest_dir, "testing_stores", &format!("faucet_{}", uuid)]
        .iter()
        .collect();
    if let Ok(file) = std::fs::read_to_string(path)
        && let Ok(faucet) = toml::from_str::<TestFaucetRaw>(&file)
    {
        Ok(faucet)
    } else {
        Err(anyhow!("Error reading faucet"))
    }
}
