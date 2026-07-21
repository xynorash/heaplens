#![cfg(windows)]
//! Targeted regression test for the 2026-07-21 root-cause investigation.
//!
//! `hook_self_load_wire.rs` (the original Step 1 acceptance gate) never
//! actually exercised a spawned thread's allocations under the hook — its
//! entire scripted workload runs on the process's main thread, the same
//! thread that calls `LoadLibraryW`. That gap is exactly why the bug this
//! test guards against went uncaught until a dedicated concurrency stress
//! harness (`self_load_concurrency_stress.rs`) went looking for it:
//! `heaplens-hook` is always loaded via `LoadLibraryW` at runtime, and its
//! reentrancy guard (a `thread_local!`) and ring registry (also, until the
//! fix, a `thread_local!`) both crashed reliably on the very first hooked
//! allocation from any thread *other than* the one that loaded the DLL —
//! see `guard.rs`/`ring.rs`'s doc comments for the confirmed mechanism and
//! fix (raw `TlsAlloc` for the guard, `FlsAlloc` + explicit pre-unload
//! `shutdown` for the ring registry).
//!
//! This test runs `self_load_spawned_thread.rs` (which does its entire
//! scripted workload on a spawned thread, never main) against a real
//! daemon and asserts *exact* event counts by pointer identity — not just
//! "the process didn't crash," though that alone is most of what this test
//! exists to catch. A gate that only checked exit status would still be a
//! real regression test for this specific bug (it crashed the whole
//! process), but exact pointer-identity matching additionally confirms the
//! *content* wired through correctly for a spawned-thread producer, not
//! just that nothing crashed.

use std::io::Read;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::ingest;
use heaplens_daemon::msg::GraphMsg;
use heaplens_daemon::resolver::Resolver;

const PIPE_NAME: &str = r"\\.\pipe\heaplens";
// Must match crates/heaplens-hook/examples/self_load_spawned_thread.rs exactly.
const ALLOC_COUNT: usize = 20;

fn parse_hex_ptr(s: &str) -> u64 {
    let hex = s.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or_else(|e| panic!("bad pointer {s:?}: {e}"))
}

#[derive(Default, Debug)]
struct Expected {
    allocs: std::collections::HashMap<u64, u64>,
    frees: Vec<u64>,
}

fn parse_expected(stdout: &str) -> Expected {
    let mut e = Expected::default();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["ALLOC", ptr, size] => {
                let ptr = parse_hex_ptr(ptr.trim_start_matches("ptr="));
                let size: u64 = size.trim_start_matches("size=").parse().unwrap();
                e.allocs.insert(ptr, size);
            }
            ["FREE", ptr] => {
                e.frees.push(parse_hex_ptr(ptr.trim_start_matches("ptr=")));
            }
            _ => {}
        }
    }
    e
}

fn harness_path() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    let target_debug = test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path");
    target_debug.join("examples").join("self_load_spawned_thread.exe")
}

fn hook_dll_path() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    let target_debug = test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path");
    target_debug.join("heaplens_hook.dll")
}

