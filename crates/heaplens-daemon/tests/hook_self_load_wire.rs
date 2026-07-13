#![cfg(windows)]
//! Stage 7, Step 1 acceptance gate (docs/stage7-injection-design.md §8).
//!
//! Runs `heaplens-hook`'s self-load harness (`examples/self_load_harness.rs`
//! in the `heaplens-hook` package) as a real subprocess against a real
//! daemon, and asserts *exact* event counts and sizes against the harness's
//! known, scripted workload — not just "some events arrived." A gate that
//! only checked "the process ran without crashing" would pass with a
//! silently broken capture path.
//!
//! The harness prints every scripted operation's exact pointer (and size)
//! to stdout. This test parses that output and matches daemon-side events
//! to it by pointer identity, rather than asserting a raw global count:
//! hooking `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` intercepts
//! *every* call to those functions process-wide, including Windows' own
//! internal heap traffic triggered as a side effect of the writer thread's
//! own pipe I/O (confirmed empirically — extra captured events with sizes
//! never requested by the scripted workload). Pointer-identity matching is
//! the only reliable way to isolate "did the workload's own operations get
//! captured correctly" from that ambient, expected noise.

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

// Must match crates/heaplens-hook/examples/self_load_harness.rs exactly.
const ALLOC_COUNT: usize = 50;
const REALLOC_COUNT: usize = 10;
const UNTRACKED_REALLOC_COUNT: usize = 1;
const EXPECTED_DEALLOC_COUNT: usize = ALLOC_COUNT + UNTRACKED_REALLOC_COUNT;

#[derive(Default, Debug)]
struct Expected {
    /// ptr -> size, for the 50 direct allocations.
    allocs: std::collections::HashMap<u64, u64>,
    /// (old_ptr, new_ptr, size) for the 10 tracked + 1 untracked reallocs.
    reallocs: Vec<(u64, u64, u64)>,
    /// The exact pointers freed, in order (51 total).
    frees: Vec<u64>,
    /// Pointers used by the post-detach workload (5 allocs + 5 frees) —
    /// must NEVER appear in any captured event; their presence would prove
    /// hooks were not actually removed by detach.
    post_detach_ptrs: std::collections::HashSet<u64>,
}

fn parse_hex_ptr(s: &str) -> u64 {
    // Harness prints pointers via `{:?}` on `*mut u8`, e.g. "0x1a2b3c4d".
    let hex = s.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or_else(|e| panic!("bad pointer {s:?}: {e}"))
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
            ["REALLOC", old, new, size] => {
                let old = parse_hex_ptr(old.trim_start_matches("old="));
                let new = parse_hex_ptr(new.trim_start_matches("new="));
                let size: u64 = size.trim_start_matches("size=").parse().unwrap();
                e.reallocs.push((old, new, size));
            }
            ["REALLOC_UNTRACKED", old, new, size] => {
                let old = parse_hex_ptr(old.trim_start_matches("old="));
                let new = parse_hex_ptr(new.trim_start_matches("new="));
                let size: u64 = size.trim_start_matches("size=").parse().unwrap();
                e.reallocs.push((old, new, size));
            }
            ["FREE", ptr] => {
                let ptr = parse_hex_ptr(ptr.trim_start_matches("ptr="));
                e.frees.push(ptr);
            }
            ["POST_DETACH_ALLOC", ptr, _size] => {
                e.post_detach_ptrs.insert(parse_hex_ptr(ptr.trim_start_matches("ptr=")));
            }
            ["POST_DETACH_FREE", ptr] => {
                e.post_detach_ptrs.insert(parse_hex_ptr(ptr.trim_start_matches("ptr=")));
            }
            _ => {}
        }
    }
    e
}

