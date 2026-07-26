#![cfg(windows)]
//! Regression coverage for the hook owner-free crash diagnosis (Nash's
//! observed `0xC0000005` on "case 2": owner freed while children remain
//! live, under real cross-process injection) and its resolution.
//!
//! ## Original finding (2026-07-21/22, `feat/stage7-injection`)
//!
//! The owner-freed-while-children-live workload itself (owner alloc, 8
//! children alloc, owner free while children live, 20 rounds of
//! alloc/realloc/free churn, children free) never crashed — it completed
//! and printed its final marker every time, whether or not a daemon was
//! listening. The crash was **not** in that pattern.
//!
//! The real, reliably-reproducing mechanism found: the target process
//! exiting normally (falling off the end of `main`) while
//! `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` hooks were still
//! installed — i.e. `heaplens-injector --detach` was never called before
//! the target's own process exit — reliably terminated with exit status
//! `0xC0000005` (STATUS_ACCESS_VIOLATION). A temporary diagnostic Vectored
//! Exception Handler installed for that investigation never caught the
//! fault (no log entry) — consistent with a `__fastfail`-driven crash,
//! which bypasses SEH/VEH by design.
//!
//! ## Re-verified 2026-07-26, master — the crash no longer reproduces
//!
//! That original investigation (`cf6dccc`, 20:41 the same evening) predates
//! the TLS/FLS fix (`8cc0f1c`, 23:12) that replaced `guard.rs`/`ring.rs`'s
//! `thread_local!`-based per-thread state with raw `TlsAlloc`/`FlsAlloc`.
//! `guard.rs`'s own root-cause doc comment: the old `thread_local!` crashed
//! *any thread other than the one that called `LoadLibraryW`* on its very
//! first access — a target's own worker/writer threads are exactly that
//! population, and a process tearing down with those threads mid-execution
//! while hooks are active is exactly the scenario that would hit it. Nobody
//! explicitly re-validated this specific "exit without detach" scenario
//! against the fixed build until now.
//!
//! Re-tested directly, on master, with real `heaplens-injector --attach`:
//! - `owner_freed_while_children_live.exe` (single-threaded, the original
//!   repro): 15/15 clean exits, no crash. **Fixed, confirmed.**
//! - `fls_race_repro.exe` (a targeted repro for a distinct, *named*
//!   hazard already documented on `ring.rs`'s `thread_ring::shutdown` — a
//!   live FLS registration whose exit callback lives inside
//!   `heaplens_hook.dll` could in principle fire *after* the DLL is
//!   unmapped, contingent on Windows' informal, not-guaranteed
//!   unmap-vs-callback ordering during whole-process teardown; not the
//!   mechanism the TLS/FLS fix targeted, so checked separately: 200
//!   short-lived, unjoined threads per run, each registering exactly one
//!   fresh FLS slot then racing a hard `std::process::exit`): 30/30 clean
//!   runs, 6,000 total race attempts, no crash. **No reproducing evidence
//!   for this theoretical gap.**
//!
//! ## NEW finding, same investigation: multi-threaded exit-without-detach
//! can HANG (not crash)
//!
//! `exit_without_detach_multithreaded.exe` (several worker threads
//! *continuously* making hooked allocations in a tight loop, still live
//! when the process exits, never joined, no detach) does not crash — but
//! roughly 40-60% of runs (observed across ~15 attempts, several batches)
//! the process never terminates at all. Confirmed by direct observation,
//! not assumption:
//! - `Process.HasExited` stays false past a 15s wait; CPU time (queried via
//!   `Get-Process`) **plateaus** within the first couple of seconds and
//!   never grows again, even minutes later — consistent with the worker
//!   threads becoming *blocked*, not spinning.
//! - The hung process resists termination by three independent
//!   mechanisms tried in sequence: PowerShell `Stop-Process -Force`
//!   (reports success, process persists), `taskkill /F` (reports "no
//!   running instance" while `tasklist` simultaneously shows it still
//!   running), and finally `Invoke-CimMethod ... -MethodName Terminate`
//!   (WMI `Win32_Process.Terminate`), which did eventually succeed.
//! - `heaplens-injector --detach` against a hung instance fails with
//!   `OpenThread failed for worker thread <tid> in process <pid>` — the
//!   DLL's own persistent worker thread (`attach_impl`'s spawned thread,
//!   normally parked via alertable `SleepEx` and reused via `QueueUserAPC`
//!   for detach) appears to be gone or invalid, breaking the normal detach
//!   path too.
//!
//! Unconfirmed (no debugger available in this pass) but mechanically
//! plausible hypothesis: a worker thread suspended by the OS mid-critical-
//! section — e.g. holding `heaplens_alloc`'s `DBGHELP_LOCK`
//! (`capture_stack`'s `backtrace::trace_unsynchronized` serialization) or
//! `ring.rs`'s registry `Mutex` — exactly as process teardown begins,
//! deadlocking against whatever the exit sequence itself needs. This would
//! explain why `fls_race_repro.exe`'s threads (one allocation each, mostly
//! idle) never hit it in 6,000 attempts, while continuously-looping worker
//! threads (spending a much larger fraction of their lifetime inside the
//! hook's critical sections) hit it well over a third of the time.
//!
//! Conclusion: the *original* crash (`0xC0000005`) is fixed — resolved,
//! almost certainly as a side effect of the TLS/FLS work, per the timeline
//! and root-cause match above. But this investigation found a *different*,
//! more severe defect on the same "exit without detach" path: a hang that
//! resists normal process termination. **The "Attach to Process" UI gate
//! must stay disabled** — re-enabling it would trade a clean, honest crash
//! for a silent, hard-to-kill zombie process, which is worse, not better.
//! `multithreaded_exit_without_detach_is_clean` below is `#[ignore]`d
//! (asserting it unconditionally would non-deterministically hang the test
//! suite and leak unkillable processes on every run) — it exists to be run
//! deliberately once this is properly root-caused with real debugging
//! tools (this pass had none available) and fixed.
//!
//! This was, and remains, a **live** exposure, not just a test-harness
//! concern: the daemon's own `TargetDisconnected` handling (`main.rs`) does
//! not call `injector::detach` when a target exits on its own — it only
//! clears `attached_pid` and broadcasts `TargetExited`. Any real target
//! that exits while still attached hits this same path; these tests are
//! what stand behind re-enabling the "Attach to Process" UI gate.

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::time::Duration;