#[derive(Default, Debug)]
struct Summary {
    matched_allocs: std::collections::HashSet<u64>,
    matched_frees: std::collections::HashSet<u64>,
    alloc_size_mismatches: Vec<(u64, u64, u64)>,
    total_events_seen: u64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_self_load_spawned_thread_end_to_end() {
    let harness = harness_path();
    if !harness.exists() {
        panic!(
            "self_load_spawned_thread.exe not found at {harness:?}.\n\
             Build it first: cargo build --example self_load_spawned_thread -p heaplens-hook"
        );
    }
    let dll = hook_dll_path();
    if !dll.exists() {
        panic!(
            "heaplens_hook.dll not found at {dll:?}.\n\
             Build it first: cargo build -p heaplens-hook"
        );
    }

    let summary = Arc::new(Mutex::new(Summary::default()));
    let summary_for_graph = summary.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();
    let ingest_tx = tx.clone();

    let ingest_handle = tokio::spawn(ingest::run(PIPE_NAME.to_owned(), ingest_tx));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut child = std::process::Command::new(&harness)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn self_load_spawned_thread: {e}"));
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    let raw_events: Arc<Mutex<Vec<heaplens_protocol::AllocEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let raw_events_for_graph = raw_events.clone();
    let graph_handle = tokio::spawn(async move {
        let mut resolver = Resolver::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(GraphMsg::Symbols(syms)) => {
                        for (addr, name, is_machinery) in syms {
                            resolver.insert(addr, name, is_machinery);
                        }
                    }
                    Some(GraphMsg::Events(events)) => {
                        raw_events_for_graph.lock().unwrap().extend(events);
                    }
                    Some(GraphMsg::TargetConnected { .. }) | Some(GraphMsg::TargetDisconnected { .. }) => {}
                    Some(GraphMsg::Tick) => {}
                    None => break,
                },
                _ = tokio::time::sleep(remaining) => break,
            }
        }
    });

    // Wait for the harness to exit. This is the core regression assertion:
    // before the fix, this process segfaulted (non-success exit) reliably
    // on the very first allocation from the spawned thread.
    let exit_result =
        tokio::task::spawn_blocking(move || -> std::io::Result<(Option<std::process::ExitStatus>, String)> {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            let status = loop {
                match child.try_wait()? {
                    Some(status) => break Some(status),
                    None => {
                        if std::time::Instant::now() >= deadline {
                            let _ = child.kill();
                            break None;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            };
            let mut out = String::new();
            child_stdout.read_to_string(&mut out)?;
            Ok((status, out))
        })
        .await
        .expect("spawn_blocking panicked");

    let (status, stdout) = exit_result.expect("wait/read error");
    match status {
        Some(status) => assert!(
            status.success(),
            "self_load_spawned_thread exited with non-zero/abnormal status: {status} — \
             this is exactly the failure mode of the bug this test guards against \
             (a segfault on the spawned thread's first hooked allocation)"
        ),
        None => panic!("self_load_spawned_thread did not exit within 15s"),
    }
    assert!(
        stdout.contains("self_load_spawned_thread: done"),
        "harness stdout missing completion marker — output:\n{stdout}"
    );

    let expected = parse_expected(&stdout);
    assert_eq!(expected.allocs.len(), ALLOC_COUNT, "harness printed wrong ALLOC line count");
    assert_eq!(expected.frees.len(), ALLOC_COUNT, "harness printed wrong FREE line count");

    tokio::time::sleep(Duration::from_secs(1)).await;
    ingest_handle.abort();
    drop(tx);
    match tokio::time::timeout(Duration::from_secs(3), graph_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.is_cancelled() => {}
        Ok(Err(e)) => panic!("graph task panicked: {e}"),
        Err(_) => panic!("graph task did not finish within timeout"),
    }

    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();
    let events = raw_events.lock().unwrap();
    let mut s = Summary::default();
    let expected_frees: std::collections::HashSet<u64> = expected.frees.iter().copied().collect();

    for ev in events.iter() {
        s.total_events_seen += 1;
        match ev.kind {
            0 => {
                graph.on_alloc(ev, &resolver);
                if let Some(&expected_size) = expected.allocs.get(&ev.ptr) {
                    s.matched_allocs.insert(ev.ptr);
                    if ev.size != expected_size {
                        s.alloc_size_mismatches.push((ev.ptr, expected_size, ev.size));
                    }
                }
            }
            1 => {
                graph.on_dealloc(ev.ptr);
                if expected_frees.contains(&ev.ptr) {
                    s.matched_frees.insert(ev.ptr);
                }
            }
            _ => {}
        }
    }

    *summary_for_graph.lock().unwrap() = s;
    let s = summary.lock().unwrap();
    eprintln!(
        "hook_self_load_spawned_thread summary: matched_allocs={} matched_frees={} \
         total_events_seen={} size_mismatches={:?}",
        s.matched_allocs.len(), s.matched_frees.len(), s.total_events_seen, s.alloc_size_mismatches
    );

    assert_eq!(
        s.matched_allocs.len(), ALLOC_COUNT,
        "expected all {ALLOC_COUNT} spawned-thread allocations to be captured by pointer \
         identity, matched {} of {}", s.matched_allocs.len(), expected.allocs.len()
    );
    assert!(
        s.alloc_size_mismatches.is_empty(),
        "captured alloc events with wrong size for a known pointer: {:?}",
        s.alloc_size_mismatches
    );
    assert_eq!(
        s.matched_frees.len(), ALLOC_COUNT,
        "expected all {ALLOC_COUNT} spawned-thread frees to be captured by pointer identity, \
         matched {}", s.matched_frees.len()
    );
}
