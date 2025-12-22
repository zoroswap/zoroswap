use axum::extract::ws::Message;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};
use uuid::Uuid;

use super::messages::{ServerMessage, SubscriptionChannel};

/// Metadata about a WebSocket connection
pub struct ConnectionMetadata {
    pub connected_at: DateTime<Utc>,
    pub last_pong: Arc<Mutex<DateTime<Utc>>>,
    pub ip_address: Option<String>,
}

impl ConnectionMetadata {
    pub fn new(ip_address: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            connected_at: now,
            last_pong: Arc::new(Mutex::new(now)),
            ip_address,
        }
    }
}

/// WebSocket sender with metadata
pub struct WebSocketSender {
    pub tx: mpsc::UnboundedSender<Message>,
    pub metadata: ConnectionMetadata,
}

/// Subscription statistics
pub struct SubscriptionStats {
    pub total_connections: usize,
    pub subscriptions_by_channel: HashMap<String, usize>,
}

/// Manages WebSocket connections and subscriptions
pub struct ConnectionManager {
    connections: Arc<DashMap<Uuid, WebSocketSender>>,
    subscriptions: Arc<DashMap<SubscriptionChannel, HashSet<Uuid>>>,
}

impl ConnectionManager {
    /// Create a new ConnectionManager
    pub fn new() -> Self {
        Self {
            connections: Arc::new(DashMap::new()),
            subscriptions: Arc::new(DashMap::new()),
        }
    }

    /// Add a new WebSocket connection
    pub fn add_connection(
        &self,
        conn_id: Uuid,
        tx: mpsc::UnboundedSender<Message>,
        ip_address: Option<String>,
    ) {
        let metadata = ConnectionMetadata::new(ip_address.clone());
        self.connections
            .insert(conn_id, WebSocketSender { tx, metadata });
        debug!(
            conn_id = %conn_id,
            ip = ?ip_address,
            "WebSocket connection established"
        );
    }

    /// Remove a WebSocket connection and all its subscriptions
    pub fn remove_connection(&self, conn_id: Uuid) {
        self.connections.remove(&conn_id);

        // Remove from all subscriptions
        for mut entry in self.subscriptions.iter_mut() {
            entry.value_mut().remove(&conn_id);
        }

        debug!(conn_id = %conn_id, "WebSocket connection removed");
    }

    /// Subscribe a connection to a channel
    pub fn subscribe(&self, conn_id: Uuid, channel: SubscriptionChannel) {
        self.subscriptions
            .entry(channel.clone())
            .or_insert_with(HashSet::new)
            .insert(conn_id);

        debug!(
            conn_id = %conn_id,
            channel = ?channel,
            total_subscribers = self.subscriptions.get(&channel).map(|s| s.len()).unwrap_or(0),
            "Subscription added"
        );
    }

    /// Unsubscribe a connection from a channel
    pub fn unsubscribe(&self, conn_id: Uuid, channel: SubscriptionChannel) {
        if let Some(mut subscribers) = self.subscriptions.get_mut(&channel) {
            subscribers.remove(&conn_id);
            debug!(
                conn_id = %conn_id,
                channel = ?channel,
                "Subscription removed"
            );
        }
    }

    /// Check if a connection is subscribed to a channel
    pub fn is_subscribed(&self, conn_id: Uuid, channel: &SubscriptionChannel) -> bool {
        self.subscriptions
            .get(channel)
            .map(|subscribers| subscribers.contains(&conn_id))
            .unwrap_or(false)
    }

    /// Send a message to a specific connection
    pub fn send_to_connection(&self, conn_id: Uuid, msg: ServerMessage) {
        if let Some(conn) = self.connections.get(&conn_id) {
            let json = match serde_json::to_string(&msg) {
                Ok(json) => json,
                Err(e) => {
                    warn!("Failed to serialize message: {}", e);
                    serde_json::to_string(&ServerMessage::Error {
                        message: "Failed to serialize message".to_string(),
                    })
                    .unwrap_or_default()
                }
            };

            if let Err(e) = conn.tx.send(Message::Text(json.into())) {
                warn!("Failed to send to connection {}: {}", conn_id, e);
            }
        }
    }

