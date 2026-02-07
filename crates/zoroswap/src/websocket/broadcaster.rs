use anyhow::Result;
use tokio::sync::broadcast;
use tracing::warn;

use super::messages::{OraclePriceEvent, OrderUpdateEvent, PoolStateEvent, StatsEvent};

/// Central event broadcasting system for WebSocket updates
/// Uses Tokio broadcast channels for pub/sub pattern
#[derive(Clone)]
pub struct EventBroadcaster {
    pub order_updates_tx: broadcast::Sender<OrderUpdateEvent>,
    pub pool_state_tx: broadcast::Sender<PoolStateEvent>,
    pub oracle_prices_tx: broadcast::Sender<OraclePriceEvent>,
    pub stats_tx: broadcast::Sender<StatsEvent>,
}

impl EventBroadcaster {
    /// Create a new EventBroadcaster with specified channel capacities
    pub fn new() -> Self {
        // Buffer sizes based on expected message frequency
        let (order_updates_tx, _) = broadcast::channel(1000); // High volume
        let (pool_state_tx, _) = broadcast::channel(100);
        let (oracle_prices_tx, _) = broadcast::channel(100);
        let (stats_tx, _) = broadcast::channel(10);

        Self {
            order_updates_tx,
            pool_state_tx,
            oracle_prices_tx,
            stats_tx,
        }
    }

    /// Broadcast an order update event
    pub fn broadcast_order_update(&self, event: OrderUpdateEvent) -> Result<()> {
        match self.order_updates_tx.send(event) {
            Ok(receiver_count) => {
                if receiver_count == 0 {
                    // No subscribers, but this is normal
                }
                Ok(())
            }
            Err(e) => {
                warn!("Failed to broadcast order update: {}", e);
                Ok(()) // Don't fail the operation if broadcast fails
            }
        }
    }

    /// Broadcast a pool state update event
    pub fn broadcast_pool_state(&self, event: PoolStateEvent) -> Result<()> {
        match self.pool_state_tx.send(event) {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to broadcast pool state: {}", e);
                Ok(())
            }
        }
    }

    /// Broadcast an oracle price update event
    pub fn broadcast_oracle_price(&self, event: OraclePriceEvent) -> Result<()> {
        match self.oracle_prices_tx.send(event) {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to broadcast oracle price: {}", e);
                Ok(())
            }
        }
    }

    /// Broadcast a stats update event
    pub fn broadcast_stats(&self, event: StatsEvent) -> Result<()> {
        match self.stats_tx.send(event) {
            Ok(_) => Ok(()),
            Err(e) => {
                warn!("Failed to broadcast stats: {}", e);
                Ok(())
            }
        }
    }

    /// Subscribe to order updates
    pub fn subscribe_order_updates(&self) -> broadcast::Receiver<OrderUpdateEvent> {
        self.order_updates_tx.subscribe()
    }

    /// Subscribe to pool state updates
    pub fn subscribe_pool_state(&self) -> broadcast::Receiver<PoolStateEvent> {
        self.pool_state_tx.subscribe()
    }

    /// Subscribe to oracle price updates
    pub fn subscribe_oracle_prices(&self) -> broadcast::Receiver<OraclePriceEvent> {
        self.oracle_prices_tx.subscribe()
    }

    /// Subscribe to stats updates
    pub fn subscribe_stats(&self) -> broadcast::Receiver<StatsEvent> {
        self.stats_tx.subscribe()
    }
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websocket::messages::OrderStatus;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_broadcast_order_update() {
        let broadcaster = EventBroadcaster::new();
        let mut rx = broadcaster.subscribe_order_updates();

        let event = OrderUpdateEvent {
            order_id: Uuid::new_v4(),
            note_id: "0x1234567890abcdef".to_string(),
            status: OrderStatus::Pending,
            details: crate::websocket::messages::OrderUpdateDetails {
                amount_in: 1000,
                amount_out: None,
                asset_in_faucet: "faucet1".to_string(),
                asset_out_faucet: "faucet2".to_string(),
                reason: None,
            },
            timestamp: 1234567890,
        };

        broadcaster.broadcast_order_update(event.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.order_id, event.order_id);
        assert_eq!(received.status, OrderStatus::Pending);
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let broadcaster = EventBroadcaster::new();
        let mut rx1 = broadcaster.subscribe_oracle_prices();
        let mut rx2 = broadcaster.subscribe_oracle_prices();

        let event = OraclePriceEvent {
            oracle_id: "BTC".to_string(),
            faucet_id: "faucet1".to_string(),
            price: 50000,
            timestamp: 1234567890,
        };

        broadcaster.broadcast_oracle_price(event.clone()).unwrap();

        let received1 = rx1.recv().await.unwrap();
        let received2 = rx2.recv().await.unwrap();

        assert_eq!(received1.price, 50000);
        assert_eq!(received2.price, 50000);
    }

    #[tokio::test]
    async fn test_broadcast_without_subscribers() {
        let broadcaster = EventBroadcaster::new();

        // Should not fail even with no subscribers
        let event = StatsEvent {
            open_orders: 10,
            closed_orders: 5,
            timestamp: 1234567890,
        };

        let result = broadcaster.broadcast_stats(event);
        assert!(result.is_ok());
    }
}
