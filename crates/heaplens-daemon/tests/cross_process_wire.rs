#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::ingest;
use heaplens_daemon::msg::{ConnectRequest, GraphMsg};
use heaplens_daemon::resolver::Resolver;
use heaplens_daemon::server;

const PIPE_NAME: &str = r"\\.\pipe\heaplens";
/// Minimum allocs expected: 100 nested_alloc calls × 2 Vec allocs each = 200.
/// We assert ≥ 100 to tolerate any ring drops under pressure.
const MIN_ALLOC_NODES: usize = 100;

#[derive(Default, Debug)]
struct Summary {
    alloc_count: usize,
    nodes_with_edges: usize, // nodes whose edges_out is non-empty
}

fn producer_path() -> std::path::PathBuf {
    // Test binary is at target/debug/deps/<name>-<hash>.exe
    // Wire producer is at target/debug/examples/wire_producer.exe
    let test_exe = std::env::current_exe().unwrap();
    let target_debug = test_exe
        .parent() // deps/
        .and_then(|p| p.parent()) // debug/ (or release/)
        .expect("cannot determine target dir from test exe path");
    target_debug.join("examples").join("wire_producer.exe")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_process_wire_end_to_end() {
    let producer = producer_path();
    if !producer.exists() {
        panic!(
            "wire_producer.exe not found at {:?}.\n\
             Build it first: cargo build --example wire_producer -p heaplens-alloc",
            producer
        );
    }

    let summary = Arc::new(Mutex::new(Summary::default()));
    let summary_for_graph = summary.clone();

    // Channel: ingest task produces into it; graph loop consumes.
    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();
    let ingest_tx = tx.clone();

    // WS infrastructure — broadcast channel for diffs + connect-request channel.
    let (broadcast_tx, _) = broadcast::channel::<Arc<heaplens_protocol::GraphMessage>>(64);
    let (connect_tx, mut connect_rx) = mpsc::unbounded_channel::<ConnectRequest>();

    // Pick a free port for the WS server.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_port = ws_listener.local_addr().unwrap().port();
    drop(ws_listener);
    let ws_addr = format!("127.0.0.1:{ws_port}");

    tokio::spawn(server::run(ws_addr.clone(), connect_tx));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Start the pipe server BEFORE spawning the child.
    // ingest::run loops forever; we abort it after the child exits.
    let ingest_handle = tokio::spawn(ingest::run(PIPE_NAME.to_owned(), ingest_tx));

    // Give the OS time to make the pipe available.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Spawn the producer process.
    let mut child = std::process::Command::new(&producer)
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn wire_producer: {e}"));

    // Graph loop: runs in a background task, drains rx until closed or timeout.
    // Also handles WS ConnectRequests via select! so clients can get a snapshot.
    let broadcast_tx_clone = broadcast_tx.clone();
    let graph_handle = tokio::spawn(async move {
        let mut graph = OwnershipGraph::new();
        let resolver = Resolver::new();

        // Drain for up to 8 seconds total (child takes ~600ms, plus 2s drain window).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);

        // Track alloc_count directly — drain_diff cancels nodes born AND freed in the
        // same drain window, so we can't rely on the diff alone for counting.
        let mut alloc_count: usize = 0;
        let mut nodes_with_edges: usize = 0;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(GraphMsg::Events(events)) => {
                        for ev in &events {
                            match ev.kind {
                                0 => {
                                    graph.on_alloc(ev);
                                    alloc_count += 1;
                                }
                                1 => graph.on_dealloc(ev.ptr),
                                2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size),
                                _ => {}
                            }
                        }
                        // Drain diff after each batch so nodes allocated before deallocs
                        // appear in the diff accumulator's "add" set and edge relationships
                        // are captured before the nodes are freed.
                        let diff = graph.drain_diff(&resolver);
                        // Broadcast non-empty diffs so connected WS clients receive updates.
                        let is_non_empty = matches!(
                            &diff,
                            heaplens_protocol::GraphMessage::Diff { add, update, remove, .. }
                            if !add.is_empty() || !update.is_empty() || !remove.is_empty()
                        );
                        if is_non_empty {
                            let _ = broadcast_tx_clone.send(Arc::new(diff.clone()));
                        }
                        if let heaplens_protocol::GraphMessage::Diff { add, update, .. } = diff {
                            nodes_with_edges += add
                                .iter()
                                .chain(update.iter())
                                .filter(|n| !n.edges.is_empty())
                                .count();
                        }
                    }
                    _ => break,
                },
                req = connect_rx.recv() => {
                    if let Some(ConnectRequest { reply }) = req {
                        // Subscribe to future diffs before snapshotting so no diffs are lost.
                        let diff_rx = broadcast_tx_clone.subscribe();
                        let snapshot = graph.snapshot(&resolver);
                        let _ = reply.send((snapshot, diff_rx));
                    }
                },
                _ = tokio::time::sleep(remaining) => break,
            }
        }

        // Write summary before the task returns.
        let mut s = summary_for_graph.lock().unwrap();
        s.alloc_count = alloc_count;
        s.nodes_with_edges = nodes_with_edges;
    });

    // Wait for child to exit (with 10s timeout via polling).
    let exit_result =
        tokio::task::spawn_blocking(move || -> Result<Option<std::process::ExitStatus>, std::io::Error> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match child.try_wait()? {
                    Some(status) => return Ok(Some(status)),
                    None => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            return Ok(None);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        })
        .await
        .expect("spawn_blocking panicked");

    match exit_result {
        Ok(Some(status)) => {
            assert!(status.success(), "wire_producer exited with non-zero status: {status}")
        }
        Ok(None) => panic!("wire_producer did not exit within 10s"),
        Err(e) => panic!("wait error: {e}"),
    }

    // Give the daemon a 2-second drain window after child exits.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // WS snapshot assertion — performed while graph loop is still running so it
    // can handle the ConnectRequest and return a snapshot.
    // Note: by this point, wire_producer has freed all its allocations, so the
    // snapshot's node list will be empty (all live=false). We assert structure only.
    {
        let ws_url = format!("ws://{ws_addr}");
        let (mut ws_stream, _) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .expect("failed to connect to WS server");

        let first_msg = tokio::time::timeout(Duration::from_secs(5), ws_stream.next())
            .await
            .expect("WS receive timed out")
            .expect("WS stream ended")
            .expect("WS message error");

        if let Message::Text(json) = first_msg {
            let parsed: serde_json::Value =
                serde_json::from_str(&json).expect("WS snapshot is not valid JSON");
            assert_eq!(
                parsed["type"], "snapshot",
                "WS first message must be a snapshot; got: {parsed}"
            );
        } else {
            panic!("expected Text WS message for snapshot, got: {first_msg:?}");
        }
    }

    // Stop the ingest task (releases its tx clone) and close the test's tx.
    ingest_handle.abort();
    drop(tx); // close last sender → graph loop's rx returns None → graph loop exits

    // Await graph loop with timeout.
    match tokio::time::timeout(Duration::from_secs(3), graph_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.is_cancelled() => {} // abort is OK
        Ok(Err(e)) => panic!("graph task panicked: {e}"),
        Err(_) => panic!("graph task did not finish within timeout after abort"),
    }

    // Assert.
    let s = summary.lock().unwrap();
    eprintln!("Wire test summary: {s:?}");

    assert!(
        s.alloc_count >= MIN_ALLOC_NODES,
        "expected ≥ {MIN_ALLOC_NODES} alloc nodes, got {} — possible wire/framing failure",
        s.alloc_count
    );
    assert!(
        s.nodes_with_edges >= 1,
        "expected ≥ 1 node with ownership edges (φ inference on real stacks), got 0 — \
         stacks may not have crossed the wire intact, or inline(never) was optimised away"
    );
}
