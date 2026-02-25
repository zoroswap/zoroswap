use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::{Felt, Word, account::AccountId, keystore::FilesystemKeyStore, note::NoteTag};
use std::{collections::HashMap, str::FromStr};
use tracing::error;
use url::Url;
use zoro_miden::pool::ZoroPool;
use zoro_miden::{client::MidenClient, price::PriceData};
use zoroswap::{Config, get_oracle_prices, oracle_sse::PriceMetadata};

/// Common state for E2E tests.
pub struct E2ETestSetup {
    pub config: Config,
    pub client: MidenClient,
    pub keystore: FilesystemKeyStore,
    pub zoro_pool: ZoroPool,
    pub prices: HashMap<AccountId, PriceData>,
}

impl E2ETestSetup {
    pub async fn new(store_path: &str) -> Result<Self> {
        dotenv::dotenv().ok();

        let config = Config::from_config_file(
            "../../config.toml",
            "../../masm",
            "../../keystore",
            store_path,
        )?;

        assert!(
            config.liquidity_pools.len() > 1,
            "Less than 2 liquidity pools configured"
        );

        let mut client = MidenClient::new(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_path,
            None,
        )
        .await?;
        let keystore = FilesystemKeyStore::new(config.keystore_path.into())?;
        client.sync_state().await?;
        let zoro_pool = ZoroPool::new_from_existing_pool(
            config.miden_endpoint.clone(),
            config.keystore_path,
            config.store_path,
            &config.pool_account_id,
            config.liquidity_pools.clone(),
        )
        .await?;

        let prices = match get_oracle_prices(
            config.oracle_https,
            config.liquidity_pools.iter().map(|p| p.oracle_id).collect(),
        )
        .await
        {
            Ok(metadatas) => {
                let mut prices = HashMap::with_capacity(config.liquidity_pools.len());
                for price in metadatas {
                    let faucet_id = config
                        .liquidity_pools
                        .iter()
                        .find_map(|p| {
                            if p.oracle_id.eq(&price.id) {
                                Some(p.faucet_id)
                            } else {
                                None
                            }
                        })
                        .unwrap();
                    prices.insert(faucet_id, price.price.into());
                }
                prices
            }
            Err(e) => {
                error!("Oracle prices fetch failed: {e}, proceeding with fake prices.");
                let timestamp = Utc::now().timestamp_millis() as u64;
                let mut prices = HashMap::with_capacity(config.liquidity_pools.len());
                for (index, pool) in config.liquidity_pools.iter().enumerate() {
                    prices.insert(
                        pool.faucet_id,
                        PriceData {
                            timestamp,
                            price: (index as u64 + 1) * 100000,
                        },
                    );
                }
                prices
            }
        };

        Ok(E2ETestSetup {
            config,
            client,
            keystore,
            zoro_pool,
            prices,
        })
    }
}

/// POST a serialized note to `{server_url}/{endpoint}/submit`.
pub async fn send_to_server(server_url: &str, note: String, endpoint: &str) -> Result<()> {
    let url = Url::from_str(format!("{server_url}/{endpoint}/submit").as_str())?;
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        .body(serde_json::json!({ "note_data": note }).to_string())
        .header("Content-Type", "application/json")
        .send()
        .await?;

    println!("Server response: {:?}", res.text().await?);
    Ok(())
}

/// Look up a single price by oracle ID from fetched price metadata.
pub fn extract_oracle_price(
    prices: &[PriceMetadata],
    oracle_id: &str,
    symbol: &str,
) -> Result<u64> {
    Ok(prices
        .iter()
        .find(|p| p.id.eq(oracle_id))
        .ok_or(anyhow!(
            "No price for {} ({}) on price oracle.",
            symbol,
            oracle_id
        ))?
        .price
        .price)
}

/// Build the 12-element input vector expected by the ZOROSWAP note script.
pub fn build_zoroswap_inputs(
    requested_asset_word: Word,
    deadline: u64,
    p2id_tag: NoteTag,
    beneficiary_id: AccountId,
    sender_id: AccountId,
) -> Vec<Felt> {
    vec![
        requested_asset_word[0],
        requested_asset_word[1],
        requested_asset_word[2],
        requested_asset_word[3],
        Felt::new(deadline),
        p2id_tag.into(),
        Felt::new(0), // padding (unused by note script)
        Felt::new(0), // padding (unused by note script)
        beneficiary_id.suffix(),
        beneficiary_id.prefix().into(),
        sender_id.suffix(),
        sender_id.prefix().into(),
    ]
}
