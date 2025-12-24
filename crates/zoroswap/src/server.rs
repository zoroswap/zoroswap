use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderValue, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Utc;
use miden_client::account::AccountId;
use reqwest::header;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info};

use crate::{
    amm_state::AmmState,
    config::RawLiquidityPoolConfig,
    faucet::FaucetMintInstruction,
    note_serialization::deserialize_note,
    order::OrderType,
    websocket::{ConnectionManager, EventBroadcaster, websocket_handler},
};

#[derive(Clone)]
pub struct AppState {
    pub amm_state: Arc<AmmState>,
    pub faucet_tx: Sender<FaucetMintInstruction>,
    pub connection_manager: Arc<ConnectionManager>,
    pub event_broadcaster: Arc<EventBroadcaster>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitOrderRequest {
    pub note_data: String, // Base64 encoded serialized note
}

#[derive(Debug, Serialize, Deserialize)]
struct MintRequest {
    pub address: String,
    pub faucet_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitOrderResponse {
    pub success: bool,
    pub order_id: String,
    pub message: String,
}

#[derive(Debug)]
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response<Body> {
        match self {
            ApiError(e) => {
                error!("Api error: {e:?}");
                (StatusCode::INTERNAL_SERVER_ERROR).into_response()
            }
        }
    }
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/pools/info", get(pool_info))
        .route("/pools/balance", get(pool_balances))
        .route("/pools/settings", get(pool_settings))
        .route("/orders/submit", post(submit_swap))
        .route("/deposit/submit", post(submit_deposit))
        .route("/withdraw/submit", post(submit_withdraw))
        .route("/faucets/mint", post(mint_faucet))
        .route("/stats", get(get_stats))
        .route("/ws", get(websocket_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health_check() -> impl IntoResponse {
    let response = serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now()
    });

    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    (headers, Json(response))
}

async fn pool_info(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.amm_state.config();
    let network_id = config.network_id.clone();
    let pool_account_id = config.pool_account_id.to_bech32(network_id.clone());
    let pools: Vec<RawLiquidityPoolConfig> = config
        .liquidity_pools
        .iter()
        .map(|p| p.to_raw_config(network_id.clone()))
        .collect();
    let response = serde_json::json!({
        "pool_account_id": pool_account_id,
        "liquidity_pools": pools
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=120, must-revalidate"),
    );
    (headers, Json(response))
}

#[derive(Clone, Debug, Serialize)]
struct PoolBalancesResponse {
    faucet_id: String,
    reserve: String,
    reserve_with_slippage: String,
    total_liabilities: String,
}
#[derive(Clone, Debug, Serialize)]
struct PoolSettingsResponse {
    faucet_id: String,
    swap_fee: String,
    backstop_fee: String,
    protocol_fee: String,
}

async fn pool_balances(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.amm_state.config();
    let network_id = config.network_id;
    let pools = state.amm_state.liquidity_pools();
    let pool_states: Vec<PoolBalancesResponse> = pools
        .iter()
        .map(|s| PoolBalancesResponse {
            faucet_id: s.faucet_account_id.to_bech32(network_id.clone()),
            reserve: s.balances.reserve.to_string(),
            reserve_with_slippage: s.balances.reserve_with_slippage.to_string(),
            total_liabilities: s.balances.total_liabilities.to_string(),
        })
        .collect();
    let response = serde_json::json!({
        "data": pool_states
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=15, must-revalidate"),
    );
    (headers, Json(response))
}

async fn pool_settings(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.amm_state.config();
    let network_id = config.network_id;
    let pools = state.amm_state.liquidity_pools();
    let pool_states: Vec<PoolSettingsResponse> = pools
        .iter()
        .map(|s| PoolSettingsResponse {
            faucet_id: s.faucet_account_id.to_bech32(network_id.clone()),
            swap_fee: s.settings.swap_fee.to_string(),
            backstop_fee: s.settings.backstop_fee.to_string(),
            protocol_fee: s.settings.protocol_fee.to_string(),
        })
        .collect();
    let response = serde_json::json!({
        "data": pool_states
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=15, must-revalidate"),
    );
    (headers, Json(response))
}

async fn mint_faucet(
    State(state): State<AppState>,
    Json(payload): Json<MintRequest>,
) -> Result<impl IntoResponse, ApiError> {
    debug!(
        "Address {} requesting mint from {}",
        &payload.address, &payload.faucet_id
    );
    let (_, account_id) =
        AccountId::from_bech32(&payload.address).map_err(|e| ApiError(anyhow!(e)))?;
    let (_, faucet_id) =
        AccountId::from_bech32(&payload.faucet_id).map_err(|e| ApiError(anyhow!(e)))?;

    let resp = match state
        .faucet_tx
        .send(FaucetMintInstruction {
            account_id,
            faucet_id,
        })
        .await
    {
        Ok(()) => Json(serde_json::json!({ "success": true })),
        Err(e) => {
            error!("Error processing mint: {e:?}");
            Json(serde_json::json!({ "success": false, "message": "Error minting." }))
        }
    };
    Ok(resp)
}

async fn submit_swap(
    State(state): State<AppState>,
    Json(payload): Json<SubmitOrderRequest>,
) -> Json<SubmitOrderResponse> {
    debug!("Received order submission request");
    // Deserialize the note from base64
    let note = match deserialize_note(&payload.note_data) {
        Ok(note) => note,
        Err(e) => {
            error!("Failed to deserialize note: {}", e);
            return Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Invalid note data: {}", e),
            });
        }
    };

    match state.amm_state.add_order(note, OrderType::Swap) {
        Ok((_, order_id, _)) => {
            info!("Successfully added swap order: {:?}", order_id);
            Json(SubmitOrderResponse {
                success: true,
                order_id: order_id.to_string(),
                message:
                    "Swap order submitted successfully. Matching engine will process it automatically."
                        .to_string(),
            })
        }
        Err(e) => {
            error!("Failed to add swap order: {}", e);
            Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Failed to submit order: {}", e),
            })
        }
    }
}

async fn submit_deposit(
    State(state): State<AppState>,
    Json(payload): Json<SubmitOrderRequest>,
) -> Json<SubmitOrderResponse> {
    info!("Received order submission request");
    // Deserialize the note from base64
    let note = match deserialize_note(&payload.note_data) {
        Ok(note) => note,
        Err(e) => {
            error!("Failed to deserialize note: {}", e);
            return Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Invalid note data: {}", e),
            });
        }
    };

    match state.amm_state.add_order(note, OrderType::Deposit) {
        Ok((_, order_id, _)) => {
            info!("Successfully added order: {:?}", order_id);
            Json(SubmitOrderResponse {
                success: true,
                //order_id: note_id,
                order_id: order_id.to_string(),
                message:
                    "Deposit order submitted successfully. Matching engine will process it automatically."
                        .to_string(),
            })
        }
        Err(e) => {
            error!("Failed to add deposit order: {}", e);
            Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Failed to submit deposit order: {}", e),
            })
        }
    }
}

