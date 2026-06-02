use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
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
use uuid::Uuid;
use zoro_miden::note::TrustedNote;

use crate::{
    amm_state::AmmState,
    config::RawLiquidityPoolConfig,
    faucet::FaucetMintInstruction,
    websocket::{ConnectionManager, websocket_handler},
};

#[derive(Clone)]
pub struct AppState {
    pub amm_state: Arc<AmmState>,
    pub faucet_tx: Sender<FaucetMintInstruction>,
    pub connection_manager: Arc<ConnectionManager>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitOrderRequest {
    pub note_data: String, // Base64 encoded serialized note
}

#[derive(Debug, Serialize, Deserialize)]
struct SubmitPositionSwap {
    position_id: Uuid,
    asset_in: String,
    asset_out: String,
    amount_in: u64,
    min_amount_out: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PositionGetNote {
    position_id: Uuid,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct AddPositionResponse {
    pub success: bool,
    pub position_id: Uuid,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PositionGetNoteResponse {
    pub success: bool,
    pub note_data: String, // Base64 encoded serialized note
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PositionGetInfoResponse {
    assets: Vec<(String, u64)>,
    note_id: String,
    serial_num: String,
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
        .route("/positions/new", post(position_new))
        .route("/positions/swap", post(position_swap))
        .route("/positions/get_note", get(position_get_note))
        .route("/positions/{position_id}", get(position_get_info))
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
        .map(|p| RawLiquidityPoolConfig::from_pool_config(p, network_id.clone()))
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
        .map(|s| {
            let balances = s.value().balances();
            PoolBalancesResponse {
                faucet_id: s.key().to_bech32(network_id.clone()),
                reserve: balances.reserve.to_string(),
                reserve_with_slippage: balances.reserve_with_slippage.to_string(),
                total_liabilities: balances.total_liabilities.to_string(),
            }
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
        .map(|s| {
            let settings = s.value().settings();
            PoolSettingsResponse {
                faucet_id: s.key().to_bech32(network_id.clone()),
                swap_fee: settings.swap_fee.to_string(),
                backstop_fee: settings.backstop_fee.to_string(),
                protocol_fee: settings.protocol_fee.to_string(),
            }
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

    if !state.amm_state.liquidity_pools().contains_key(&faucet_id) {
        return Ok(Json(
            serde_json::json!({ "success": false, "message": format!("Error minting: faucet {} is not supported", faucet_id.to_hex()) }),
        ));
    }

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
            Json(
                serde_json::json!({ "success": false, "message": format!("Error minting: {e:?}") }),
            )
        }
    };
    Ok(resp)
}

async fn submit_swap(
    State(state): State<AppState>,
    Json(payload): Json<SubmitOrderRequest>,
) -> Json<SubmitOrderResponse> {
    debug!("Received swap order submission request");
    let note = match TrustedNote::from_base64(&payload.note_data) {
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
    let note_id = note.note().id();
    match state.amm_state.add_order(note, None, None) {
        Ok((_, order_id, _)) => {
            info!(
                order_id =? order_id,
                note_id = note_id.to_hex(),
                "New swap order"
            );
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
    let note = match TrustedNote::from_base64(&payload.note_data) {
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
    let note_id = note.note().id();
    match state.amm_state.add_order(note, None, None) {
        Ok((_, order_id, _)) => {
            info!(
                order_id =? order_id,
                note_id = note_id.to_hex(),
                "New deposit order"
            );
            Json(SubmitOrderResponse {
                success: true,
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
    let note = match TrustedNote::from_base64(&payload.note_data) {
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
    let note_id = note.note().id();
    match state.amm_state.add_order(note, None, None) {
        Ok((_, order_id, _)) => {
            info!(
                order_id =? order_id,
                note_id = note_id.to_hex(),
                "New withdraw order"
            );
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

async fn position_get_info(
    State(state): State<AppState>,
    Query(payload): Query<PositionGetNote>,
) -> Result<impl IntoResponse, ApiError> {
    match state.amm_state.get_position_note_info(payload.position_id) {
        Ok((assets, serial_num, note_id)) => Ok(Json(PositionGetInfoResponse {
            assets,
            serial_num,
            note_id,
        })),
        Err(e) => Err(ApiError(e)),
    }
}

async fn position_get_note(
    State(state): State<AppState>,
    Query(payload): Query<PositionGetNote>,
) -> Json<PositionGetNoteResponse> {
    match state
        .amm_state
        .get_position_note_serialized(payload.position_id)
    {
        Ok(serialized_note) => Json(PositionGetNoteResponse {
            success: true,
            note_data: serialized_note,
            message: "Getting position note successful.".to_string(),
        }),
        Err(e) => {
            error!("Failed getting position note: {:?}", e);
            Json(PositionGetNoteResponse {
                success: false,
                note_data: "".to_string(),
                message: format!("Failed to get note for position: {}", e),
            })
        }
    }
}

async fn position_swap(
    State(state): State<AppState>,
    Json(payload): Json<SubmitPositionSwap>,
) -> Json<SubmitOrderResponse> {
    match state.amm_state.add_position_order(
        payload.position_id,
        payload.asset_in,
        payload.asset_out,
        payload.amount_in,
        payload.min_amount_out,
    ) {
        Ok((note_id, order_id, _)) => {
            info!(
                order_id =? order_id,
                note_id = note_id,
                "New position swap order"
            );
            Json(SubmitOrderResponse {
                success: true,
                order_id: order_id.to_string(),
                message:
                    "Position Swap order submitted successfully. Matching engine will process it automatically."
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

async fn position_new(
    State(state): State<AppState>,
    Json(payload): Json<SubmitOrderRequest>,
) -> Json<AddPositionResponse> {
    let note = match TrustedNote::from_base64(&payload.note_data) {
        Ok(note) => note,
        Err(e) => {
            error!("Failed to deserialize note: {}", e);
            return Json(AddPositionResponse {
                success: false,
                position_id: Uuid::nil(),
                message: format!("Invalid note data: {}", e),
            });
        }
    };
    let note_id = note.note().id();
    match state.amm_state.add_position(note) {
        Ok(new_position_id) => {
            info!(
                new_position_id =? new_position_id,
                note_id = note_id.to_hex(),
                "New position opened"
            );
            Json(AddPositionResponse {
                success: true,
                position_id: new_position_id,
                message: "New position opened.".to_string(),
            })
        }
        Err(e) => {
            error!("Failed to open new position: {}", e);
            Json(AddPositionResponse {
                success: false,
                position_id: Uuid::nil(),
                message: format!("Failed to open new position: {}", e),
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
