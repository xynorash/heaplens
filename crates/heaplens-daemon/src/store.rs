use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{params, Connection};
use tokio::sync::mpsc as tokio_mpsc;
use tracing::{info, warn};

use crate::msg::StoreMsg;

/// Open (or create) the SQLite database at `path`. Returns the sender channel.
///
/// Spawns a bridge tokio task and a dedicated `std::thread` for the store.
/// The store thread collects `StoreMsg::Nodes` batches for up to 100ms, then
/// commits all pending rows in a single transaction. `StoreMsg::Flush` triggers
/// an early commit; `StoreMsg::Shutdown` commits any remaining rows and exits.
pub fn open(path: &str) -> Result<(tokio_mpsc::UnboundedSender<StoreMsg>, std::thread::JoinHandle<()>)> {
    let conn = Connection::open(path)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS nodes (
            id      INTEGER NOT NULL,
            ptr     INTEGER NOT NULL,
            size    INTEGER NOT NULL,
            ts      INTEGER NOT NULL,
            symbol  TEXT    NOT NULL,
            state   TEXT    NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ts ON nodes(ts);
        CREATE TABLE IF NOT EXISTS orphan_events (
            node_id               INTEGER NOT NULL,
            owner_free_ts_ns      INTEGER NOT NULL,
            orphan_detected_ts_ns INTEGER NOT NULL,
            tau_ms                INTEGER NOT NULL
        );",
    )?;

    let (std_tx, std_rx) = std_mpsc::channel::<StoreMsg>();
    let (tokio_tx, mut tokio_rx) = tokio_mpsc::unbounded_channel::<StoreMsg>();

    // Bridge: tokio receiver → std sender
    tokio::spawn(async move {
        while let Some(msg) = tokio_rx.recv().await {
            if std_tx.send(msg).is_err() {
                break;
            }
        }
    });

    // Store thread: owns Connection, batches inserts every 100ms on a fixed deadline.
    let handle = thread::spawn(move || {
        let batch_interval = Duration::from_millis(100);
        let mut batch: Vec<heaplens_protocol::NodeDto> = Vec::new();
        let mut orphan_batch: Vec<crate::msg::OrphanEventRecord> = Vec::new();
        let mut deadline = std::time::Instant::now() + batch_interval;

        loop {
            let now = std::time::Instant::now();
            let timeout = if now >= deadline {
                Duration::ZERO
            } else {
                deadline - now
            };

            match std_rx.recv_timeout(timeout) {
                Ok(StoreMsg::Nodes(dtos)) => {
                    batch.extend(dtos);
                    // Do NOT reset deadline — let it fire at the fixed interval.
                }
                Ok(StoreMsg::OrphanEvents(records)) => {
                    orphan_batch.extend(records);
                    // Same fixed-interval batching as the Nodes path above.
                }
                Ok(StoreMsg::Flush) | Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    if !batch.is_empty() {
                        if let Err(e) = commit_batch(&conn, &batch) {
                            warn!("store commit failed: {e}");
                        }
                        batch.clear();
                    }
                    if !orphan_batch.is_empty() {
                        if let Err(e) = commit_orphan_batch(&conn, &orphan_batch) {
                            warn!("store orphan-event commit failed: {e}");
                        }
                        orphan_batch.clear();
                    }
                    deadline = std::time::Instant::now() + batch_interval;
                }
                Ok(StoreMsg::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    if !batch.is_empty() {
                        if let Err(e) = commit_batch(&conn, &batch) {
                            warn!("store final commit failed: {e}");
                        }
                    }
                    if !orphan_batch.is_empty() {
                        if let Err(e) = commit_orphan_batch(&conn, &orphan_batch) {
                            warn!("store final orphan-event commit failed: {e}");
                        }
                    }
                    info!("store thread exiting");
                    break;
                }
            }
        }
    });

    Ok((tokio_tx, handle))
}

fn commit_batch(conn: &Connection, batch: &[heaplens_protocol::NodeDto]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for dto in batch {
        // Serialize state using serde so the DB value matches the WS wire format
        // (e.g. "healthy" not "Healthy").
        let state_str = serde_json::to_value(&dto.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", dto.state));
        tx.execute(
            "INSERT INTO nodes (id, ptr, size, ts, symbol, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                dto.id as i64,
                dto.ptr as i64,
                dto.size as i64,
                dto.ts as i64,
                &dto.symbol,
                state_str
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn commit_orphan_batch(conn: &Connection, batch: &[crate::msg::OrphanEventRecord]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for rec in batch {
        tx.execute(
            "INSERT INTO orphan_events (node_id, owner_free_ts_ns, orphan_detected_ts_ns, tau_ms) \
             VALUES (?1, ?2, ?3, ?4)",
            params![
                rec.node_id as i64,
                rec.owner_free_ts_ns as i64,
                rec.orphan_detected_ts_ns as i64,
                rec.tau_ms as i64,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
