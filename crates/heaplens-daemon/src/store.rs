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
pub fn open(path: &str) -> Result<tokio_mpsc::UnboundedSender<StoreMsg>> {
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
        CREATE INDEX IF NOT EXISTS idx_ts ON nodes(ts);",
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

    // Store thread: owns Connection, batches inserts every 100ms
    thread::spawn(move || {
        let mut batch: Vec<heaplens_protocol::NodeDto> = Vec::new();
        loop {
            match std_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(StoreMsg::Nodes(dtos)) => {
                    batch.extend(dtos);
                }
                Ok(StoreMsg::Flush) | Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    if !batch.is_empty() {
                        if let Err(e) = commit_batch(&conn, &batch) {
                            warn!("store commit failed: {e}");
                        }
                        batch.clear();
                    }
                }
                Ok(StoreMsg::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    if !batch.is_empty() {
                        if let Err(e) = commit_batch(&conn, &batch) {
                            warn!("store final commit failed: {e}");
                        }
                    }
                    info!("store thread exiting");
                    break;
                }
            }
        }
    });

    Ok(tokio_tx)
}

fn commit_batch(conn: &Connection, batch: &[heaplens_protocol::NodeDto]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for dto in batch {
        tx.execute(
            "INSERT INTO nodes (id, ptr, size, ts, symbol, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                dto.id as i64,
                dto.ptr as i64,
                dto.size as i64,
                dto.ts as i64,
                &dto.symbol,
                format!("{:?}", dto.state)
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