    /// Broadcast a message to all connections subscribed to a channel
    pub fn broadcast_to_channel(&self, channel: &SubscriptionChannel, msg: ServerMessage) {
        let subscribers = match self.subscriptions.get(channel) {
            Some(subs) => subs.clone(),
            None => {
                trace!(channel = ?channel, "No subscribers for channel");
                return; // No subscribers
            }
        };

        if subscribers.is_empty() {
            trace!(channel = ?channel, "Channel has empty subscriber list");
            return;
        }

        let json = match serde_json::to_string(&msg) {
            Ok(json) => json,
            Err(e) => {
                warn!("Failed to serialize broadcast message: {}", e);
                return;
            }
        };

        trace!(
            channel = ?channel,
            recipients = subscribers.len(),
            "Broadcasting message to subscribers"
        );

        for conn_id in subscribers.iter() {
            if let Some(conn) = self.connections.get(conn_id) {
                if let Err(e) = conn.tx.send(Message::Text(json.clone().into())) {
                    warn!("Failed to broadcast to connection {}: {}", conn_id, e);
                }
            }
        }
    }

    /// Update the last pong time for a connection
    pub fn update_last_pong(&self, conn_id: Uuid) {
        if let Some(conn) = self.connections.get(&conn_id) {
            *conn.metadata.last_pong.lock().unwrap() = Utc::now();
        }
    }

    /// Get the number of active connections
    pub fn get_connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Get subscription statistics
    pub fn get_subscription_stats(&self) -> HashMap<String, usize> {
        self.subscriptions
            .iter()
            .map(|entry| (format!("{:?}", entry.key()), entry.value().len()))
            .collect()
    }

    /// Start a heartbeat task that checks for stale connections
    pub fn start_heartbeat_task(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                self.check_stale_connections();
            }
        });
    }

    /// Check for and remove stale connections
    fn check_stale_connections(&self) {
        let now = Utc::now();
        let timeout = Duration::from_secs(60);

        let stale: Vec<Uuid> = self
            .connections
            .iter()
            .filter_map(|entry| {
                let last_pong = *entry.metadata.last_pong.lock().unwrap();
                if now
                    .signed_duration_since(last_pong)
                    .to_std()
                    .unwrap_or_default()
                    > timeout
                {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();

        for conn_id in stale {
            debug!("Removing stale connection: {}", conn_id);
            self.remove_connection(conn_id);
        }
    }
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_remove_connection() {
        let manager = ConnectionManager::new();
        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::unbounded_channel();

        manager.add_connection(conn_id, tx, Some("127.0.0.1".to_string()));
        assert_eq!(manager.get_connection_count(), 1);

        manager.remove_connection(conn_id);
        assert_eq!(manager.get_connection_count(), 0);
    }

    #[test]
    fn test_subscription_tracking() {
        let manager = ConnectionManager::new();
        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::unbounded_channel();
        let channel = SubscriptionChannel::Stats;

        manager.add_connection(conn_id, tx, None);
        manager.subscribe(conn_id, channel.clone());
        assert!(manager.is_subscribed(conn_id, &channel));

        manager.unsubscribe(conn_id, channel.clone());
        assert!(!manager.is_subscribed(conn_id, &channel));
    }

    #[test]
    fn test_remove_connection_clears_subscriptions() {
        let manager = ConnectionManager::new();
        let conn_id = Uuid::new_v4();
        let (tx, _rx) = mpsc::unbounded_channel();
        let channel = SubscriptionChannel::Stats;

        manager.add_connection(conn_id, tx, None);
        manager.subscribe(conn_id, channel.clone());
        assert!(manager.is_subscribed(conn_id, &channel));

        manager.remove_connection(conn_id);
        assert!(!manager.is_subscribed(conn_id, &channel));
    }
}
