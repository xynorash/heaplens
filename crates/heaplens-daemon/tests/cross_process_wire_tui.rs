#![cfg(windows)]
//! Validates that `checkout_service_tui` (the iced GUI demo target) produces
//! the same phi ownership shapes as `checkout_service.rs` (the console
//! version) despite running its allocation sites from inside iced's own
//! Elm-style update loop instead of a plain `loop { }` in `main`. Modeled on
//! `cross_process_wire.rs`'s ingest/graph harness, adapted for a target that
//! never exits on its own (a GUI) and has two independent owner stars
//! (leak's `pool_manager`, hot cluster's `queue_owner`) rather than one.
//!
//! Drives the scripted window via `HEAPLENS_GUI_AUTOSTART=1` — a
//! validation-only hook in the GUI binary that starts the deterministic
//! T+15s/30s/33s schedule without a real mouse click.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::ingest;
use heaplens_daemon::msg::GraphMsg;
use heaplens_daemon::resolver::Resolver;

const PIPE_NAME: &str = r"\\.\pipe\heaplens";

#[derive(Default, Debug)]
struct Summary {
    alloc_count: usize,
    // Peak edge list per node id — see cross_process_wire.rs's Summary doc
    // comment for why max-ever-seen, not last-write-wins.
    final_edges: std::collections::HashMap<u64, Vec<u64>>,
    non_machinery_names: std::collections::HashSet<String>,
}

fn gui_target_path() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    let target_debug = test_exe.parent().and_then(|p| p.parent()).expect("cannot determine target dir");
    target_debug.join("examples").join("checkout_service_tui.exe")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkout_service_tui_matches_console_topology() {
    let target = gui_target_path();
    if !target.exists() {
        panic!(
            "checkout_service_tui.exe not found at {:?}.\nBuild it first: cargo build --example checkout_service_tui -p heaplens-alloc",
            target
        );
    }

    let summary = Arc::new(Mutex::new(Summary::default()));
    let summary_for_graph = summary.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();
    let ingest_tx = tx.clone();

    let ingest_handle = tokio::spawn(ingest::run(PIPE_NAME.to_owned(), ingest_tx));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut child = std::process::Command::new(&target)
        .env("HEAPLENS_GUI_AUTOSTART", "1")
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn checkout_service_tui: {e}"));

    let graph_handle = tokio::spawn(async move {
        let mut graph = OwnershipGraph::new();
        let mut resolver = Resolver::new();
        // Cover leak (T+15/17s) and hot-cluster grow (T+30/33s) with margin
        // before hot-drain (T+40s) clears the backlog. Wider than the GUI
        // target's window: rendering to an inherited console under the test
        // harness is slower than a native terminal, so real-wall-clock
        // schedule firing can lag noticeably behind the iced target's pace.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(38);

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
                                0 => { graph.on_alloc(ev, &resolver); alloc_count += 1; }
                                1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                                2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                                _ => {}
                            }
                        }
                        let diff = graph.drain_diff(&resolver);
                        if let heaplens_protocol::GraphMessage::Diff { add, update, .. } = diff {
                            for n in add.iter().chain(update.iter()) {
                                let entry = final_edges.entry(n.id).or_default();
                                if n.edges.len() > entry.len() {
                                    *entry = n.edges.clone();
                                }
                            }
                        }
                    }
                    Some(GraphMsg::TargetConnected { .. }) | Some(GraphMsg::TargetDisconnected { .. }) => continue,
                    _ => break,
                },
                _ = tokio::time::sleep(remaining) => break,
            }
        }

        let mut s = summary_for_graph.lock().unwrap();
        s.alloc_count = alloc_count;
        s.final_edges = final_edges;
        s.non_machinery_names = non_machinery_names;
    });

    match tokio::time::timeout(Duration::from_secs(65), graph_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("graph task panicked: {e}"),
        Err(_) => panic!("graph task did not finish within timeout"),
    }

    // GUI never exits on its own — kill it now that sampling is done.
    let _ = child.kill();
    let _ = child.wait();
    ingest_handle.abort();

    let s = summary.lock().unwrap();
    eprintln!("GUI wire test summary: {s:?}");

    assert!(s.alloc_count > 0, "no alloc events observed at all — wire/framing failure");
    assert!(
        s.non_machinery_names.len() > 1,
        "expected multiple distinct non-machinery call sites, got {:?}",
        s.non_machinery_names
    );

    // Two independent owner stars are expected: leak's pool_manager (fan-out
    // ~12, allocated in leak_phase_tick) and hot cluster's queue_owner
    // (fan-out ~40 at peak before drain, allocated in hot_phase_tick) — not
    // one combined root like wire_producer's single long-lived container,
    // because these two owners come from two distinct named functions.
    let mut fan_outs: Vec<usize> = s.final_edges.values().map(|e| e.len()).filter(|&n| n > 1).collect();
    fan_outs.sort_unstable();
    eprintln!("Observed fan-outs > 1: {fan_outs:?}");

    let has_leak_star = fan_outs.iter().any(|&n| (8..=14).contains(&n));
    let has_hot_star = fan_outs.iter().any(|&n| n >= 25);

    assert!(
        has_leak_star,
        "expected an owner with fan-out ~12 (leak's pool_manager -> connections), got fan-outs {fan_outs:?}"
    );
    assert!(
        has_hot_star,
        "expected an owner with fan-out >= 25 (hot cluster's queue_owner -> orders, peak 40), got fan-outs {fan_outs:?}"
    );
}
