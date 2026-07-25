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

/// For each `(ptr, size)` in `expected`, checks the *last* `kind == 0`
/// (alloc) event captured for that ptr against `expected`'s size — not the
/// first.
///
/// Comparing the first occurrence (this function's predecessor) is unsound:
/// the harness's own `println!` calls after every real `alloc_n`/`free_n`
/// make real `HeapAlloc`/`HeapFree` calls of their own (stdout's internal
/// buffering), which the same active hook faithfully captures too. The
/// allocator can legitimately reuse an address for one of these incidental,
/// short-lived allocations *before* handing that same address out for the
/// harness's own real scripted allocation later in the run — confirmed via
/// direct event-log instrumentation during the 2026-07-26 flake
/// investigation: address `0x27ff25f61f0` was genuinely alloc'd (size 30)
/// and freed by stdout's own buffering, then alloc'd for real (size 32) by
/// the harness, strictly in that order, both captured correctly. Matching
/// the first occurrence for that ptr picks up the unrelated incidental
/// allocation instead of the real one and reports a false size mismatch.
/// The *last* alloc event for a ptr is always the one still live when its
/// matching free (tracked separately, by `expected_frees`) occurs, so it's
/// the one that actually corresponds to what the harness scripted.
fn find_alloc_size_mismatches(
    events: &[heaplens_protocol::AllocEvent],
    expected: &std::collections::HashMap<u64, u64>,
) -> Vec<(u64, u64, u64)> {
    let mut last_alloc_size: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for ev in events {
        if ev.kind == 0 && expected.contains_key(&ev.ptr) {
            last_alloc_size.insert(ev.ptr, ev.size);
        }
    }
    let mut mismatches: Vec<(u64, u64, u64)> = expected
        .iter()
        .filter_map(|(&ptr, &expected_size)| {
            let actual = *last_alloc_size.get(&ptr)?;
            (actual != expected_size).then_some((ptr, expected_size, actual))
        })
        .collect();
    mismatches.sort();
    mismatches
}

#[cfg(test)]
mod find_alloc_size_mismatches_tests {
    use super::find_alloc_size_mismatches;
    use heaplens_protocol::{AllocEvent, EventKind};
    use std::collections::HashMap;

    fn alloc_ev(ptr: u64, size: u64, ts: u64) -> AllocEvent {
        AllocEvent::new(EventKind::Alloc, ptr, 0, size, 8, ts, [0u64; 16], 0)
    }

    fn free_ev(ptr: u64, ts: u64) -> AllocEvent {
        AllocEvent::new(EventKind::Dealloc, ptr, 0, 0, 0, ts, [0u64; 16], 0)
    }

    #[test]
    fn a_single_alloc_matching_expected_size_has_no_mismatch() {
        let events = vec![alloc_ev(0x100, 32, 1)];
        let expected = HashMap::from([(0x100, 32)]);
        assert_eq!(find_alloc_size_mismatches(&events, &expected), vec![]);
    }

    #[test]
    fn a_single_alloc_with_wrong_size_is_a_real_mismatch() {
        let events = vec![alloc_ev(0x100, 40, 1)];
        let expected = HashMap::from([(0x100, 32)]);
        assert_eq!(find_alloc_size_mismatches(&events, &expected), vec![(0x100, 32, 40)]);
    }

    /// The exact confirmed scenario: an incidental alloc+free (stdout
    /// buffering, size 30) reuses an address before the harness's own real
    /// scripted alloc (size 32) claims it. Must not report a mismatch —
    /// the *last* alloc event (32) is the one that matters.
    #[test]
    fn address_reused_by_an_incidental_alloc_before_the_real_one_is_not_a_mismatch() {
        let events = vec![
            alloc_ev(0x27ff25f61f0, 30, 100), // incidental (stdout buffering)
            free_ev(0x27ff25f61f0, 200),      // incidental freed
            alloc_ev(0x27ff25f61f0, 32, 300), // the harness's real alloc
            free_ev(0x27ff25f61f0, 400),      // the harness's real free
        ];
        let expected = HashMap::from([(0x27ff25f61f0, 32)]);
        assert_eq!(find_alloc_size_mismatches(&events, &expected), vec![]);
    }

    #[test]
    fn a_pointer_never_alloced_is_absent_not_a_mismatch() {
        let events: Vec<AllocEvent> = vec![];
        let expected = HashMap::from([(0x100, 32)]);
        assert_eq!(find_alloc_size_mismatches(&events, &expected), vec![]);
    }

    #[test]
    fn events_for_untracked_pointers_are_ignored() {
        let events = vec![alloc_ev(0x999, 999, 1), alloc_ev(0x100, 32, 2)];
        let expected = HashMap::from([(0x100, 32)]);
        assert_eq!(find_alloc_size_mismatches(&events, &expected), vec![]);
    }
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
                if expected.allocs.contains_key(&ev.ptr) {
                    s.matched_allocs.insert(ev.ptr);
                }
            }
            1 => {
                graph.on_dealloc(ev.ptr, ev.ts_nanos);
                if expected_frees.contains(&ev.ptr) {
                    s.matched_frees.insert(ev.ptr);
                }
            }
            _ => {}
        }
    }
    s.alloc_size_mismatches = find_alloc_size_mismatches(&events, &expected.allocs);

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
