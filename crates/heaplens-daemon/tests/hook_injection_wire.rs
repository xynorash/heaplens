#![cfg(windows)]
//! Stage 7, Step 2 acceptance gate (docs/stage7-injection-design.md §8).
//!
//! Real cross-process injection: spawns `injection_target.exe` (a plain,
//! separate, debug-info-carrying process with no dependency on
//! `heaplens-hook`/`heaplens-alloc`), attaches to it via a real
//! `heaplens-injector.exe --attach <pid>` subprocess call timed against the
//! target's own `TARGET_PID=`/`WORKLOAD_DONE` stdout markers, and reproduces
//! Step 1's exact pointer-identity capture proof — but this time the hook
//! DLL was injected into a process that never linked against it, not
//! self-loaded. Detach is driven the same way, and the post-detach workload
//! must again produce zero captured events, proving detach actually removed
//! the hooks (not just disabled them in a still-loaded DLL).
//!
//! See `hook_self_load_wire.rs`'s module doc for why matching is done by
//! pointer identity rather than raw event counts (Rtl*Heap hooking is
//! process-wide and captures ambient Windows-internal heap traffic too).

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::ingest;
use heaplens_daemon::msg::GraphMsg;
use heaplens_daemon::resolver::Resolver;

const PIPE_NAME: &str = r"\\.\pipe\heaplens";

// Must match crates/heaplens-hook/examples/injection_target.rs exactly.
const ALLOC_COUNT: usize = 50;
const REALLOC_COUNT: usize = 10;
const UNTRACKED_REALLOC_COUNT: usize = 1;
const EXPECTED_DEALLOC_COUNT: usize = ALLOC_COUNT + UNTRACKED_REALLOC_COUNT;

#[derive(Default, Debug)]
struct Expected {
    allocs: std::collections::HashMap<u64, u64>,
    reallocs: Vec<(u64, u64, u64)>,
    frees: Vec<u64>,
    post_detach_ptrs: std::collections::HashSet<u64>,
}

fn parse_hex_ptr(s: &str) -> u64 {
    let hex = s.trim_start_matches("0x");
    u64::from_str_radix(hex, 16).unwrap_or_else(|e| panic!("bad pointer {s:?}: {e}"))
}