fn harness_path() -> std::path::PathBuf {
    // Test binary is at target/debug/deps/<name>-<hash>.exe;
    // the harness is at target/debug/examples/self_load_harness.exe.
    let test_exe = std::env::current_exe().unwrap();
    let target_debug = test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path");
    target_debug.join("examples").join("self_load_harness.exe")
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
    matched_reallocs: std::collections::HashSet<(u64, u64)>,
    matched_frees: std::collections::HashSet<u64>,
    alloc_size_mismatches: Vec<(u64, u64, u64)>, // ptr, expected, actual
    total_events_seen: u64,
    /// Any captured event touching a pointer the post-detach workload
    /// used — a nonempty list here means hooks were not actually removed.
    post_detach_leaks: Vec<u64>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_self_load_end_to_end() {
    let harness = harness_path();
    if !harness.exists() {
        panic!(
            "self_load_harness.exe not found at {harness:?}.\n\
             Build it first: cargo build --example self_load_harness -p heaplens-hook"
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
        .unwrap_or_else(|e| panic!("failed to spawn self_load_harness: {e}"));
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    // Expected sets are filled in once the child's stdout is fully read,
    // after it exits — but the graph task needs them to filter incoming
    // events. Use a channel: the graph task starts collecting immediately
    // (untouched by "expected" until it arrives) and re-evaluates once the
    // expected set is known. Simpler: buffer all raw events first, process
    // against `Expected` after the child exits and stdout is parsed.
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
                    _ => break,
                },
                _ = tokio::time::sleep(remaining) => break,
            }
        }
    });

    // Wait for the harness to exit.
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
            "self_load_harness exited with non-zero status: {status}"
        ),
        None => panic!("self_load_harness did not exit within 15s"),
    }
    assert!(
        stdout.contains("self_load_harness: done"),
        "harness stdout missing completion marker — output:\n{stdout}"
    );

    let expected = parse_expected(&stdout);
    assert_eq!(expected.allocs.len(), ALLOC_COUNT, "harness printed wrong ALLOC line count");
    assert_eq!(
        expected.reallocs.len(), REALLOC_COUNT + UNTRACKED_REALLOC_COUNT,
        "harness printed wrong REALLOC line count"
    );
    assert_eq!(expected.frees.len(), EXPECTED_DEALLOC_COUNT, "harness printed wrong FREE line count");
    assert_eq!(expected.post_detach_ptrs.len(), 5, "harness printed wrong POST_DETACH pointer count");

    // Drain window for the post-detach workload (must produce zero events).
    tokio::time::sleep(Duration::from_secs(2)).await;
    ingest_handle.abort();
    drop(tx);
    match tokio::time::timeout(Duration::from_secs(3), graph_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) if e.is_cancelled() => {}
        Ok(Err(e)) => panic!("graph task panicked: {e}"),
        Err(_) => panic!("graph task did not finish within timeout"),
    }

    // Now replay the raw events against a fresh graph, matching against
    // `expected` by pointer identity.
    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();
    let events = raw_events.lock().unwrap();
    let mut s = Summary::default();
    let expected_realloc_pairs: std::collections::HashSet<(u64, u64)> =
        expected.reallocs.iter().map(|(o, n, _)| (*o, *n)).collect();
    let expected_frees: std::collections::HashSet<u64> = expected.frees.iter().copied().collect();

    // The post-detach workload's pointers can coincidentally equal a
    // pre-detach pointer (Windows' heap allocator readily reuses a
    // just-freed block for the next similarly-sized request) — a pointer
    // *value* match alone doesn't prove a leak. What matters is whether any
    // event *after* the pre-detach workload finished touches one of those
    // pointers. Find the last event index that matches any pre-detach
    // pointer; only events after it are eligible to count as leaks.
    let last_pre_detach_index = events
        .iter()
        .enumerate()
        .filter(|(_, ev)| match ev.kind {
            0 => expected.allocs.contains_key(&ev.ptr),
            1 => expected_frees.contains(&ev.ptr),
            2 => expected_realloc_pairs.contains(&(ev.old_ptr, ev.ptr)),
            _ => false,
        })
        .map(|(i, _)| i)
        .max();

    for (i, ev) in events.iter().enumerate() {
        s.total_events_seen += 1;
        let eligible_for_leak_check = last_pre_detach_index.is_none_or(|last| i > last);
        match ev.kind {
            0 => {
                graph.on_alloc(ev, &resolver);
                if let Some(&expected_size) = expected.allocs.get(&ev.ptr) {
                    s.matched_allocs.insert(ev.ptr);
                    if ev.size != expected_size {
                        s.alloc_size_mismatches.push((ev.ptr, expected_size, ev.size));
                    }
                }
                if eligible_for_leak_check && expected.post_detach_ptrs.contains(&ev.ptr) {
                    s.post_detach_leaks.push(ev.ptr);
                }
            }
            1 => {
                graph.on_dealloc(ev.ptr);
                if expected_frees.contains(&ev.ptr) {
                    s.matched_frees.insert(ev.ptr);
                }
                if eligible_for_leak_check && expected.post_detach_ptrs.contains(&ev.ptr) {
                    s.post_detach_leaks.push(ev.ptr);
                }
            }
            2 => {
                graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver);
                if expected_realloc_pairs.contains(&(ev.old_ptr, ev.ptr)) {
                    s.matched_reallocs.insert((ev.old_ptr, ev.ptr));
                }
                if eligible_for_leak_check && expected.post_detach_ptrs.contains(&ev.ptr) {
                    s.post_detach_leaks.push(ev.ptr);
                }
            }
            _ => {}
        }
    }

    *summary_for_graph.lock().unwrap() = s;
    let s = summary.lock().unwrap();
    eprintln!(
        "hook_self_load_wire summary: matched_allocs={} matched_reallocs={} matched_frees={} \
         total_events_seen={} size_mismatches={:?} post_detach_leaks={:?}",
        s.matched_allocs.len(), s.matched_reallocs.len(), s.matched_frees.len(),
        s.total_events_seen, s.alloc_size_mismatches, s.post_detach_leaks
    );

    // Exact matches against the harness's own known-good pointers — not
    // just "at least," and not a brittle global total (see module doc).
    assert_eq!(
        s.matched_allocs.len(), ALLOC_COUNT,
        "expected all {ALLOC_COUNT} scripted allocations to be captured by pointer identity, \
         matched {} of {}", s.matched_allocs.len(), expected.allocs.len()
    );
    assert!(
        s.alloc_size_mismatches.is_empty(),
        "captured alloc events with wrong size for a known pointer: {:?}",
        s.alloc_size_mismatches
    );
    assert_eq!(
        s.matched_reallocs.len(), REALLOC_COUNT + UNTRACKED_REALLOC_COUNT,
        "expected all {} scripted reallocs (including the untracked pre-attach-pointer one, \
         §1.4) to be captured by (old_ptr, new_ptr) identity, matched {}",
        REALLOC_COUNT + UNTRACKED_REALLOC_COUNT, s.matched_reallocs.len()
    );
    assert_eq!(
        s.matched_frees.len(), EXPECTED_DEALLOC_COUNT,
        "expected all {EXPECTED_DEALLOC_COUNT} scripted frees to be captured by pointer \
         identity, matched {}", s.matched_frees.len()
    );

    // Positive proof that detach actually removed the hooks: none of the
    // post-detach workload's own pointers were captured as any kind of
    // event. This does not depend on coincidental pointer non-reuse (the
    // way an "absence from the pre-detach sets" check would) — these
    // pointers are checked explicitly, by their own dedicated marker.
    assert!(
        s.post_detach_leaks.is_empty(),
        "captured event(s) touching a post-detach pointer — hooks were not fully removed by \
         detach: {:?}",
        s.post_detach_leaks
    );
}
