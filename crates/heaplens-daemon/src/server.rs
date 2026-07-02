use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use futures_util::SinkExt;
use tracing::{info, warn};
use heaplens_protocol::GraphMessage;
use crate::msg::ConnectRequest;

/// Run the WebSocket server until the process exits.
///
/// `addr`       — bind address, e.g. `"127.0.0.1:9001"`.
/// `connect_tx` — channel to request a snapshot + diff subscription from the
///                graph task for each new client.
pub async fn run(addr: String, connect_tx: mpsc::UnboundedSender<ConnectRequest>) {
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("WS server bind failed on {addr}: {e}");
            return;
        }
    };
    let bound = listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| addr.clone());
    info!("WebSocket server listening on {bound}");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!("WS client connecting from {peer}");
                let tx = connect_tx.clone();
                tokio::spawn(async move {
                    handle_client(stream, peer, tx).await;
                });
            }
            Err(e) => {
                warn!("WS accept error: {e}");
            }
        }
    }
}

async fn handle_client(
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    connect_tx: mpsc::UnboundedSender<ConnectRequest>,
) {
    let ws_stream = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            warn!("WS handshake failed for {peer}: {e}");
            return;
        }
    };

    // Request snapshot + diff subscription from the graph task atomically.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if connect_tx.send(ConnectRequest { reply: reply_tx }).is_err() {
        warn!("graph task gone, dropping WS client {peer}");
        return;
    }
    let (snapshot, mut diff_rx): (GraphMessage, broadcast::Receiver<Arc<GraphMessage>>) =
        match reply_rx.await {
            Ok(pair) => pair,
            Err(_) => {
                warn!("graph task dropped reply for {peer}");
                return;
            }
        };

    let (mut ws_sink, _ws_source) = futures_util::StreamExt::split(ws_stream);

    // Send the initial full snapshot.
    match serde_json::to_string(&snapshot) {
        Ok(json) => {
            if ws_sink.send(Message::Text(json)).await.is_err() {
                return;
            }
        }
        Err(e) => {
            warn!("snapshot serialize failed for {peer}: {e}");
            return;
        }
    }

    // Forward diffs as they arrive.
    loop {
        match diff_rx.recv().await {
            Ok(diff) => match serde_json::to_string(diff.as_ref()) {
                Ok(json) => {
                    if ws_sink.send(Message::Text(json)).await.is_err() {
                        info!("WS client {peer} disconnected");
                        break;
                    }
                }
                Err(e) => warn!("diff serialize failed for {peer}: {e}"),
            },
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("WS client {peer} lagged by {n} diffs — some dropped");
                // Continue: keep the connection alive, just note the gap.
            }
            Err(broadcast::error::RecvError::Closed) => {
                info!("broadcast channel closed, dropping WS client {peer}");
                break;
            }
        }
    }
}
