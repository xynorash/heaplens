// Observability-only regression tests for the target-diagnostics counters
// added alongside `feat/target-diagnostics` (events_received,
// symbols_resolved/hex_fallback, and forwarded Handshake pid/name). These
// counters must never influence phi ownership inference or anomaly
// detection — see graph.rs's `symbol_stats` and main.rs's Tick-arm doc
// comments for the "read-only, decision already made" discipline this
// mirrors from the H1 orphan-timestamp work.
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

fn make_alloc_ev(ptr: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 16];
    let len = stack.len().min(16);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(EventKind::Alloc, ptr, 0, 64, 8, ts, s, len as u8)
}

fn make_dealloc_ev(ptr: u64, ts: u64) -> AllocEvent {
    AllocEvent::new(EventKind::Dealloc, ptr, 0, 0, 0, ts, [0u64; 16], 0)
}

fn is_non_empty_diff(msg: &GraphMessage) -> bool {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => {
            !add.is_empty() || !update.is_empty() || !remove.is_empty()
        }
        GraphMessage::Snapshot { .. } | GraphMessage::Stats { .. } => false,
    }
}

fn as_json(msg: Message) -> serde_json::Value {
    match msg {
        Message::Text(json) => serde_json::from_str(&json).expect("valid JSON"),
        other => panic!("expected a Text WebSocket message, got: {other:?}"),
    }
}

/// Mirrors main.rs's Tick-arm handling closely enough to exercise the new
/// counters end-to-end over a real WS connection: events counted before
/// diff-visibility filtering, `symbol_stats` computed fresh each tick, and a
/// Stats broadcast sent every tick (unlike main.rs's real 1s cadence — sent
/// every tick here so tests don't need to sleep a full second).
async fn run_graph_loop(
    mut graph_rx: mpsc::UnboundedReceiver<GraphMsg>,
    mut connect_rx: mpsc::UnboundedReceiver<ConnectRequest>,
    broadcast_tx: broadcast::Sender<Arc<GraphMessage>>,
) {
    let mut graph = OwnershipGraph::new();
    let mut resolver = Resolver::new();
    let mut events_received: u64 = 0;
    let mut target_pid: Option<u64> = None;
    let mut target_name: Option<String> = None;

    loop {
        tokio::select! {
            msg = graph_rx.recv() => match msg {
                Some(GraphMsg::Events(events)) => {
                    events_received += events.len() as u64;
                    for ev in &events {
                        match ev.kind {
                            0 => graph.on_alloc(ev, &resolver),
                            1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                            2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                            _ => {}
                        }
                    }
                }
                Some(GraphMsg::Symbols(syms)) => {
                    for (addr, name, is_machinery) in syms {
                        resolver.insert(addr, name, is_machinery);
                    }
                }
                Some(GraphMsg::Handshake { pid, name }) => {
                    target_pid = Some(pid);
                    target_name = Some(name);
                }
                Some(GraphMsg::Tick) => {
                    let diff = graph.drain_diff(&resolver);
                    if is_non_empty_diff(&diff) {
                        let _ = broadcast_tx.send(Arc::new(diff));
                    }

                    let (symbols_resolved, hex_fallback) = graph.symbol_stats(&resolver);
                    let stats = GraphMessage::Stats {
                        ts: graph.max_ts_seen,
                        events_received,
                        symbols_resolved,
                        hex_fallback,
                        target_pid,
                        target_name: target_name.clone(),
                    };
                    let _ = broadcast_tx.send(Arc::new(stats));
                }
                None => break,
            },
            req = connect_rx.recv() => {
                if let Some(ConnectRequest { reply }) = req {
                    let diff_rx = broadcast_tx.subscribe();
                    let snapshot = graph.snapshot(&resolver);
                    let _ = reply.send((snapshot, diff_rx));
                }
            }
        }
    }
}