fn target_debug_dir() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path")
        .to_path_buf()
}

fn target_path(name: &str) -> std::path::PathBuf {
    target_debug_dir().join("examples").join(format!("{name}.exe"))
}

fn injector_path() -> std::path::PathBuf {
    target_debug_dir().join("heaplens-injector.exe")
}

fn read_until_marker(
    reader: &mut BufReader<std::process::ChildStdout>,
    marker: &str,
    timeout: Duration,
) -> Vec<String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut lines = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if std::time::Instant::now() >= deadline {
            break;
        }
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim_end().to_string();
                let hit = trimmed.starts_with(marker);
                lines.push(trimmed);
                if hit {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    lines
}

fn spawn_and_attach(
    target_name: &str,
    work_dir: &std::path::Path,
) -> (std::process::Child, BufReader<std::process::ChildStdout>, u32) {
    let target_exe = target_path(target_name);
    let injector_exe = injector_path();
    assert!(
        target_exe.exists(),
        "build the example first: cargo build --release -p heaplens-hook --example {target_name}"
    );
    assert!(injector_exe.exists(), "build the injector first: cargo build --release -p heaplens-injector");

    let mut target = std::process::Command::new(&target_exe)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn {target_name}.exe: {e}"));

    let stdout = target.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let pid_lines = read_until_marker(&mut reader, "TARGET_PID=", Duration::from_secs(5));
    let pid_line = pid_lines
        .iter()
        .find(|l| l.starts_with("TARGET_PID="))
        .expect("target never printed TARGET_PID");
    let pid: u32 = pid_line.trim_start_matches("TARGET_PID=").parse().unwrap();

    let attach_output = std::process::Command::new(&injector_exe)
        .arg(pid.to_string())
        .arg("--attach")
        .output()
        .expect("failed to spawn heaplens-injector --attach");
    assert!(
        attach_output.status.success(),
        "attach failed: stdout={} stderr={}",
        String::from_utf8_lossy(&attach_output.stdout),
        String::from_utf8_lossy(&attach_output.stderr)
    );

    (target, reader, pid)
}

