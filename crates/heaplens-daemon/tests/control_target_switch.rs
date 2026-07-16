#![cfg(windows)]
//! Stage 7 Step 3 acceptance gate (docs/stage7-injection-design.md §8).
//!
//! Drives the daemon's new WS control messages (`ListProcesses`,
//! `AttachTarget`, `DetachTarget`) against the *real* `heaplens-daemon.exe`
//! binary — not an in-process mini graph loop, since Step 3's own new code
//! (process spawning, the target-switch transition) only lives in
//! `main.rs`'s real graph loop, not the test-only `run_graph_loop` helper
//! `ws_tests.rs` uses for the pre-existing snapshot/diff tests.
//!
//! Confirms: `ListProcesses` returns a real process list; `AttachTarget`
//! against a live, cooperative target (Step 2's `injection_target.exe`)
//! actually causes events to arrive and populate the graph, driven purely
//! through the WS control channel (no direct `heaplens-injector` CLI call
//! from the test); attaching a *second* target while the first is live
//! performs the §3.4 transition — old detached, graph cleared (node count
//! visibly drops to 0 before the new target's nodes arrive, not merged with
//! them) — and `DetachTarget` cleanly ends the session.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use heaplens_protocol::{ControlRequest, ControlResponse, GraphMessage};

const WS_ADDR: &str = "127.0.0.1:19998";
const READY_TIMEOUT: Duration = Duration::from_secs(15);

fn target_debug_dir() -> std::path::PathBuf {
    let test_exe = std::env::current_exe().unwrap();
    test_exe
        .parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target dir from test exe path")
        .to_path_buf()
}

fn daemon_exe_path() -> std::path::PathBuf {
    target_debug_dir().join("heaplens-daemon.exe")
}

fn target_exe_path() -> std::path::PathBuf {
    target_debug_dir().join("examples").join("injection_target.exe")
}

fn powershell_kill(name: &str) {
    let _ = Command::new("powershell.exe")
        .args(["-Command", &format!("Get-Process {name} -ErrorAction SilentlyContinue | Stop-Process -Force")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Kills every process it's holding when dropped — covers early `return`,
/// a failed `assert!`, and normal completion with one cleanup path, so a
/// failing assertion partway through can't leak a daemon or target process
/// into subsequent test runs.
struct Procs {
    daemon: Child,
    targets: Vec<Child>,
    db_path: std::path::PathBuf,
}

impl Drop for Procs {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        for t in &mut self.targets {
            let _ = t.kill();
        }
        powershell_kill("injection_target");
        let _ = std::fs::remove_file(&self.db_path);
    }
}

/// Reads lines from a child's piped stdout on a background thread,
/// forwarding each to a channel so the test can synchronize against the
/// target's own markers while it keeps running.
fn spawn_line_reader(child: &mut Child) -> std_mpsc::Receiver<String> {
    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std_mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if tx.send(line.trim_end().to_owned()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn wait_for_pid(rx: &std_mpsc::Receiver<String>) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(std::time::Instant::now() < deadline, "target did not print TARGET_PID within 5s");
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(rest) = l.strip_prefix("TARGET_PID=") {
                return rest.trim().parse().expect("TARGET_PID not a valid u32");
            }
        }
    }
}

fn spawn_target(procs: &mut Procs) -> (std_mpsc::Receiver<String>, u32) {
    let exe = target_exe_path();
    assert!(exe.exists(), "injection_target.exe not found at {exe:?} — build it first");
    let mut child = Command::new(&exe).stdout(Stdio::piped()).spawn().expect("failed to spawn injection_target");
    let rx = spawn_line_reader(&mut child);
    let pid = wait_for_pid(&rx);
    procs.targets.push(child);
    (rx, pid)
}

/// Sends one control request and returns the next `ControlResponse` seen —
/// skipping over any interleaved `GraphMessage` (snapshot/diff) frames,
/// which arrive on the same connection independently of request/response
/// pairing.
async fn send_control<S>(ws: &mut S, req: &ControlRequest) -> ControlResponse
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let json = serde_json::to_string(req).unwrap();
    ws.send(Message::Text(json)).await.expect("send control request");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(std::time::Instant::now() < deadline, "no ControlResponse arrived within 10s for {req:?}");
        let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await
        else {
            continue;
        };
        if let Ok(resp) = serde_json::from_str::<ControlResponse>(&text) {
            return resp;
        }
        // Not a ControlResponse — must be a GraphMessage snapshot/diff. Ignore and keep waiting.
    }
}

/// Waits for the next `GraphMessage` (snapshot or diff), ignoring any
/// interleaved `ControlResponse` frames.
async fn next_graph_message<S>(ws: &mut S, timeout: Duration) -> Option<GraphMessage>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let msg = tokio::time::timeout(remaining, ws.next()).await.ok().flatten()?.ok()?;
        let Message::Text(text) = msg else { continue };
        if let Ok(gm) = serde_json::from_str::<GraphMessage>(&text) {
            return Some(gm);
        }
        // Not a GraphMessage — a ControlResponse; ignore and keep waiting.
    }
}