fn parse_expected(lines: &[String]) -> Expected {
    let mut e = Expected::default();
    for line in lines {
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

fn target_debug_dir() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path")
        .to_path_buf()
}

fn target_path() -> std::path::PathBuf {
    target_debug_dir().join("examples").join("injection_target.exe")
}

fn injector_path() -> std::path::PathBuf {
    target_debug_dir().join("heaplens-injector.exe")
}

fn hook_dll_path() -> std::path::PathBuf {
    target_debug_dir().join("heaplens_hook.dll")
}

#[derive(Default, Debug)]
struct Summary {
    matched_allocs: std::collections::HashSet<u64>,
    matched_reallocs: std::collections::HashSet<(u64, u64)>,
    matched_frees: std::collections::HashSet<u64>,
    alloc_size_mismatches: Vec<(u64, u64, u64)>,
    total_events_seen: u64,
    post_detach_leaks: Vec<u64>,
}

/// Runs `heaplens-injector.exe <pid> <mode>`, returning (success, stdout+stderr).
fn run_injector(pid_arg: &str, mode: &str) -> (bool, String) {
    let out = std::process::Command::new(injector_path())
        .arg(pid_arg)
        .arg(mode)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn heaplens-injector: {e}"));
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_injection_end_to_end() {
    let target = target_path();
    if !target.exists() {
        panic!(
            "injection_target.exe not found at {target:?}.\n\
             Build it first: cargo build --example injection_target -p heaplens-hook"
        );
    }
    let injector = injector_path();
    if !injector.exists() {
        panic!(
            "heaplens-injector.exe not found at {injector:?}.\n\
             Build it first: cargo build -p heaplens-injector"
        );
    }
    let dll = hook_dll_path();
    if !dll.exists() {
        panic!("heaplens_hook.dll not found at {dll:?}.\nBuild it first: cargo build -p heaplens-hook");
    }

    // ── Negative validation-gate tests, before touching a real target ──────
    // Nonexistent PID: OpenProcess must fail cleanly, not hang or crash.
    let (ok, out) = run_injector("999999999", "--attach");
    assert!(!ok, "attach to a nonexistent pid must fail, got: {out}");
    assert!(
        out.contains("cannot open process") || out.contains("access denied"),
        "expected a clear cannot-open message for a nonexistent pid, got: {out}"
    );

    // Malformed pid argument.
    let (ok, out) = run_injector("not-a-pid", "--attach");
    assert!(!ok, "attach with a non-numeric pid must fail, got: {out}");
    assert!(out.contains("invalid pid"), "expected an invalid-pid message, got: {out}");

    // ── Real cross-process attach/detach against a cooperative target ──────
    let summary = Arc::new(Mutex::new(Summary::default()));
    let summary_for_graph = summary.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();
    let ingest_tx = tx.clone();
    let ingest_handle = tokio::spawn(ingest::run(PIPE_NAME.to_owned(), ingest_tx));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut child = std::process::Command::new(&target)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn injection_target: {e}"));
    let child_stdout = child.stdout.take().expect("piped stdout");

    // Read the target's stdout on a blocking thread, forwarding every line
    // to this test via a channel so we can synchronize attach/detach against
    // its markers while it keeps running, and still keep the full text for
    // parsing afterward.
    let (line_tx, line_rx) = std_mpsc::channel::<String>();
    let reader_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(child_stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim_end().to_owned();
                    if line_tx.send(trimmed).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut all_lines: Vec<String> = Vec::new();
    let mut pid: Option<u32> = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pid.is_none() {
        if std::time::Instant::now() >= deadline {
            panic!("injection_target did not print TARGET_PID within 5s");
        }
        match line_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => {
                if let Some(rest) = l.strip_prefix("TARGET_PID=") {
                    pid = Some(rest.trim().parse().expect("TARGET_PID not a valid u32"));
                }
                all_lines.push(l);
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                panic!("injection_target exited before printing TARGET_PID");
            }
        }
    }
    let pid = pid.unwrap();

    // Attach now, inside the target's 800ms pre-workload window.
    let (ok, out) = run_injector(&pid.to_string(), "--attach");
    assert!(ok, "attach to live target {pid} failed: {out}");
    assert!(
        out.contains("HeapLensHookAttachRemote succeeded"),
        "expected an explicit attach-succeeded message, got: {out}"
    );

    // Drain lines until the workload-done marker (or the process exits).
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_workload_done = false;
    while !saw_workload_done {
        if std::time::Instant::now() >= deadline {
            panic!("injection_target did not print WORKLOAD_DONE within 10s");
        }
        match line_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(l) => {
                if l == "WORKLOAD_DONE" {
                    saw_workload_done = true;
                }
                all_lines.push(l);
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                panic!("injection_target exited before printing WORKLOAD_DONE");
            }
        }
    }

    // Detach — the target is now in its 2s pre-post-detach-workload sleep.
    let (ok, out) = run_injector(&pid.to_string(), "--detach");
    assert!(ok, "detach from live target {pid} failed: {out}");
    assert!(
        out.contains("HeapLensHookDetach succeeded"),
        "expected an explicit detach-succeeded message, got: {out}"
    );
    assert!(
        out.contains("heaplens_hook.dll unloaded"),
        "expected confirmation the DLL was unloaded from the target, got: {out}"
    );

    // Confirm the target process is still alive and unharmed by detach —
    // clean detach must not crash or kill the host process.
    match child.try_wait() {
        Ok(None) => {} // still running — expected
        Ok(Some(status)) => panic!("target process exited unexpectedly right after detach: {status}"),
        Err(e) => panic!("try_wait failed: {e}"),
    }

    // Drain remaining lines (post-detach workload + completion marker) until
    // the target exits on its own.
    let exit_status = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(status) => return status,
                None => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        panic!("injection_target did not exit within 10s after detach");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    });
    while let Ok(l) = line_rx.recv_timeout(Duration::from_secs(6)) {
        all_lines.push(l);
    }
    let status = exit_status.join().expect("exit-status thread panicked");
    assert!(status.success(), "injection_target exited with non-zero status: {status}");
    let _ = reader_handle.join();

    assert!(
        all_lines.iter().any(|l| l == "injection_target: done"),
        "target stdout missing completion marker — output:\n{}",
        all_lines.join("\n")
    );

    let expected = parse_expected(&all_lines);
    assert_eq!(expected.allocs.len(), ALLOC_COUNT, "target printed wrong ALLOC line count");
    assert_eq!(
        expected.reallocs.len(), REALLOC_COUNT + UNTRACKED_REALLOC_COUNT,
        "target printed wrong REALLOC line count"
    );
    assert_eq!(expected.frees.len(), EXPECTED_DEALLOC_COUNT, "target printed wrong FREE line count");
    assert_eq!(expected.post_detach_ptrs.len(), 5, "target printed wrong POST_DETACH pointer count");

    // Drain window so any straggling wire traffic is collected.
    tokio::time::sleep(Duration::from_secs(1)).await;
    ingest_handle.abort();
    drop(tx);

    let raw_events: Arc<Mutex<Vec<heaplens_protocol::AllocEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let raw_events_for_graph = raw_events.clone();
    let mut resolver = Resolver::new();
    while let Ok(msg) = rx.try_recv() {
        match msg {
            GraphMsg::Symbols(syms) => {
                for (addr, name, is_machinery) in syms {
                    resolver.insert(addr, name, is_machinery);
                }
            }
            GraphMsg::Events(events) => {
                raw_events_for_graph.lock().unwrap().extend(events);
            }
            GraphMsg::Tick | GraphMsg::TargetConnected { .. } | GraphMsg::TargetDisconnected { .. } => {}
        }
    }

    let mut graph = OwnershipGraph::new();
    let resolver = Resolver::new();
    let events = raw_events.lock().unwrap();
    let mut s = Summary::default();
    let expected_realloc_pairs: std::collections::HashSet<(u64, u64)> =
        expected.reallocs.iter().map(|(o, n, _)| (*o, *n)).collect();
    let expected_frees: std::collections::HashSet<u64> = expected.frees.iter().copied().collect();

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
        "hook_injection_wire summary: matched_allocs={} matched_reallocs={} matched_frees={} \
         total_events_seen={} size_mismatches={:?} post_detach_leaks={:?}",
        s.matched_allocs.len(), s.matched_reallocs.len(), s.matched_frees.len(),
        s.total_events_seen, s.alloc_size_mismatches, s.post_detach_leaks
    );

    assert_eq!(
        s.matched_allocs.len(), ALLOC_COUNT,
        "expected all {ALLOC_COUNT} scripted allocations to be captured cross-process by pointer \
         identity, matched {} of {}", s.matched_allocs.len(), expected.allocs.len()
    );
    assert!(
        s.alloc_size_mismatches.is_empty(),
        "captured alloc events with wrong size for a known pointer: {:?}",
        s.alloc_size_mismatches
    );
    assert_eq!(
        s.matched_reallocs.len(), REALLOC_COUNT + UNTRACKED_REALLOC_COUNT,
        "expected all {} scripted reallocs (including the untracked pre-attach-pointer one) to be \
         captured, matched {}", REALLOC_COUNT + UNTRACKED_REALLOC_COUNT, s.matched_reallocs.len()
    );
    assert_eq!(
        s.matched_frees.len(), EXPECTED_DEALLOC_COUNT,
        "expected all {EXPECTED_DEALLOC_COUNT} scripted frees to be captured, matched {}",
        s.matched_frees.len()
    );
    assert!(
        s.post_detach_leaks.is_empty(),
        "captured event(s) touching a post-detach pointer — hooks were not fully removed by \
         detach: {:?}",
        s.post_detach_leaks
    );
}
