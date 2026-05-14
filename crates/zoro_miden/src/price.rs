use alloy::primitives::U256;
use chrono::Utc;

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

    pub fn quote_with(&self, other_price: u64) -> U256 {
        let price = self.price as f64 / other_price as f64;
        U256::from(price * 1e12)
    }
}
