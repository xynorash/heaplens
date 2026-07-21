#![cfg(windows)]
//! Regression test for the hook owner-free crash diagnosis (Nash's
//! observed `0xC0000005` on "case 2": owner freed while children remain
//! live, under real cross-process injection).
//!
//! Reproduction findings (repeated runs, real `heaplens-injector --attach`,
//! temporary diagnostic VEH installed in heaplens-hook — see diag_veh.rs):
//!
//! The owner-freed-while-children-live workload itself (owner alloc, 8
//! children alloc, owner free while children live, 20 rounds of
//! alloc/realloc/free churn, children free) never crashes — it completes
//! and prints its final marker every time, whether or not a daemon is
//! listening. The crash is **not** in that pattern.
//!
//! The real, reliably-reproducing mechanism found: if the target process
//! is allowed to exit **normally** (fall off the end of `main`) while the
//! `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` hooks are still
//! installed — i.e. `heaplens-injector --detach` was never called before
//! the target's own process exit — the process reliably terminates with
//! exit status `0xC0000005` (STATUS_ACCESS_VIOLATION), confirmed via
//! `std::process::ExitStatus` (the OS-reported code, not a shell artifact —
//! Git Bash's `$?`/`wait` reports a misleading "Segmentation fault" for the
//! same runs and cannot be trusted here). With an explicit `--detach`
//! called before the target exits, the same workload exits cleanly (status
//! 0) every time. This isolates the trigger precisely: hooks left active
//! through the target's own `DLL_PROCESS_DETACH`/process-teardown sequence,
//! not anything about the owner-free pattern itself. The temporary VEH did
//! not capture the fault (no log entry, no Windows Event Log Application
//! Error/WER entry either) — consistent with, though not proof of, the
//! same class of `__fastfail`-driven crash (bypasses SEH/VEH by design)
//! already documented nine times elsewhere in this module for hooks-active-
//! during-thread-teardown hazards, just for `DLL_PROCESS_DETACH` instead of
//! `DLL_THREAD_DETACH`. No fix is attempted here — this test only locks in
//! the two observed behaviors (safe with detach, crashes without) so the
//! mechanism stays documented and reproducible pending a decision on next
//! steps.
//!
//! Note this is a **live** exposure, not just a test-harness artifact: the
//! daemon's own `TargetDisconnected` handling (`main.rs`) does not call
//! `injector::detach` when a target exits on its own — it only clears
//! `attached_pid` and broadcasts `TargetExited`. Any real target that exits
//! while still attached hits this same path.

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

fn target_path() -> std::path::PathBuf {
    target_debug_dir().join("examples").join("owner_freed_while_children_live.exe")
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

fn spawn_and_attach(work_dir: &std::path::Path) -> (std::process::Child, BufReader<std::process::ChildStdout>, u32) {
    let target_exe = target_path();
    let injector_exe = injector_path();
    assert!(target_exe.exists(), "build the example first: cargo build -p heaplens-hook --example owner_freed_while_children_live");
    assert!(injector_exe.exists(), "build the injector first: cargo build -p heaplens-injector");

    let mut target = std::process::Command::new(&target_exe)
        .current_dir(work_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn owner_freed_while_children_live.exe");

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
    let veh_log = work_dir.join("heaplens_hook_veh.log");
    let _ = std::fs::remove_file(&veh_log);

    let (mut target, mut reader, pid) = spawn_and_attach(&work_dir);

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
    assert!(!veh_log.exists(), "VEH log written despite a clean detach: {:?}", std::fs::read_to_string(&veh_log).unwrap_or_default());

    let _ = std::fs::remove_dir_all(&work_dir);
}

/// Documents the known, reproducing crash: a target that exits normally
/// while still attached (no detach called) reliably dies with
/// `0xC0000005`. `#[ignore]`d so the default test run stays green — this is
/// tracking a known, reported issue, not asserting desired behavior. Run
/// explicitly with `cargo test -- --ignored` to reproduce.
///
/// No fix is attempted by this test or anywhere else in this change — see
/// this file's module doc comment.
#[test]
#[ignore = "known crash, reported not fixed — see module doc comment"]
fn target_exit_without_detach_currently_crashes() {
    let work_dir = std::env::temp_dir().join(format!("heaplens_owner_free_nocrash_repro_{}", std::process::id()));
    std::fs::create_dir_all(&work_dir).unwrap();

    let (mut target, mut reader, _pid) = spawn_and_attach(&work_dir);

    let workload_lines = read_until_marker(&mut reader, "WORKLOAD_DONE", Duration::from_secs(10));
    assert!(
        workload_lines.iter().any(|l| l.starts_with("WORKLOAD_DONE")),
        "target did not reach WORKLOAD_DONE — got: {workload_lines:?}"
    );

    // No detach — let the target exit on its own, hooks still active.
    let status = target.wait_timeout_or_kill(Duration::from_secs(5)).expect("target did not exit");
    assert!(
        !status.success(),
        "expected the known crash (status != 0) but the target exited cleanly — \
         if this assertion fails, the bug may have been fixed; re-check and update this test's ignore reason"
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
