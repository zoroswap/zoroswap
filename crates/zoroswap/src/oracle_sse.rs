use crate::{
    amm_state::AmmState,
    order::OracleId,
    websocket::{EventBroadcaster, OraclePriceEvent},
};
use anyhow::Result;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, str::FromStr, sync::Arc};
use tracing::{error, info};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binary {
    pub data: Vec<String>,
    pub encoding: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Price {
    pub price: u64,
    pub publish_time: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PriceMetadata {
    pub id: String,
    pub metadata: serde_json::Value,
    pub price: Price,
}

#[derive(Deserialize, Debug)]
pub struct ParsedEvent {
    pub parsed: Vec<PriceMetadata>,
    pub binary: Binary,
}

pub struct OracleSSEClient {
    state: Arc<AmmState>,
    broadcaster: Arc<EventBroadcaster>,
}

#[derive(Clone, Debug, Copy)]
pub struct PriceData {
    pub price: u64,
    pub timestamp: u64,
}

impl PriceData {
    pub fn new(timestamp: u64, price: u64) -> Self {
        Self { price, timestamp }
    }

    pub fn new_at_now(price: u64) -> Self {
        Self {
            price,
            timestamp: Utc::now().timestamp_millis() as u64,
        }
    }
}

impl OracleSSEClient {
    pub fn new(state: Arc<AmmState>, broadcaster: Arc<EventBroadcaster>) -> Self {
        Self { state, broadcaster }
    }

    pub async fn start(&mut self) -> Result<String> {
        let mut url = Url::from_str(self.state.config().oracle_sse)
            .expect("Failed parsing Oracle SSE endpoint string");
        let assets_to_listen_for: Vec<OracleId> = self
            .state
            .config()
            .liquidity_pools
            .iter()
            .map(|lp| lp.oracle_id)
            .collect::<HashSet<_>>() // to unique
            .into_iter()
            .collect();
        let query = assets_to_listen_for
            .iter()
            .map(|asset| format!("ids[]={}", asset))
            .collect::<Vec<String>>()
            .join("&");
        url.set_query(Some(&query));
        loop {
            let mut es = EventSource::get(url.clone());
            while let Some(event) = es.next().await {
                match event {
                    Ok(Event::Open) => info!("Oracle connection open!"),
                    Ok(Event::Message(message)) => {
                        if let Err(e) = self.update_prices_in_state(&message.data) {
                            error!("Failed to update oracle prices in state: {e:?}");
                        };
                    }
                    Err(err) => {
                        error!("Oracle SSE event error: {err:?}");
                        es.close();
                    }
                }
            }
            error!("Oracle SSE connection dropped.");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    pub async fn init_prices_in_state(&self) -> Result<()> {
        let ids = self
            .state
            .config()
            .liquidity_pools
            .iter()
            .map(|pool_config| pool_config.oracle_id)
            .collect::<Vec<&str>>();
        let prices = get_oracle_prices(self.state.config().oracle_https, ids).await?;
        self.state.update_oracle_prices(prices);
        Ok(())
    }

    fn update_prices_in_state(&self, message: &str) -> Result<()> {
        let parsed_json = serde_json::from_str::<ParsedEvent>(message)?;

        // Update in-memory state
        self.state.update_oracle_prices(parsed_json.parsed.clone());

        // Broadcast to WebSocket clients
        for price_update in parsed_json.parsed {
            if let Ok(faucet_id) = self.state.config().oracle_id_to_faucet_id(&price_update.id) {
                let _ = self.broadcaster.broadcast_oracle_price(OraclePriceEvent {
                    oracle_id: price_update.id.clone(),
                    faucet_id: faucet_id.to_hex(),
                    price: price_update.price.price,
                    timestamp: price_update.price.publish_time,
                });
            }
        }

        Ok(())
    }
}

pub async fn get_oracle_prices(oracle_url: &str, ids: Vec<&str>) -> Result<Vec<PriceMetadata>> {
    let mut url = Url::from_str(oracle_url)?;
    let query = ids
        .iter()
        .map(|id| format!("ids[]={}", id))
        .collect::<Vec<String>>()
        .join("&");
    url.set_query(Some(&query));
    let resp = reqwest::get(url).await?.text().await?;
    let parsed_event = serde_json::from_str::<ParsedEvent>(&resp)?;
    Ok(parsed_event.parsed)
}
