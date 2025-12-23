use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};
use uuid::Uuid;

use crate::server::AppState;

use super::messages::{ClientMessage, ServerMessage, SubscriptionChannel};

/// WebSocket upgrade handler
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_websocket_connection(socket, state))
}

/// Handle a WebSocket connection
async fn handle_websocket_connection(socket: WebSocket, state: AppState) {
    let conn_id = Uuid::new_v4();
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Create channel for this connection
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Register connection
    debug!(conn_id = %conn_id, "New WebSocket connection established");
    state.connection_manager.add_connection(conn_id, tx, None); // TODO: Extract IP address from request

    // Spawn sender task: forwards messages from channel to WebSocket
    let sender_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(_) = ws_sender.send(msg).await {
                break;
            }
        }
    });

    // Receiver loop: handle messages from client
    while let Some(msg_result) = ws_receiver.next().await {
        match msg_result {
            Ok(Message::Text(text)) => {
                debug!(conn_id = %conn_id, "Received text message");
                if let Err(e) = handle_text_message(&text, conn_id, &state).await {
                    error!("Error handling text message: {}", e);
                }
            }
            Ok(Message::Ping(_)) => {
                debug!(conn_id = %conn_id, "Received ping");
                state.connection_manager.update_last_pong(conn_id);
            }
            Ok(Message::Pong(_)) => {
                debug!(conn_id = %conn_id, "Received pong");
                state.connection_manager.update_last_pong(conn_id);
            }
            Ok(Message::Close(_)) => {
                debug!(conn_id = %conn_id, "Client closed connection");
                break;
            }
            Ok(Message::Binary(_)) => {
                warn!(conn_id = %conn_id, "Received unexpected binary message");
            }
            Err(e) => {
                error!(conn_id = %conn_id, "WebSocket error: {}", e);
                break;
            }
        }
    }

    // Cleanup
    state.connection_manager.remove_connection(conn_id);
    sender_task.abort();
    debug!(conn_id = %conn_id, "WebSocket connection closed");
}

/// Handle a text message from the client
async fn handle_text_message(text: &str, conn_id: Uuid, state: &AppState) -> anyhow::Result<()> {
    let client_msg: ClientMessage = serde_json::from_str(text)?;
    handle_client_message(client_msg, conn_id, state).await;
    Ok(())
}

/// Handle a parsed client message
async fn handle_client_message(msg: ClientMessage, conn_id: Uuid, state: &AppState) {
    match msg {
        ClientMessage::Subscribe { channels } => {
            debug!(conn_id = %conn_id, "Client subscribing to {} channels", channels.len());
            for channel in channels {
                debug!(conn_id = %conn_id, channel = ?channel, "Subscribing to channel");
                state.connection_manager.subscribe(conn_id, channel.clone());
                state
                    .connection_manager
                    .send_to_connection(conn_id, ServerMessage::Subscribed { channel });
            }
        }
        ClientMessage::Unsubscribe { channels } => {
            for channel in channels {
                state
                    .connection_manager
                    .unsubscribe(conn_id, channel.clone());
                state
                    .connection_manager
                    .send_to_connection(conn_id, ServerMessage::Unsubscribed { channel });
            }
        }
        ClientMessage::Ping => {
            state
                .connection_manager
                .send_to_connection(conn_id, ServerMessage::Pong);
            state.connection_manager.update_last_pong(conn_id);
        }
    }
}

/// Broadcast a server message to all subscribers of matching channels
pub fn broadcast_message(
    connection_manager: &Arc<super::connection_manager::ConnectionManager>,
    channel: SubscriptionChannel,
    message: ServerMessage,
) {
    connection_manager.broadcast_to_channel(&channel, message);
}
