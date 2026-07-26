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
//! ## Root-caused and fixed, 2026-07-26 (same day, follow-up pass)
//!
//! Confirmed via WinDbg Preview (`cdbX64.exe -pv`, non-invasive attach —
//! invasive attach was itself found to disturb the hang, reliably making
//! it exit right as `DebugActiveProcess`-based attach completed, 3/3
//! times; non-invasive attach doesn't). Symbol-resolved stack trace of
//! the sole surviving ("main") thread, reproduced identically on two
//! separate hung instances:
//!
//! ```text
//! ntdll!NtWaitForAlertByThreadId
//! ntdll!RtlWaitOnAddress
//! KERNELBASE!WaitOnAddress
//! heaplens_hook!std::sys::sync::mutex::futex::Mutex::lock_contended
//! heaplens_hook!heaplens_alloc::capture::capture_stack   <- blocked here
//! heaplens_hook!heaplens_alloc::record
//! heaplens_hook!heaplens_hook::hook_heap_free
//! ucrtbase!free_base
//! ucrtbase!destroy_fls
//! ntdll!RtlpFlsDataCleanup
//! ntdll!LdrShutdownProcess
//! ntdll!RtlExitUserProcess
//! KERNEL32!ExitProcessImplementation
//! ucrtbase!common_exit
//! <target's own main>
//! ```
//!
//! Confirmed mechanism: `RtlExitUserProcess` abruptly terminates the other
//! worker threads without running their cleanup. If one was caught inside
//! `capture_stack`, holding `heaplens_alloc::DBGHELP_LOCK` (a process-wide
//! `Mutex<()>`, acquired on every hooked allocation from every thread —
//! see `capture.rs`), that lock is now orphaned permanently. The sole
//! surviving thread's own FLS cleanup then frees a fiber-local buffer;
//! since hooks were never disabled (no explicit detach), that free routes
//! through `hook_heap_free` -> `record` -> `capture_stack`, which tries to
//! acquire the same orphaned lock and blocks forever. This explains the
//! rate difference from `fls_race_repro.exe` precisely: threads spending a
//! larger fraction of their lifetime inside `capture_stack` (continuous
//! looping) have proportionally higher odds of being caught mid-lock at
//! the instant of abrupt termination than short-lived, mostly-idle ones.
//!
//! Fixed in `heaplens-alloc/src/capture.rs`: `capture_stack` now uses
//! `DBGHELP_LOCK.try_lock()` instead of `.lock()` — a held lock (orphaned
//! or merely contended, indistinguishable and both handled identically)
//! yields an empty, unresolved capture instead of blocking. Cannot
//! deadlock regardless of *why* the lock is unavailable. Re-validated: 25
//! manual runs (mixed batch/individual, exit codes explicitly checked)
//! all clean post-fix, on the same machine and binaries that showed 9/10
//! and 7/8 hung pre-fix.
//!
//! Conclusion: both defects on this path are now fixed and covered by
//! permanent regression tests below (none `#[ignore]`d).
//! `multithreaded_exit_without_detach_is_clean` is a real, asserting test
//! now that the fix makes it reliably pass — re-enabling the "Attach to
//! Process" UI gate is a separate, deliberate follow-up, not automatic
//! just because this is fixed.
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
/// Used to be `#[ignore]`d: this scenario didn't crash, but reproduced a
/// hang roughly 40-87.5% of the time across separate investigation
/// sessions (CPU-plateau observation, resistance to normal process
/// termination, injector's own `--detach` failing against a hung
/// instance). Root-caused via WinDbg (`cdb -pv`, symbol-resolved stack
/// trace, 2026-07-26): the sole surviving thread deadlocked in
/// `heaplens_alloc::capture::capture_stack`'s `DBGHELP_LOCK.lock()` —
/// `RtlExitUserProcess` abruptly terminates the other worker threads
/// without cleanup, and if one was caught holding that lock at that
/// instant, it's orphaned forever; the survivor's own FLS-cleanup-
/// triggered heap free (still routed through the active hook, since no
/// detach ran) then blocks on the same lock permanently. Fixed by
/// replacing `capture_stack`'s blocking `.lock()` with `.try_lock()` (see
/// `capture.rs`'s doc comment) — a held lock now yields an empty,
/// unresolved capture instead of blocking, which cannot deadlock
/// regardless of *why* the lock is held. Re-validated: 25 manual runs
/// (10 + 10 + 5, mixed batch and individually-tracked exit codes) all
/// clean post-fix, vs. 9/10 and 7/8 hung on the same machine, same
/// binaries, pre-fix. No longer `#[ignore]`d — this is now an assertion
/// of desired, confirmed-working behavior.
#[test]
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