fn node_count(msg: &GraphMessage) -> usize {
    match msg {
        GraphMessage::Snapshot { nodes, .. } => nodes.len(),
        GraphMessage::Diff { add, .. } => add.len(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_channel_drives_process_list_and_target_switch() {
    let daemon_exe = daemon_exe_path();
    assert!(daemon_exe.exists(), "heaplens-daemon.exe not found at {daemon_exe:?} — build it first");
    let injector_exe = target_debug_dir().join("heaplens-injector.exe");
    assert!(injector_exe.exists(), "heaplens-injector.exe not found next to the daemon — build it first");

    powershell_kill("injection_target");

    let db_path = std::env::temp_dir().join(format!("heaplens-step3-test-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db_path);

    let daemon = Command::new(&daemon_exe)
        .env("HEAPLENS_WS_ADDR", WS_ADDR)
        .env("HEAPLENS_DB_PATH", db_path.to_str().unwrap())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn heaplens-daemon.exe");

    let mut procs = Procs { daemon, targets: Vec::new(), db_path };

    // Wait for the daemon's WS port to accept connections.
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = procs.daemon.try_wait() {
            panic!("daemon exited early with {status} before its WS port came up");
        }
        if std::net::TcpStream::connect(WS_ADDR).is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "daemon WS port did not come up within {READY_TIMEOUT:?}");
        std::thread::sleep(Duration::from_millis(100));
    }

    let (ws_stream, _) = tokio_tungstenite::connect_async(&format!("ws://{WS_ADDR}"))
        .await
        .expect("failed to connect WS client");
    let mut ws = ws_stream;

    // Consume the initial (empty) snapshot every fresh connection gets.
    let initial = next_graph_message(&mut ws, Duration::from_secs(5)).await;
    assert!(matches!(initial, Some(GraphMessage::Snapshot { .. })), "expected an initial snapshot, got: {initial:?}");

    // ── 1. Spawn target #1, confirm it appears via ListProcesses ───────────
    let (target1_rx, pid1) = spawn_target(&mut procs);
    println!("target1 pid={pid1}");

    let resp = send_control(&mut ws, &ControlRequest::ListProcesses).await;
    let ControlResponse::ProcessList { processes } = resp else {
        panic!("expected ProcessList, got: {resp:?}");
    };
    assert!(
        processes.iter().any(|p| p.pid == pid1),
        "ListProcesses did not include target1 (pid={pid1}); got {} processes",
        processes.len()
    );

    // ── 2. AttachTarget(pid1) over WS — must actually populate the graph ──
    let resp = send_control(&mut ws, &ControlRequest::AttachTarget { pid: pid1 }).await;
    let ControlResponse::AttachResult { ok, message } = resp else {
        panic!("expected AttachResult, got: {resp:?}");
    };
    assert!(ok, "AttachTarget(pid1) failed: {message}");

    // The attach broadcasts an empty snapshot immediately (§3.4) even on a
    // clean first attach (nothing to clear, but the code path is uniform) —
    // drain messages until we see real nodes arrive from target1's workload.
    let mut saw_nonempty = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match next_graph_message(&mut ws, Duration::from_secs(3)).await {
            Some(msg) if node_count(&msg) > 0 => {
                saw_nonempty = true;
                break;
            }
            _ => continue,
        }
    }
    assert!(saw_nonempty, "no non-empty snapshot/diff arrived after AttachTarget(pid1) — events never reached the graph");

    // Wait for target1's own workload-done marker before switching targets.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_done = false;
    while std::time::Instant::now() < deadline {
        if let Ok(l) = target1_rx.recv_timeout(Duration::from_millis(200)) {
            if l == "WORKLOAD_DONE" {
                saw_done = true;
                break;
            }
        }
    }
    assert!(saw_done, "target1 never printed WORKLOAD_DONE");

    // ── 3. Spawn target #2, attach it while target #1 is still live ────────
    let (_target2_rx, pid2) = spawn_target(&mut procs);
    println!("target2 pid={pid2}");

    let resp = send_control(&mut ws, &ControlRequest::AttachTarget { pid: pid2 }).await;
    let ControlResponse::AttachResult { ok, message } = resp else {
        panic!("expected AttachResult for target2, got: {resp:?}");
    };
    assert!(ok, "AttachTarget(pid2) failed: {message}");

    // Evidence the graph was actually CLEARED, not merged: the very next
    // graph message(s) must include an empty snapshot (node count 0) before
    // any of target2's own nodes appear.
    let mut saw_clear = false;
    let mut saw_repopulate = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline && !saw_repopulate {
        match next_graph_message(&mut ws, Duration::from_secs(3)).await {
            Some(GraphMessage::Snapshot { nodes, .. }) if nodes.is_empty() => {
                saw_clear = true;
            }
            Some(msg) if node_count(&msg) > 0 => {
                assert!(
                    saw_clear,
                    "target2's nodes appeared before an empty snapshot was observed — \
                     graph was not cleared before repopulating (looks merged, not cleared)"
                );
                saw_repopulate = true;
            }
            _ => continue,
        }
    }
    assert!(saw_clear, "never observed an empty snapshot after AttachTarget(pid2) — graph clear-on-switch (§3.4) did not happen");
    assert!(saw_repopulate, "graph was cleared but never repopulated with target2's own events");

    // ── 4. DetachTarget — clean session end ────────────────────────────────
    let resp = send_control(&mut ws, &ControlRequest::DetachTarget).await;
    let ControlResponse::DetachResult { ok, message } = resp else {
        panic!("expected DetachResult, got: {resp:?}");
    };
    assert!(ok, "DetachTarget failed: {message}");

    // `procs` drops here — kills the daemon and both target processes.
}
