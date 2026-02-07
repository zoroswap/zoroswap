use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pool::PoolBalances;

/// Messages sent from client to server
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ClientMessage {
    Subscribe { channels: Vec<SubscriptionChannel> },
    Unsubscribe { channels: Vec<SubscriptionChannel> },
    Ping,
}

/// Messages sent from server to client
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type")]
pub enum ServerMessage {
    // Subscription confirmations
    Subscribed {
        channel: SubscriptionChannel,
    },
    Unsubscribed {
        channel: SubscriptionChannel,
    },

    // Data updates
    OrderUpdate {
        order_id: String,
        note_id: String,
        status: OrderStatus,
        timestamp: u64,
        details: OrderUpdateDetails,
    },
    PoolStateUpdate {
        faucet_id: String,
        balances: PoolBalances,
        timestamp: u64,
    },
    OraclePriceUpdate {
        oracle_id: String,
        faucet_id: String,
        price: u64,
        timestamp: u64,
    },
    StatsUpdate {
        open_orders: usize,
        closed_orders: usize,
        timestamp: u64,
    },

    // Control messages
    Pong,
    Error {
        message: String,
    },
}

/// Subscription channels that clients can subscribe to
#[derive(Debug, Deserialize, Serialize, Clone, Hash, Eq, PartialEq)]
#[serde(tag = "channel")]
pub enum SubscriptionChannel {
    #[serde(rename = "order_updates")]
    OrderUpdates {
        #[serde(default)]
        order_id: Option<String>,
    },
    #[serde(rename = "pool_state")]
    PoolState {
        #[serde(default)]
        faucet_id: Option<String>,
    },
    #[serde(rename = "oracle_prices")]
    OraclePrices {
        #[serde(default)]
        oracle_id: Option<String>,
    },
    #[serde(rename = "stats")]
    Stats,
}

impl SubscriptionChannel {
    /// Check if a specific subscription matches this channel
    pub fn matches(&self, other: &SubscriptionChannel) -> bool {
        match (self, other) {
            (
                SubscriptionChannel::OrderUpdates {
                    order_id: Some(id1),
                },
                SubscriptionChannel::OrderUpdates {
                    order_id: Some(id2),
                },
            ) => id1 == id2,
            (
                SubscriptionChannel::OrderUpdates { order_id: None },
                SubscriptionChannel::OrderUpdates { .. },
            ) => true,
            (
                SubscriptionChannel::PoolState {
                    faucet_id: Some(id1),
                },
                SubscriptionChannel::PoolState {
                    faucet_id: Some(id2),
                },
            ) => id1 == id2,
            (
                SubscriptionChannel::PoolState { faucet_id: None },
                SubscriptionChannel::PoolState { .. },
            ) => true,
            (
                SubscriptionChannel::OraclePrices {
                    oracle_id: Some(id1),
                },
                SubscriptionChannel::OraclePrices {
                    oracle_id: Some(id2),
                },
            ) => id1 == id2,
            (
                SubscriptionChannel::OraclePrices { oracle_id: None },
                SubscriptionChannel::OraclePrices { .. },
            ) => true,
            (SubscriptionChannel::Stats, SubscriptionChannel::Stats) => true,
            _ => false,
        }
    }
}

/// Order status in its lifecycle
#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Pending,
    Matching,
    Executed,
    Failed,
    Expired,
}

/// Details of an order update
#[derive(Debug, Serialize, Clone)]
pub struct OrderUpdateDetails {
    pub amount_in: u64,
    pub amount_out: Option<u64>,
    pub asset_in_faucet: String,
    pub asset_out_faucet: String,
    pub reason: Option<String>,
}

/// Event types for internal broadcasting
#[derive(Debug, Clone)]
pub struct OrderUpdateEvent {
    pub order_id: Uuid,
    pub note_id: String,
    pub status: OrderStatus,
    pub details: OrderUpdateDetails,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct PoolStateEvent {
    pub faucet_id: String,
    pub balances: PoolBalances,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct OraclePriceEvent {
    pub oracle_id: String,
    pub faucet_id: String,
    pub price: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone)]
pub struct StatsEvent {
    pub open_orders: usize,
    pub closed_orders: usize,
    pub timestamp: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subscription_channel_matching() {
        // Test order_updates matching
        let all_orders = SubscriptionChannel::OrderUpdates { order_id: None };
        let specific_order = SubscriptionChannel::OrderUpdates {
            order_id: Some("123".to_string()),
        };

        assert!(all_orders.matches(&specific_order));
        assert!(all_orders.matches(&all_orders));
        assert!(specific_order.matches(&specific_order));
        assert!(!specific_order.matches(&all_orders));

        // Test different channels don't match
        let pool_sub = SubscriptionChannel::PoolState { faucet_id: None };
        assert!(!all_orders.matches(&pool_sub));
    }

    #[test]
    fn test_message_serialization() {
        let msg = ServerMessage::OrderUpdate {
            order_id: "test-123".to_string(),
            note_id: "0xabcdef123456".to_string(),
            status: OrderStatus::Executed,
            timestamp: 1234567890,
            details: OrderUpdateDetails {
                amount_in: 1000,
                amount_out: Some(2000),
                asset_in_faucet: "faucet1".to_string(),
                asset_out_faucet: "faucet2".to_string(),
                reason: None,
            },
        };

        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("OrderUpdate"));
        assert!(json.contains("executed"));
        assert!(json.contains("note_id"));
    }

    #[test]
    fn test_client_message_deserialization() {
        let json = r#"{"type":"Subscribe","channels":[{"channel":"stats"}]}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();

        match msg {
            ClientMessage::Subscribe { channels } => {
                assert_eq!(channels.len(), 1);
                assert!(matches!(channels[0], SubscriptionChannel::Stats));
            }
            _ => panic!("Expected Subscribe message"),
        }
    }
}
