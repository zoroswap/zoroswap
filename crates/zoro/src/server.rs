use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use chrono::Utc;
use miden_client::account::AccountId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tower_http::cors::CorsLayer;
use tracing::{error, info};

use crate::{
    amm_state::AmmState, config::RawLiquidityPoolConfig, faucet::FaucetMintInstruction,
    note_serialization::deserialize_note,
};

#[derive(Clone)]
pub struct AppState {
    pub amm_state: Arc<AmmState>,
    pub faucet_tx: Sender<FaucetMintInstruction>,
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
        .route("/orders/submit", post(submit_order))
        .route("/faucets/mint", post(mint_faucet))
        .route("/stats", get(get_stats))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "healthy",
        "timestamp": Utc::now()
    }))
}

async fn pool_info(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.amm_state.config();
    let network_id = config.network_id.clone();
    let pool_account_id = config.pool_account_id.to_bech32(network_id.clone());
    let pools: Vec<RawLiquidityPoolConfig> = config
        .liquidity_pools
        .iter()
        .map(|p| p.to_raw_config(network_id.clone()))
        .collect();
    Json(serde_json::json!({
        "pool_account_id": pool_account_id,
        "liquidity_pools": pools
    }))
}

async fn mint_faucet(
    State(state): State<AppState>,
    Json(payload): Json<MintRequest>,
) -> Result<impl IntoResponse, ApiError> {
    info!(
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

async fn submit_order(
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

    match state.amm_state.add_order(note) {
        Ok(order_id) => {
            info!("Successfully added order: {}", order_id);
            Json(SubmitOrderResponse {
                success: true,
                order_id,
                message:
                    "Order submitted successfully. Matching engine will process automatically."
                        .to_string(),
            })
        }
        Err(e) => {
            error!("Failed to add order: {}", e);
            Json(SubmitOrderResponse {
                success: false,
                order_id: "".to_string(),
                message: format!("Failed to submit order: {}", e),
            })
        }
    }
}

async fn get_stats(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let open_orders = state.amm_state.get_open_orders();
    let closed_orders = state.amm_state.get_closed_orders();
    Ok(Json(serde_json::json!({
        "open_orders": open_orders.len(),
        "closed_orders": closed_orders.len(),
        "timestamp": chrono::Utc::now()
    })))
}
