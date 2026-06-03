use anyhow::{Result, anyhow};
use chrono::Utc;
use miden_client::note::Note;
use miden_client::{Felt, Word, account::AccountId, keystore::FilesystemKeyStore, note::NoteTag};
use std::{collections::HashMap, str::FromStr};
use tracing::error;
use url::Url;
use uuid::Uuid;
use zoro_miden::note::TrustedNote;
use zoro_miden::pool::ZoroPool;
use zoro_miden::{client::MidenClient, price::PriceData};
use zoroswap::PositionGetNoteResponse;
use zoroswap::{Config, get_oracle_prices, oracle_sse::PriceMetadata};

/// Common state for E2E tests.
pub struct E2ETestSetup {
    pub config: Config,
    pub client: MidenClient,
    pub zoro_pool: ZoroPool,
    pub prices: HashMap<AccountId, PriceData>,
}

impl E2ETestSetup {
    pub async fn new() -> Result<Self> {
        dotenv::dotenv().ok();

        let config = Config::from_config_file("../../config.toml")?;

        assert!(
            config.liquidity_pools.len() > 1,
            "Less than 2 liquidity pools configured"
        );

        let mut client =
            MidenClient::new(config.miden_endpoint.clone(), "keystore", "testing_stores").await?;
        client.sync_state().await?;
        let zoro_pool = ZoroPool::new_from_existing_pool(
            config.miden_endpoint.clone(),
            &client.keystore_dir(),
            &client.store_dir(),
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
            zoro_pool,
            prices,
        })
    }
}

pub async fn send_to_server(
    server_url: &str,
    notes: Vec<String>,
    endpoint: &str,
) -> Result<Vec<String>> {
    let url = Url::from_str(format!("{server_url}/{endpoint}").as_str())?;
    let client = reqwest::Client::new();
    let mut texts = Vec::new();
    for note in notes {
        let res = client
            .post(url.clone())
            .body(serde_json::json!({ "note_data": note }).to_string())
            .header("Content-Type", "application/json")
            .send()
            .await?;
        let text = res.text().await?;
        println!("Server response: {:?}", text);
        texts.push(text);
    }

    Ok(texts)
}

pub async fn send_position_swap_to_server(
    server_url: String,
    endpoint: String,
    position_id: Uuid,
    asset_in: String,
    asset_out: String,
    amount_in: u64,
    amount_out: u64,
) -> Result<String> {
    let url = Url::from_str(format!("{server_url}/{endpoint}").as_str())?;
    let client = reqwest::Client::new();
    let res = client
        .post(url)
        .body(
            serde_json::json!({
                "position_id": position_id,
                "asset_in": asset_in,
                "asset_out": asset_out,
                "amount_in": amount_in,
                "min_amount_out": amount_out
            })
            .to_string(),
        )
        .header("Content-Type", "application/json")
        .send()
        .await?;

    let text = res.text().await?;
    println!("Server response: {:?}", text);
    Ok(text)
}

pub async fn get_position_note(
    server_url: String,
    endpoint: String,
    position_id: Uuid,
) -> Result<TrustedNote> {
    let client = reqwest::Client::new();
    let params = [("position_id", &position_id.to_string())];
    let url = Url::parse_with_params(format!("http://{server_url}/{endpoint}").as_str(), params)?;
    let res = client.get(url).send().await?;

    let text = res.text().await?;
    let payload: PositionGetNoteResponse = serde_json::from_str(&text)?;
    let note = TrustedNote::from_base64(&payload.note_data)?;
    println!("Server response: {:?}", text);
    Ok(note)
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
