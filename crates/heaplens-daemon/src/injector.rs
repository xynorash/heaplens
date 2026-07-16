//! Stage 7 Step 3 (docs/stage7-injection-design.md §3.2): the daemon's own
//! orchestration of `heaplens-injector.exe` as a short-lived child process.
//! `heaplens-injector` is a synchronous, one-shot CLI tool — it runs the
//! attach or detach sequence to completion and exits; there is no long-lived
//! injector process to track after a call returns.

use std::path::PathBuf;

use tokio::process::Command;

const INJECTOR_EXE: &str = "heaplens-injector.exe";

/// Resolves `heaplens-injector.exe`'s path relative to this daemon's own
/// executable — the same "ships next to me" convention every other
/// component pair in this project uses (see `heaplens-injector`'s own
/// `dll_path()`, `heaplens-launcher`'s `exe_dir()`).
fn injector_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot determine daemon's own exe path: {e}"))?;
    let dir = exe.parent().ok_or("daemon exe has no parent directory")?;
    let path = dir.join(INJECTOR_EXE);
    if !path.exists() {
        return Err(format!("{INJECTOR_EXE} not found at {path:?} — expected next to heaplens-daemon.exe"));
    }
    Ok(path)
}

async fn run(pid: u32, mode: &str) -> Result<(), String> {
    let exe = injector_path()?;
    let output = Command::new(&exe)
        .arg(pid.to_string())
        .arg(mode)
        .output()
        .await
        .map_err(|e| format!("failed to spawn {INJECTOR_EXE}: {e}"))?;

    if output.status.success() {
        return Ok(());
    }
    let mut msg = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        if !msg.is_empty() {
            msg.push_str(" | ");
        }
        msg.push_str(stderr);
    }
    if msg.is_empty() {
        msg = format!("{INJECTOR_EXE} exited with {}", output.status);
    }
    Err(msg)
}

/// Runs `heaplens-injector <pid> --attach` to completion.
pub async fn attach(pid: u32) -> Result<(), String> {
    run(pid, "--attach").await
}

/// Runs `heaplens-injector <pid> --detach` to completion.
pub async fn detach(pid: u32) -> Result<(), String> {
    run(pid, "--detach").await
}
