use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};
use tracing::{info, warn};
use heaplens_protocol::{ControlRequest, ControlResponse, GraphMessage};

use crate::msg::{ConnectRequest, TargetCmd};

/// Run the WebSocket server until the process exits.
///
/// `addr`       — bind address, e.g. `"127.0.0.1:9001"`.
/// `connect_tx` — channel to request a snapshot + diff subscription from the
///                graph task for each new client.
/// `target_tx`  — channel to forward inbound control requests (§3, Stage 7
///                Step 3) to the graph task, which owns both graph state and
///                "which pid is currently attached" session state.
pub async fn run(
    addr: String,
    connect_tx: mpsc::UnboundedSender<ConnectRequest>,
    target_tx: mpsc::UnboundedSender<TargetCmd>,
    control_push_tx: broadcast::Sender<Arc<ControlResponse>>,
) {
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
                let ttx = target_tx.clone();
                let push_rx = control_push_tx.subscribe();
                tokio::spawn(async move {
                    handle_client(stream, peer, tx, ttx, push_rx).await;
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
    target_tx: mpsc::UnboundedSender<TargetCmd>,
    mut control_push_rx: broadcast::Receiver<Arc<ControlResponse>>,
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

    let (mut ws_sink, mut ws_source) = ws_stream.split();

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

    // Forward diffs as they arrive, and handle inbound control requests
    // (Stage 7 §3) on the same connection.
    loop {
        tokio::select! {
            diff = diff_rx.recv() => {
                match diff {
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
            msg = ws_source.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let Ok(req) = serde_json::from_str::<ControlRequest>(&text) else {
                            warn!("WS client {peer} sent unrecognized control message: {text}");
                            continue;
                        };
                        let resp = handle_control_request(req, &target_tx).await;
                        match serde_json::to_string(&resp) {
                            Ok(json) => {
                                if ws_sink.send(Message::Text(json)).await.is_err() {
                                    info!("WS client {peer} disconnected");
                                    break;
                                }
                            }
                            Err(e) => warn!("control response serialize failed for {peer}: {e}"),
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WS client {peer} closed the connection");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("WS read error for {peer}: {e}");
                        break;
                    }
                    _ => {} // ping/pong/binary/frame — nothing to do
                }
            }
            push = control_push_rx.recv() => {
                match push {
                    Ok(resp) => match serde_json::to_string(resp.as_ref()) {
                        Ok(json) => {
                            if ws_sink.send(Message::Text(json)).await.is_err() {
                                info!("WS client {peer} disconnected");
                                break;
                            }
                        }
                        Err(e) => warn!("control push serialize failed for {peer}: {e}"),
                    },
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WS client {peer} lagged by {n} control pushes — some dropped");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("control push channel closed, dropping WS client {peer}");
                        break;
                    }
                }
            }
        }
    }
}

/// Forwards one control request to the graph task (the single owner of
/// both graph and attach-session state, §3.4) and waits for its reply.
/// `AttachTarget`/`DetachTarget` can take up to a few seconds — they spawn
/// and wait on `heaplens-injector.exe` — but that await only blocks this
/// one client's own task, not the WS server or other connected clients.
async fn handle_control_request(req: ControlRequest, target_tx: &mpsc::UnboundedSender<TargetCmd>) -> ControlResponse {
    match req {
        ControlRequest::ListProcesses => {
            let (reply, rx) = oneshot::channel();
            if target_tx.send(TargetCmd::ListProcesses { reply }).is_err() {
                return ControlResponse::ProcessList { processes: Vec::new() };
            }
            ControlResponse::ProcessList { processes: rx.await.unwrap_or_default() }
        }
        ControlRequest::AttachTarget { pid } => {
            let (reply, rx) = oneshot::channel();
            if target_tx.send(TargetCmd::Attach { pid, reply }).is_err() {
                return ControlResponse::AttachResult { ok: false, message: "daemon graph task is gone".to_owned() };
            }
            match rx.await {
                Ok(Ok(())) => ControlResponse::AttachResult { ok: true, message: format!("attached to pid {pid}") },
                Ok(Err(message)) => ControlResponse::AttachResult { ok: false, message },
                Err(_) => ControlResponse::AttachResult { ok: false, message: "daemon graph task dropped the reply".to_owned() },
            }
        }
        ControlRequest::DetachTarget => {
            let (reply, rx) = oneshot::channel();
            if target_tx.send(TargetCmd::Detach { reply }).is_err() {
                return ControlResponse::DetachResult { ok: false, message: "daemon graph task is gone".to_owned() };
            }
            match rx.await {
                Ok(Ok(())) => ControlResponse::DetachResult { ok: true, message: "detached".to_owned() },
                Ok(Err(message)) => ControlResponse::DetachResult { ok: false, message },
                Err(_) => ControlResponse::DetachResult { ok: false, message: "daemon graph task dropped the reply".to_owned() },
            }
        }
    }
}