/// The safe, correct-usage pattern: detach before the target exits. Must
/// stay clean — this is what real daemon-driven attach/detach cycles
/// already do (`heaplens-injector --detach` runs while the target is still
/// alive), and this test only confirms the owner-freed-while-children-live
/// workload itself introduces no new risk on that path.
#[test]
fn detach_before_target_exit_is_clean() {
    let work_dir = std::env::temp_dir().join(format!("heaplens_owner_free_clean_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

    let (mut target, mut reader, pid) = spawn_and_attach("owner_freed_while_children_live", &work_dir);

    let workload_lines = read_until_marker(&mut reader, "WORKLOAD_DONE", Duration::from_secs(10));
    assert!(
        workload_lines.iter().any(|l| l.starts_with("WORKLOAD_DONE")),
        "target did not reach WORKLOAD_DONE — got: {workload_lines:?}"
    );

    let injector_exe = injector_path();
    let detach_output = std::process::Command::new(&injector_exe)
        .arg(pid.to_string())
        .arg("--detach")
        .output()
        .expect("failed to spawn heaplens-injector --detach");
    assert!(
        detach_output.status.success(),
        "detach failed: stdout={} stderr={}",
        String::from_utf8_lossy(&detach_output.stdout),
        String::from_utf8_lossy(&detach_output.stderr)
    );

    let status = target.wait_timeout_or_kill(Duration::from_secs(5)).expect("target did not exit after detach");
    assert!(status.success(), "target exited abnormally even after a clean detach: {status:?}");

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// The scenario that used to crash reliably (`0xC0000005`), re-verified as
/// clean on master — see this file's module doc comment for the full
/// timeline/root-cause correlation with the TLS/FLS fix. No longer
/// `#[ignore]`d: this is now an assertion of desired, confirmed-working
/// behavior, not a tracked known-issue.
#[test]
fn target_exit_without_detach_is_clean() {
    let work_dir = std::env::temp_dir().join(format!("heaplens_owner_free_nocrash_repro_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

    let (mut target, mut reader, _pid) = spawn_and_attach("owner_freed_while_children_live", &work_dir);

    let workload_lines = read_until_marker(&mut reader, "WORKLOAD_DONE", Duration::from_secs(10));
    assert!(
        workload_lines.iter().any(|l| l.starts_with("WORKLOAD_DONE")),
        "target did not reach WORKLOAD_DONE — got: {workload_lines:?}"
    );

    // No detach — let the target exit on its own, hooks still active.
    let status = target.wait_timeout_or_kill(Duration::from_secs(5)).expect("target did not exit");
    assert!(
        status.success(),
        "target crashed on exit without detach (status={status:?}) — this scenario used to be a \
         confirmed, reliable crash (0xC0000005) and was verified clean before this test was \
         written as an assertion; if this fails, something regressed — see this file's module \
         doc comment for the full history"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// Multi-threaded variant: several worker threads still actively making
/// hooked allocations when the process exits, never joined, no detach.
/// Deliberately harder than the single-threaded repro above — the fixed
/// TLS/FLS mechanism was specifically about threads other than the one
/// that called `LoadLibraryW`, which a single-threaded target's `main`
/// alone can't exercise.
///
/// `#[ignore]`d: this scenario does NOT crash, but reproduces a hang
/// roughly 40-60% of the time (see this file's module doc comment for the
/// full evidence — CPU-plateau observation, three termination mechanisms
/// tried, injector's own `--detach` failing against a hung instance).
/// Asserting this unconditionally would non-deterministically hang the
/// test suite itself and leak processes that resist normal termination on
/// every run. Run explicitly (`cargo test -- --ignored`) once this is
/// root-caused with real debugging tools and fixed — at that point this
/// should both pass reliably AND lose its `#[ignore]`.
#[test]
#[ignore = "known hang (not crash), ~40-60% reproducing, resists normal process termination — see module doc comment"]
fn multithreaded_exit_without_detach_is_clean() {
    let work_dir = std::env::temp_dir().join(format!("heaplens_mt_exit_nocrash_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

    let (mut target, mut reader, _pid) = spawn_and_attach("exit_without_detach_multithreaded", &work_dir);

    let workload_lines = read_until_marker(&mut reader, "WORKLOAD_DONE", Duration::from_secs(10));
    assert!(
        workload_lines.iter().any(|l| l.starts_with("WORKLOAD_DONE")),
        "target did not reach WORKLOAD_DONE — got: {workload_lines:?}"
    );

    // No detach — worker threads still live and allocating, target exits
    // on its own.
    let status = target.wait_timeout_or_kill(Duration::from_secs(5)).expect("target did not exit");
    assert!(
        status.success(),
        "target crashed on exit without detach, worker threads still live (status={status:?})"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// Targeted regression for the specific, named FLS-unmap-ordering hazard
/// documented on `ring.rs`'s `thread_ring::shutdown` — see this file's
/// module doc comment. Not general stress: `fls_race_repro.exe` spawns many
/// short-lived, unjoined threads each registering exactly one fresh FLS
/// slot, then hard-exits immediately via `std::process::exit`, maximizing
/// the race between a thread's own exit-time FLS callback and the
/// process's DLL-unmap sequence.
#[test]
fn fls_race_exit_without_detach_is_clean() {
    let work_dir = std::env::temp_dir().join(format!("heaplens_fls_race_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

    let (mut target, mut reader, _pid) = spawn_and_attach("fls_race_repro", &work_dir);

    let workload_lines = read_until_marker(&mut reader, "SPAWNED", Duration::from_secs(5));
    assert!(
        workload_lines.iter().any(|l| l.starts_with("SPAWNED")),
        "target did not reach the SPAWNED marker — got: {workload_lines:?}"
    );

    // The target calls std::process::exit() immediately after printing
    // SPAWNED — no detach, 200 unjoined threads racing process teardown.
    let status = target.wait_timeout_or_kill(Duration::from_secs(5)).expect("target did not exit");
    assert!(
        status.success(),
        "target crashed under the targeted FLS-race repro (status={status:?}) — the theoretical \
         hazard documented on ring.rs's thread_ring::shutdown may have actually reproduced; \
         see this file's module doc comment"
    );

    let _ = std::fs::remove_dir_all(&work_dir);
}

trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::ExitStatus>;
}

impl WaitTimeoutOrKill for std::process::Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<std::process::ExitStatus> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if std::time::Instant::now() >= deadline {
                let _ = self.kill();
                let _ = self.wait();
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "process did not exit in time"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