async fn start_test_server() -> (
    String,
    mpsc::UnboundedSender<GraphMsg>,
    tokio::task::JoinHandle<()>,
) {
    let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);

    let addr = format!("127.0.0.1:{port}");

    let (graph_tx, graph_rx) = mpsc::unbounded_channel::<GraphMsg>();
    let (connect_tx, connect_rx) = mpsc::unbounded_channel::<ConnectRequest>();
    let (broadcast_tx, _) = broadcast::channel::<Arc<GraphMessage>>(64);

    tokio::spawn(server::run(addr.clone(), connect_tx));
    let handle = tokio::spawn(run_graph_loop(graph_rx, connect_rx, broadcast_tx));

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (format!("ws://{addr}"), graph_tx, handle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `graph.rs`'s `drain_diff` doc comment: "Nodes born and freed within the
/// same tick are invisible to the consumer." `events_received` must count
/// both raw events anyway — it is read directly off the incoming event
/// stream, before any diff-visibility filtering — otherwise a target that
/// churns allocations fast enough to always fall within one tick would
/// falsely look like "zero events" to the target-diagnostics banner.
#[tokio::test]
async fn events_received_counts_events_invisible_to_the_diff() {
    let (url, graph_tx, _handle) = start_test_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let _ = ws.next().await.unwrap().unwrap(); // initial snapshot

    graph_tx.send(GraphMsg::Events(vec![make_alloc_ev(0x9000, 100, &[0xAAAA])])).unwrap();
    graph_tx.send(GraphMsg::Events(vec![make_dealloc_ev(0x9000, 150)])).unwrap();
    graph_tx.send(GraphMsg::Tick).unwrap();

    // The diff is empty (born+freed same tick), so is_non_empty_diff
    // suppresses it — the very next message must be the Stats broadcast.
    let json = as_json(ws.next().await.unwrap().unwrap());
    assert_eq!(json["type"], "stats", "diff should be suppressed as empty; stats must still arrive");
    assert_eq!(json["events_received"], 2, "both the alloc and the dealloc must be counted");
}

/// `symbol_stats` must classify a resolved effective site as `resolved` and
/// an address the resolver never learned about as `hex_fallback`, and the
/// forwarded Handshake pid/name must appear on the Stats message.
#[tokio::test]
async fn stats_reports_symbol_resolution_and_handshake_identity() {
    let (url, graph_tx, _handle) = start_test_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let _ = ws.next().await.unwrap().unwrap(); // initial snapshot

    graph_tx
        .send(GraphMsg::Symbols(vec![(0xAAAA, "myapp::alloc_site".to_owned(), false)]))
        .unwrap();
    graph_tx
        .send(GraphMsg::Events(vec![
            make_alloc_ev(0x1000, 100, &[0xAAAA]), // resolves
            make_alloc_ev(0x2000, 200, &[0xBBBB]), // never resolved -> hex fallback
        ]))
        .unwrap();
    graph_tx
        .send(GraphMsg::Handshake { pid: 4242, name: "target.exe".to_owned() })
        .unwrap();
    graph_tx.send(GraphMsg::Tick).unwrap();

    // Two allocs => non-empty diff is sent too; find the stats message
    // among the (at most two) messages this tick produces.
    let mut found = false;
    for _ in 0..2 {
        let json = as_json(ws.next().await.unwrap().unwrap());
        if json["type"] == "stats" {
            assert_eq!(json["symbols_resolved"], 1);
            assert_eq!(json["hex_fallback"], 1);
            assert_eq!(json["target_pid"], 4242);
            assert_eq!(json["target_name"], "target.exe");
            found = true;
        }
    }
    assert!(found, "expected a stats message among this tick's broadcasts");
}

/// The new counting/broadcast code runs in the same Tick arm as
/// `drain_diff` — this pins down that phi's actual edge output is
/// unaffected: a child whose stack passes through the owner's effective
/// site must still gain an ownership edge, exactly as it would without any
/// of the new observability code present.
#[tokio::test]
async fn diff_content_unaffected_by_new_stats_broadcast() {
    let (url, graph_tx, _handle) = start_test_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let _ = ws.next().await.unwrap().unwrap(); // initial snapshot

    graph_tx
        .send(GraphMsg::Symbols(vec![
            (0xAAAA, "owner_site".to_owned(), false),
            (0xBBBB, "child_site".to_owned(), false),
        ]))
        .unwrap();
    graph_tx.send(GraphMsg::Events(vec![make_alloc_ev(0x1000, 100, &[0xAAAA])])).unwrap();
    graph_tx
        .send(GraphMsg::Events(vec![make_alloc_ev(0x2000, 200, &[0xBBBB, 0xAAAA])]))
        .unwrap();
    graph_tx.send(GraphMsg::Tick).unwrap();

    let json = as_json(ws.next().await.unwrap().unwrap());
    assert_eq!(json["type"], "diff", "diff must still be produced first, alongside the new stats broadcast");
    let add = json["add"].as_array().expect("diff must have an 'add' array");
    assert_eq!(add.len(), 2);
    let owner = add.iter().find(|n| n["ptr"] == 0x1000).expect("owner node");
    let child = add.iter().find(|n| n["ptr"] == 0x2000).expect("child node");
    let owner_edges = owner["edges"].as_array().unwrap();
    assert!(
        owner_edges.contains(&child["id"]),
        "phi ownership inference must be unaffected by the new observability counters — \
         owner_edges={owner_edges:?}, child_id={:?}", child["id"]
    );
}
