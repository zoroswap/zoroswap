use anyhow::Result;
use rusqlite::Connection;
use tracing::{info, warn};

/// Enables WAL mode on the SQLite database for better concurrent access.
/// WAL mode allows multiple readers and one writer simultaneously.
/// This should be called once at startup before any clients are created.
pub fn enable_wal_mode(store_path: &str) -> Result<()> {
    info!("Enabling WAL mode on database: {}", store_path);
    let conn = Connection::open(store_path)?;
    // Enable WAL mode for better concurrent access
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Set busy timeout to wait for locks instead of failing immediately (5 seconds)
    conn.pragma_update(None, "busy_timeout", 5000)?;
    // Verify WAL mode was set
    let mode: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if mode.to_lowercase() != "wal" {
        warn!("Failed to enable WAL mode, current mode: {}", mode);
    } else {
        info!("SQLite WAL mode enabled successfully");
    }
    Ok(())
}
