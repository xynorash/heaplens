use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;
use futures_util::StreamExt;
use heaplens_protocol::{AllocEvent, EventKind, GraphMessage};
use heaplens_daemon::{
    graph::OwnershipGraph,
    msg::{ConnectRequest, GraphMsg},
    resolver::Resolver,
    server,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_alloc_ev(ptr: u64, ts: u64, stack0: u64) -> AllocEvent {
    let mut stack = [0u64; 16];
    stack[0] = stack0;
    AllocEvent::new(EventKind::Alloc, ptr, 0, 64, 8, ts, stack, 1)
}

fn is_non_empty_diff(msg: &GraphMessage) -> bool {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => {
            !add.is_empty() || !update.is_empty() || !remove.is_empty()
        }
        GraphMessage::Snapshot { .. } | GraphMessage::Stats { .. } => false,
    }
}

/// Mini graph loop: handles GraphMsg + ConnectRequest without a store.
/// Tests drive this by sending events + ticks through graph_tx.
async fn run_graph_loop(
    mut graph_rx: mpsc::UnboundedReceiver<GraphMsg>,
    mut connect_rx: mpsc::UnboundedReceiver<ConnectRequest>,
    broadcast_tx: broadcast::Sender<Arc<GraphMessage>>,
) {
    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();
    loop {
        tokio::select! {
            msg = graph_rx.recv() => match msg {
                Some(GraphMsg::Events(events)) => {
                    for ev in &events {
                        match ev.kind {
                            0 => graph.on_alloc(ev, &resolver),
                            1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                            2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                            _ => {}
                        }
                    }
                }
                Some(GraphMsg::Symbols(_)) => {}
                Some(GraphMsg::Handshake { .. }) => {}
                Some(GraphMsg::Tick) => {
                    let diff = graph.drain_diff(&resolver);
                    if is_non_empty_diff(&diff) {
                        let _ = broadcast_tx.send(Arc::new(diff));
                    }
                }
                None => break,
            },
            req = connect_rx.recv() => {
                if let Some(ConnectRequest { reply }) = req {
                    // Subscribe before snapshot so no diffs are lost.
                    let diff_rx = broadcast_tx.subscribe();
                    let snapshot = graph.snapshot(&resolver);
                    let _ = reply.send((snapshot, diff_rx));
                }
            }
        }
    }
}

/// Spin up a WS server + mini graph loop in-process.
/// Returns the WS URL, a sender for injecting events/ticks, and the graph-loop handle.
async fn start_test_server() -> (
    String,
    mpsc::UnboundedSender<GraphMsg>,
    tokio::task::JoinHandle<()>,
) {
    // Bind on port 0 to get an OS-assigned free port, then release it.
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let addr = format!("127.0.0.1:{port}");

    let (graph_tx, graph_rx) = mpsc::unbounded_channel::<GraphMsg>();
    let (connect_tx, connect_rx) = mpsc::unbounded_channel::<ConnectRequest>();
    let (broadcast_tx, _) = broadcast::channel::<Arc<GraphMessage>>(64);

    tokio::spawn(server::run(addr.clone(), connect_tx));

    let handle = tokio::spawn(run_graph_loop(graph_rx, connect_rx, broadcast_tx));

    // Give the server a moment to bind.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (format!("ws://{addr}"), graph_tx, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A freshly-connected client must receive a JSON message with `"type": "snapshot"`.
#[tokio::test]
async fn ws_snapshot_has_type_snapshot() {
    let (url, _graph_tx, _handle) = start_test_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    if let Message::Text(json) = msg {
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "snapshot", "first message must be a snapshot");
    } else {
        panic!("expected a Text WebSocket message, got: {msg:?}");
    }
}

/// After receiving the initial snapshot, injecting an alloc + tick must produce
/// a diff message with `"type": "diff"` and exactly one entry in `"add"`.
#[tokio::test]
async fn ws_diff_after_alloc() {
    let (url, graph_tx, _handle) = start_test_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    // Consume the initial snapshot.
    let _ = ws.next().await.unwrap().unwrap();

    // Inject one alloc, then tick.
    graph_tx.send(GraphMsg::Events(vec![make_alloc_ev(0x1000, 100, 0xAAAA)])).unwrap();
    graph_tx.send(GraphMsg::Tick).unwrap();

    // Allow time for the graph loop + WS forward to complete.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let msg = ws.next().await.unwrap().unwrap();
    if let Message::Text(json) = msg {
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "diff", "second message must be a diff");
        let add = parsed["add"].as_array().expect("diff must have an 'add' array");
        assert_eq!(add.len(), 1, "exactly one node should be in 'add'");
    } else {
        panic!("expected a Text WebSocket message, got: {msg:?}");
    }
}

/// Nodes that were alive before a client connected must appear in the snapshot.
/// Deallocating one and ticking must produce a diff whose `"remove"` list contains
/// an id that was present in the original snapshot.
#[tokio::test]
async fn ws_snapshot_then_diff_consistent() {
    let (url, graph_tx, _handle) = start_test_server().await;

    // Pre-allocate 3 nodes before any client connects.
    for i in 0u64..3 {
        graph_tx.send(GraphMsg::Events(vec![
            make_alloc_ev(0x1000 + i * 0x100, 100 + i, 0xAAAA + i),
        ])).unwrap();
    }
    graph_tx.send(GraphMsg::Tick).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Connect — snapshot must carry all 3 nodes.
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let snap_msg = ws.next().await.unwrap().unwrap();

    let snapshot_ids: Vec<u64> = if let Message::Text(json) = snap_msg {
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "snapshot");
        parsed["nodes"]
            .as_array()
            .expect("snapshot must have a 'nodes' array")
            .iter()
            .map(|n| n["id"].as_u64().expect("node id must be a u64"))
            .collect()
    } else {
        panic!("expected snapshot Text message");
    };

    assert_eq!(snapshot_ids.len(), 3, "snapshot must contain all 3 pre-allocated nodes");

    // Dealloc the first node (ptr 0x1000) and tick.
    let dealloc = AllocEvent::new(EventKind::Dealloc, 0x1000, 0, 0, 0, 200, [0u64; 16], 0);
    graph_tx.send(GraphMsg::Events(vec![dealloc])).unwrap();
    graph_tx.send(GraphMsg::Tick).unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let diff_msg = ws.next().await.unwrap().unwrap();
    if let Message::Text(json) = diff_msg {
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "diff");

        let removed: Vec<u64> = parsed["remove"]
            .as_array()
            .expect("diff must have a 'remove' array")
            .iter()
            .map(|v| v.as_u64().expect("remove entry must be a u64"))
            .collect();

        assert!(!removed.is_empty(), "diff must remove at least one node");
        assert!(
            removed.iter().all(|rid| snapshot_ids.contains(rid)),
            "every removed id must have been in the snapshot"
        );
    } else {
        panic!("expected diff Text message");
    }
}

/// Two clients that connect independently must each receive a snapshot message.
#[tokio::test]
async fn ws_two_clients_both_get_snapshot() {
    let (url, _graph_tx, _handle) = start_test_server().await;

    let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let msg1 = ws1.next().await.unwrap().unwrap();
    let msg2 = ws2.next().await.unwrap().unwrap();

    for (i, msg) in [msg1, msg2].into_iter().enumerate() {
        if let Message::Text(json) = msg {
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed["type"], "snapshot", "client {i} must receive a snapshot");
        } else {
            panic!("client {i}: expected a Text WebSocket message");
        }
    }
}
