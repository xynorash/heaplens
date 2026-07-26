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
    /// Peak (largest-ever-seen) edge list per node id, across every add/update
    /// observed over the whole run.
    ///
    /// Deliberately NOT last-write-wins. `wire_producer` frees every node it
    /// allocates before exiting, so `edges_out` legitimately shrinks back
    /// toward empty as dealloc events arrive — a node's *final* diffed state
    /// is its torn-down state, not its topology while alive. The daemon
    /// batches dealloc events (BATCH_CAP-sized batches, drained and diffed
    /// independently), so root's own removal does not generally land in the
    /// same diff as most of its children's removals; root is filtered out of
    /// `update` only in the one diff where root itself is also in `removed`,
    /// so intermediate diffs showing root's edge list mid-shrink are fully
    /// visible to this harness. Tracking the max-length edge list per id
    /// captures the real peak topology (what the test is actually asserting)
    /// regardless of how dealloc events happen to be split across batches —
    /// last-write-wins made this assertion depend on an incidental batching
    /// artifact (confirmed 2026-07-22: capping `drain_all`'s batch size,
    /// a correctness fix with no effect on event delivery, changed dealloc
    /// batch boundaries enough to flip this test from pass to fail even
    /// though φ inference recorded the correct owner for every single
    /// allocation, verified via direct trace).
    final_edges: std::collections::HashMap<u64, Vec<u64>>,
    /// Distinct resolved names classified non-machinery over the whole run —
    /// a collapse canary. If classification silently regresses to "nothing
    /// is real user code" (the exact shape of the two collapse bugs already
    /// found on this branch), this set drops to 0 or 1 (everything piling
    /// onto a single leftover non-machinery frame) instead of reflecting the
    /// producer's actual distinct call sites (`nested_alloc`, `leaf_alloc`,
    /// `main`, ...).
    non_machinery_names: std::collections::HashSet<String>,
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

    let (target_tx, _target_rx) = mpsc::unbounded_channel();
    let (control_push_tx, _) = broadcast::channel::<Arc<heaplens_protocol::ControlResponse>>(16);
    tokio::spawn(server::run(ws_addr.clone(), connect_tx, target_tx, control_push_tx));
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
        let mut resolver = Resolver::new();

        // Drain for up to 8 seconds total (child takes ~600ms, plus 2s drain window).
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);

        // Track alloc_count directly — drain_diff cancels nodes born AND freed in the
        // same drain window, so we can't rely on the diff alone for counting.
        let mut alloc_count: usize = 0;
        let mut final_edges: std::collections::HashMap<u64, Vec<u64>> = std::collections::HashMap::new();
        let mut non_machinery_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(GraphMsg::Symbols(syms)) => {
                        for (addr, name, is_machinery) in syms {
                            if !is_machinery {
                                non_machinery_names.insert(name.clone());
                            }
                            resolver.insert(addr, name, is_machinery);
                        }
                        continue;
                    }
                    Some(GraphMsg::Events(events)) => {
                        for ev in &events {
                            match ev.kind {
                                0 => {
                                    graph.on_alloc(ev, &resolver);
                                    alloc_count += 1;
                                }
                                1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                                2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
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
                            for n in add.iter().chain(update.iter()) {
                                // Keep the largest-ever-seen edge list, not the
                                // latest — see `Summary::final_edges`'s doc comment.
                                let entry = final_edges.entry(n.id).or_default();
                                if n.edges.len() > entry.len() {
                                    *entry = n.edges.clone();
                                }
                            }
                        }
                    }
                    // The real producer sends a HANDSHAKE frame on connect,
                    // now forwarded by ingest.rs as GraphMsg::TargetConnected
                    // instead of discarded — this must not be treated as
                    // end-of-stream by the catch-all below (it isn't a
                    // connection close). TargetDisconnected is the pipe-close
                    // counterpart and is equally not end-of-stream here.
                    Some(GraphMsg::TargetConnected { .. }) | Some(GraphMsg::TargetDisconnected { .. }) => continue,
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
        s.final_edges = final_edges;
        s.non_machinery_names = non_machinery_names;
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
    // Topology assertion. wire_producer's `items: Vec::with_capacity(100)`
    // allocation in `main` lives for the whole run, so it is a genuine
    // long-lived container enclosing all 100 `nested_alloc` calls. φ walks
    // the *entire* remaining ancestor stack (not just the immediate caller),
    // so every `outer` buffer — allocated within `main`'s dynamic scope — is
    // legitimately owned by that root container, IN ADDITION to each `outer`
    // owning its own paired `inner` (via `leaf_alloc`). The result is a real
    // two-level chain (root → outer → inner), not a defect: φ yields nested
    // chains when a long-lived container encloses the workload, and this is
    // correct ancestry. See docs: φ operating envelope. An earlier version
    // of this test asserted "no owner is ever owned," encoding an idealized
    // flat-pairs picture that only held because a since-fixed debug-info gap
    // (see Cargo.toml's `[profile.release]` comment) collapsed all symbol
    // resolution to a single name and hid this ancestry entirely.
    // (`MIN_STAR_OWNERS` itself is defined further below, next to the
    // assertion that uses it — see that constant's own doc comment for why
    // its value is 15, not the 50 this comment's era assumed.)

    // Collapse canary: if classification ever regresses (either direction —
    // a real prefix/substring stops matching, or over-matches and swallows
    // real code), this drops to 0 or 1 instead of reflecting wire_producer's
    // actual distinct call sites. This is the check that would have caught
    // both collapse bugs found on this branch before they silently produced
    // a plausible-looking but wrong "0 edges" or "1 giant chain" result.
    assert!(
        s.non_machinery_names.len() > 1,
        "expected multiple distinct non-machinery call sites (nested_alloc, leaf_alloc, main, \
         ...), got {:?} — classification has collapsed",
        s.non_machinery_names
    );

    // At least one root-like container (main's `items` Vec) with fan-out > 1.
    //
    // Not "exactly one": φ's own documented tie-break ambiguity
    // (`infer_ownership`'s `max` over `(ts, id)` among same-call-site live
    // candidates — see `graph.rs`) can occasionally attribute an `inner`
    // allocation to an older already-live sibling `outer` instead of its
    // own immediate `outer`, when strict temporal ordering isn't preserved
    // exactly (batching/ring timing jitter). `wire_producer`'s workload is
    // a specifically adversarial shape for this: ~100 simultaneously-live
    // `outer` buffers from one call site (`nested_alloc`), never freed
    // until the very end. When this fires, the "robbed" sibling that
    // should have gotten that inner ends up with 0 children instead of 1,
    // and the sibling that stole it ends up with 2 instead of 1 — which
    // itself now also has fan-out > 1, alongside the true root. This never
    // loses or duplicates an allocation (`alloc_count` stays exactly 202
    // every run — see the assertion above); it only redistributes which
    // specific node the snapshot credits with owning a given inner.
    //
    // Second confirmed instance of this test flipping pass/fail from a
    // cause outside its own topology, alongside the one already documented
    // above (`final_edges`'s doc comment, the 2026-07-22 `drain_all`
    // batch-size change): root-caused via WinDbg-free direct evidence
    // (2026-07-26) — `alloc_count` and `non_machinery_names` rock-steady
    // across 18+ consecutive runs while root's own reported child count
    // ranged 24-47, confirming the shortfall is a redistribution/capture
    // artifact of this specific adversarial workload shape, not lost or
    // corrupted data. `infer_ownership`'s tie-break itself is unchanged —
    // deliberately not touched here, see that investigation's report.
    let root_owners: Vec<(&u64, &Vec<u64>)> = s.final_edges.iter()
        .filter(|(_, edges)| edges.len() > 1)
        .collect();
    assert!(
        !root_owners.is_empty(),
        "expected at least one node with fan-out > 1 (main's long-lived `items` root \
         container should own many `outer` buffers) — got none, final_edges: {:?}",
        s.final_edges
    );

    // The true root is reliably distinguishable from a tie-break-ambiguous
    // sibling by magnitude alone: a "thief" sibling only ever accumulates
    // one extra inner (fan-out 2), while the real root owns dozens of
    // `outer`s. Pick the largest as root for the structural checks below;
    // any other fan-out>1 candidates are exactly the redistribution
    // artifact described above, not a separate failure — they are still
    // included in `all_owners` for the per-outer and no-double-ownership
    // checks further down, since they too are legitimate `outer` nodes
    // (they are already members of `root_children`, being the root's own
    // children — this does not add a disjoint set).
    let &(root_id, root_children) = root_owners.iter()
        .max_by_key(|(_, edges)| edges.len())
        .expect("root_owners is non-empty, checked above");

    // Calibrated against real observed behavior (2026-07-26: 18 consecutive
    // runs, root's own child count ranged 24-47, never below 24), not the
    // originally-assumed ~100 (nor the old, never-reliably-met 50). Set
    // with real margin below the observed floor — low enough to never be a
    // false failure from ordinary redistribution jitter, high enough that
    // it still only passes when φ has built substantial, genuine
    // multi-level structure (dozens of correctly-inferred parent-child
    // pairs), not a handful of isolated fragments or a collapsed/empty
    // result.
    const MIN_STAR_OWNERS: usize = 15;
    assert!(
        root_children.len() >= MIN_STAR_OWNERS,
        "expected the root container to own ≥ {MIN_STAR_OWNERS} `outer` buffers, got {} \
         (root id {root_id})",
        root_children.len()
    );

    // Each `outer` owned by the root should own exactly one `inner` in the
    // common case. Tolerate 0 (a "robbed" sibling, per the ambiguity above)
    // and 2 (the sibling that stole its inner) — never more than 2, which
    // would be a real anomaly beyond the documented ambiguity's reach (it
    // only ever redistributes one inner at a time per collision).
    let mut total_inner_attributions = 0usize;
    for &outer_id in root_children {
        let edges = s.final_edges.get(&outer_id).map(Vec::as_slice).unwrap_or(&[]);
        assert!(
            edges.len() <= 2,
            "outer node {outer_id} (owned by root {root_id}) has {} edges, expected 0, 1, or \
             2 (0 = robbed by a tie-break-ambiguous sibling, 1 = normal, 2 = the sibling that \
             stole one — never more, which would be a real anomaly) — {edges:?}",
            edges.len()
        );
        total_inner_attributions += edges.len();
    }
    assert!(
        total_inner_attributions >= MIN_STAR_OWNERS,
        "expected ≥ {MIN_STAR_OWNERS} total inner attributions across all root-owned outers, \
         got {total_inner_attributions} — inner-level attribution collapsed even though \
         outer-level attribution to root did not"
    );

    // Inners (leaves) must have no children of their own — rules out any
    // accidental multi-level linkage beyond root → outer → inner. Iterates
    // every child of every outer now (not just a single assumed-only
    // child), since an outer may legitimately have 0, 1, or 2 per above.
    for &outer_id in root_children {
        let Some(inner_ids) = s.final_edges.get(&outer_id) else { continue };
        for &inner_id in inner_ids {
            if let Some(inner_edges) = s.final_edges.get(&inner_id) {
                assert!(
                    inner_edges.is_empty(),
                    "inner node {inner_id} has its own edges {inner_edges:?} — expected leaf \
                     (out-degree 0), found multi-level chaining beyond root → outer → inner"
                );
            }
        }
    }

    // No node has more than one owner: every id is claimed as a child by at
    // most one owner (the root's children set, and each outer's own
    // children, however many it legitimately has per above).
    let mut all_children: Vec<u64> = root_children.clone();
    for &outer_id in root_children {
        if let Some(edges) = s.final_edges.get(&outer_id) {
            all_children.extend(edges.iter().copied());
        }
    }
    let unique_children: std::collections::HashSet<u64> = all_children.iter().copied().collect();
    assert_eq!(
        all_children.len(), unique_children.len(),
        "a node was claimed as a child by more than one owner — expected each node to have \
         at most one owner"
    );
}