async fn submit_withdraw(
    State(state): State<AppState>,
    Json(payload): Json<SubmitOrderRequest>,
) -> Json<SubmitOrderResponse> {
    info!("Received order submission request");
    // Deserialize the note from base64
    let note = match deserialize_note(&payload.note_data) {
        Ok(note) => note,
        Err(e) => {
            error!("Failed to deserialize note: {}", e);
            return Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Invalid note data: {}", e),
            });
        }
    };

    match state.amm_state.add_order(note, OrderType::Withdraw) {
        Ok((_, order_id, _)) => {
            info!("Successfully added withdraw order: {}", order_id);
            Json(SubmitOrderResponse {
                success: true,
                order_id: order_id.to_string(),
                message:
                    "Withdraw order submitted successfully. Matching engine will process it automatically."
                        .to_string(),
            })
        }
        Err(e) => {
            error!("Failed to add withdraw order: {}", e);
            Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Failed to submit withdraw order: {}", e),
            })
        }
    }
}

async fn get_stats(State(state): State<AppState>) -> impl IntoResponse {
    let open_orders = state.amm_state.get_open_orders();
    let closed_orders = state.amm_state.get_closed_orders();
    let response = serde_json::json!({
        "open_orders": open_orders.len(),
        "closed_orders": closed_orders.len(),
        "timestamp": chrono::Utc::now(),
        "websocket": {
            "active_connections": state.connection_manager.get_connection_count(),
            "subscriptions": state.connection_manager.get_subscription_stats(),
        }
    });
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("max-age=15, must-revalidate"),
    );
    (headers, Json(response))
}
