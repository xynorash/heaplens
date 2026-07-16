// h1-harness: H1 detection-latency measurement.
//
// Workload: the existing, already-validated chaos-test orphan scenario
// (crates/heaplens-alloc/examples/chaos_orphan.rs, copied verbatim from the
// dev/chaos_test worktree — not rewritten here). It allocates an owner and
// five children owned by it via phi, sleeps 300ms, frees the owner alone
// (orphaning the children), then holds a 7s heartbeat loop so the daemon
// keeps receiving events (max_ts_seen only advances on alloc, never on
// dealloc — see graph.rs's on_dealloc, deliberately unchanged).
//
// Definition (corrected — see docs/bench_results/h1_report.md for the full
// derivation): the orphan condition is a conjunction of two predicates
// (owner freed AND age > tau), so detection cannot be measured from
// owner-free alone — it must be measured from whichever of the two
// conjuncts completes *last*:
//
//   H1 = orphan_detected_ts_ns - max(owner_free_ts_ns, node_ts_ns + tau_ms*1e6)
//
// All three input timestamps are read back from the daemon's own SQLite
// tables after the run — never inferred, never captured workload-side,
// never a proxy:
//   - owner_free_ts_ns, orphan_detected_ts_ns: orphan_events table
//     (crates/heaplens-daemon/src/store.rs), written by the observability
//     change reviewed in Part 1.
//   - node_ts_ns (the child's own allocation ts): the pre-existing `nodes`
//     table's `ts` column for that node id — this is Node::ts, set once at
//     on_alloc and never mutated, so any row for that id carries the
//     correct value. No further daemon persistence change was needed for
//     this — it reuses data the daemon already stored.
//
// All three timestamps are producer-clock nanoseconds (heaplens-alloc's
// Instant::now() relative to a process-local START, captured inside the
// *same* target process for every event) — same clock domain throughout.
//
// Two configurations are run: tau < the workload's ~300ms settle-gap
// (owner-free is the binding conjunct — reports sweep-cadence-bound
// latency) and tau > the settle-gap (age-past-tau is the binding conjunct
// — reports detection-from-tau-completion latency, expected near one tick).
//
// A `diagnose` mode additionally captures the daemon's own debug-level
// per-tick log (opt-in via RUST_LOG=heaplens_daemon=debug, added as a
// small diagnostic-only daemon change) with harness-side wall-clock
// receipt timestamps, to confirm whether ticks fire on a steady ~tick_ms
// cadence or arrive as a drained backlog — load-bearing for trusting the
// tau-bound config's number as genuine sweep latency.
//
// A failed run (WS orphan diff never observed within the timeout, or an
// inconsistent/missing/unreadable persisted row) is reported as FAILED and
// excluded from the CSV — never fabricated.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

use heaplens_protocol::{GraphMessage, NodeState};

const N_RUNS: u32 = 20;
const TAU_LOW_MS: u64 = 5; // < settle-gap: owner-free is the binding conjunct
const TAU_HIGH_MS: u64 = 500; // > settle-gap: age-past-tau is the binding conjunct
const CHILD_SYMBOL: &str = "chaos_orphan::make_children";
const WATCH_TIMEOUT: Duration = Duration::from_secs(20);
const BASE_WS_PORT: u16 = 9800;

struct RunResult {
    run: u32,
    tau_ms: u64,
    node_id: u64,
    node_ts_ns: u64,
    owner_free_ts_ns: u64,
    orphan_detected_ts_ns: u64,
    /// Wall-clock gap (harness-observed) between the detecting tick's debug
    /// log line and the immediately preceding tick's log line, for THIS
    /// specific run — not a separate side measurement. A gap close to the
    /// configured tick_ms means the detecting tick fired on normal cadence;
    /// a near-zero gap would mean it fired as part of a drained backlog.
    /// `None` if the detecting tick's max_ts could not be matched in the
    /// captured log (should not happen when tick capture succeeds).
    detecting_tick_gap_ms: Option<f64>,
    ticks_captured: usize,
}

enum RunOutcome {
    Ok(RunResult),
    Failed { run: u32, tau_ms: u64, reason: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "measure".to_owned());

    let cwd = std::env::current_dir()?;
    let daemon_exe = cwd.join("target/release/heaplens-daemon.exe");
    let workload_exe = cwd.join("target/release/examples/chaos_orphan.exe");
    if !daemon_exe.exists() {
        anyhow::bail!("daemon not built: {}", daemon_exe.display());
    }
    if !workload_exe.exists() {
        anyhow::bail!("workload not built: {}", workload_exe.display());
    }

    if mode == "diagnose" {
        let tau_ms: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(TAU_HIGH_MS);
        return diagnose_tick_cadence(&daemon_exe, &workload_exe, tau_ms).await;
    }

    println!("=== H1 detection-latency measurement (orphan scenario) ===");
    println!("H1 = orphan_detected_ts_ns - max(owner_free_ts_ns, node_ts_ns + tau_ms*1e6)");
    println!("configs: tau_ms={TAU_LOW_MS} (< ~300ms settle-gap), tau_ms={TAU_HIGH_MS} (> settle-gap)");

    let mut all_outcomes: Vec<RunOutcome> = Vec::new();
    let mut port_offset: u16 = 0;
    for &tau_ms in &[TAU_LOW_MS, TAU_HIGH_MS] {
        println!("\n--- tau_ms={tau_ms} ---");
        for run in 1..=N_RUNS {
            port_offset += 1;
            let outcome = run_once(run, tau_ms, port_offset, &daemon_exe, &workload_exe).await;
            match &outcome {
                RunOutcome::Ok(r) => println!(
                    "run {run:2}: node={} node_ts_ns={} owner_free_ts_ns={} orphan_detected_ts_ns={} -> H1={:.6}ms  (ticks_captured={}, gap_before_detecting_tick={})",
                    r.node_id,
                    r.node_ts_ns,
                    r.owner_free_ts_ns,
                    r.orphan_detected_ts_ns,
                    h1_latency_ms(r),
                    r.ticks_captured,
                    r.detecting_tick_gap_ms.map(|g| format!("{g:.3}ms")).unwrap_or_else(|| "n/a".to_owned())
                ),
                RunOutcome::Failed { run, reason, .. } => println!("run {run:2}: FAILED — {reason}"),
            }
            all_outcomes.push(outcome);
        }
    }

    write_csv(&all_outcomes)?;

    println!("\n=== summary ===");
    for &tau_ms in &[TAU_LOW_MS, TAU_HIGH_MS] {
        let successes: Vec<&RunResult> = all_outcomes
            .iter()
            .filter_map(|o| match o {
                RunOutcome::Ok(r) if r.tau_ms == tau_ms => Some(r),
                _ => None,
            })
            .collect();
        let failures: usize = all_outcomes
            .iter()
            .filter(|o| matches!(o, RunOutcome::Failed { tau_ms: t, .. } if *t == tau_ms))
            .count();

        println!("\ntau_ms={tau_ms}: {} succeeded, {failures} failed (of {N_RUNS})", successes.len());
        if successes.is_empty() {
            println!("  no successful runs — nothing to report for this config");
            continue;
        }

        let binding = if tau_ms < 300 { "owner-free (sweep-cadence-bound)" } else { "age-past-tau (tau-completion-bound)" };
        println!("  expected binding conjunct: {binding}");

        let mut latencies: Vec<f64> = successes.iter().map(|r| h1_latency_ms(r)).collect();
        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = median_of(&latencies);
        println!(
            "  H1 latency (ms): median={:.4} min={:.4} max={:.4}",
            median,
            latencies.first().unwrap(),
            latencies.last().unwrap()
        );
        if latencies.iter().any(|&v| v < 0.0) {
            println!("  *** negative H1 value present — the max() fix did not eliminate the negative case; stop and report. ***");
        }

        let gaps: Vec<f64> = successes.iter().filter_map(|r| r.detecting_tick_gap_ms).collect();
        let unmatched = successes.len() - gaps.len();
        if gaps.is_empty() {
            println!("  detecting-tick gap: no runs had a matchable tick log — cannot confirm cadence for this config");
        } else {
            let mut sorted_gaps = gaps.clone();
            sorted_gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let near_zero = gaps.iter().filter(|&&g| g < 5.0).count();
            println!(
                "  detecting-tick gap (ms, per-run, this specific tick): median={:.3} min={:.3} max={:.3} over {}/{} runs ({unmatched} unmatched)",
                median_of(&sorted_gaps),
                sorted_gaps.first().unwrap(),
                sorted_gaps.last().unwrap(),
                gaps.len(),
                successes.len()
            );
            println!("  {near_zero}/{} runs show a <5ms gap before the detecting tick (backlog-drain signature)", gaps.len());
        }
    }

    Ok(())
}

async fn run_once(
    run: u32,
    tau_ms: u64,
    port_offset: u16,
    daemon_exe: &Path,
    workload_exe: &Path,
) -> RunOutcome {
    let ws_port = BASE_WS_PORT + port_offset;
    let ws_addr = format!("127.0.0.1:{ws_port}");
    let db_path = std::env::temp_dir().join(format!("heaplens_h1_tau{tau_ms}_run{run}.db"));
    let _ = std::fs::remove_file(&db_path);

    // RUST_LOG scoped to debug for THIS crate only (not deps) — captures
    // the same per-tick diagnostic line used in `diagnose` mode, but for
    // every statistical run, so cadence evidence exists per-run rather
    // than only in a separate side measurement.
    let mut daemon_child = match Command::new(daemon_exe)
        .env("HEAPLENS_DB_PATH", &db_path)
        .env("HEAPLENS_WS_ADDR", &ws_addr)
        .env("HEAPLENS_TAU_MS", tau_ms.to_string())
        .env("RUST_LOG", "heaplens_daemon=debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return RunOutcome::Failed { run, tau_ms, reason: format!("daemon spawn failed: {e}") },
    };

    let stdout = daemon_child.stdout.take().expect("piped stdout");
    let start = Instant::now();
    let (tick_tx, tick_rx) = std::sync::mpsc::channel::<(Duration, u64)>();
    let tick_task = tokio::spawn(async move {
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(max_ts) = parse_max_ts(&line) {
                let _ = tick_tx.send((start.elapsed(), max_ts));
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let ws_url = format!("ws://{ws_addr}");
    let (hit_tx, hit_rx) = oneshot::channel::<u64>();
    let ws_task = tokio::spawn(watch_ws(ws_url, hit_tx));

    let mut workload_child = match Command::new(workload_exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = daemon_child.kill().await;
            return RunOutcome::Failed { run, tau_ms, reason: format!("workload spawn failed: {e}") };
        }
    };

    let hit = timeout(WATCH_TIMEOUT, hit_rx).await;
    ws_task.abort();

    let node_id = match hit {
        Ok(Ok(id)) => id,
        Ok(Err(_)) => {
            let _ = daemon_child.kill().await;
            let _ = workload_child.kill().await;
            return RunOutcome::Failed {
                run,
                tau_ms,
                reason: "WS watch task ended without a hit (connect or decode failure)".to_owned(),
            };
        }
        Err(_) => {
            let _ = daemon_child.kill().await;
            let _ = workload_child.kill().await;
            return RunOutcome::Failed {
                run,
                tau_ms,
                reason: format!("timed out after {WATCH_TIMEOUT:?} waiting for orphan diff"),
            };
        }
    };

    // Store thread batches on a 100ms fixed interval — give it margin to
    // commit the orphan_events row before we read it back.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let _ = daemon_child.kill().await;
    let _ = daemon_child.wait().await;
    let _ = workload_child.kill().await;
    let _ = workload_child.wait().await;

    // The daemon process is dead and its stdout pipe closed, but the task
    // reading that pipe is a separate, concurrently-scheduled tokio task —
    // draining the channel before it's actually finished (it may not have
    // been polled since the last line arrived) silently loses everything
    // still in flight. Await its completion first.
    let _ = tick_task.await;
    let mut ticks: Vec<(Duration, u64)> = Vec::new();
    while let Ok(t) = tick_rx.try_recv() {
        ticks.push(t);
    }

    let result = read_orphan_event(&db_path, node_id, run, tau_ms, &ticks);
    let _ = std::fs::remove_file(&db_path);
    result
}

/// Strips ANSI SGR escape sequences (`ESC [ ... letter`). tracing_subscriber's
/// default fmt layer colors output unconditionally (no TTY auto-detection),
/// even when stdout is a pipe — so e.g. "max_ts=" is not contiguous in the
/// raw bytes (a reset/color escape is spliced between the field name and
/// the "="), even though a terminal renders it as if it were.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // consume '['
            for c2 in chars.by_ref() {
                if c2.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Extracts the `max_ts=<n>` value from a `heaplens_daemon`'s debug-level
/// "tick" log line.
fn parse_max_ts(line: &str) -> Option<u64> {
    let plain = strip_ansi(line);
    if !plain.contains("tick") {
        return None;
    }
    let idx = plain.find("max_ts=")?;
    let rest = &plain[idx + "max_ts=".len()..];
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// Wall-clock gap between the tick whose max_ts equals `detected_ts_ns` and
/// the tick immediately preceding it in `ticks` (in capture order). `None`
/// if no exact match is found or it's the first captured tick.
fn detecting_tick_gap_ms(ticks: &[(Duration, u64)], detected_ts_ns: u64) -> Option<f64> {
    let idx = ticks.iter().position(|&(_, max_ts)| max_ts == detected_ts_ns)?;
    if idx == 0 {
        return None;
    }
    let (t_prev, _) = ticks[idx - 1];
    let (t_cur, _) = ticks[idx];
    Some((t_cur.as_secs_f64() - t_prev.as_secs_f64()) * 1000.0)
}

async fn watch_ws(ws_url: String, hit_tx: oneshot::Sender<u64>) {
    let (ws_stream, _) = match tokio_tungstenite::connect_async(&ws_url).await {
        Ok(pair) => pair,
        Err(_) => return,
    };
    let (_sink, mut source) = ws_stream.split();

    while let Some(Ok(msg)) = source.next().await {
        let text = match msg {
            Message::Text(t) => t,
            _ => continue,
        };
        let Ok(GraphMessage::Diff { add, update, .. }) = serde_json::from_str(&text) else {
            continue;
        };
        for n in add.iter().chain(update.iter()) {
            if n.symbol == CHILD_SYMBOL && n.state == NodeState::Orphan {
                let _ = hit_tx.send(n.id);
                return;
            }
        }
    }
}

fn read_orphan_event(
    db_path: &PathBuf,
    node_id: u64,
    run: u32,
    tau_ms: u64,
    ticks: &[(Duration, u64)],
) -> RunOutcome {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(e) => return RunOutcome::Failed { run, tau_ms, reason: format!("could not open db: {e}") },
    };

    // The workload orphans all 5 make_children siblings in the same
    // on_dealloc call and the same subsequent sweep, so every row in
    // orphan_events this run should carry identical owner_free/detected
    // timestamps. Read all rows and require exact agreement rather than
    // picking one blindly.
    let mut stmt = match conn.prepare(
        "SELECT node_id, owner_free_ts_ns, orphan_detected_ts_ns, tau_ms FROM orphan_events",
    ) {
        Ok(s) => s,
        Err(e) => return RunOutcome::Failed { run, tau_ms, reason: format!("orphan_events query prepare failed: {e}") },
    };
    let rows: Result<Vec<(i64, i64, i64, i64)>, _> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .and_then(Iterator::collect);
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return RunOutcome::Failed { run, tau_ms, reason: format!("orphan_events query failed: {e}") },
    };
    if rows.is_empty() {
        return RunOutcome::Failed {
            run,
            tau_ms,
            reason: format!("orphan_events table empty (WS reported node {node_id} orphaned, but no row persisted)"),
        };
    }
    let (first_id, owner_free, detected, persisted_tau) = rows[0];
    for &(id, of, d, t) in &rows[1..] {
        if of != owner_free || d != detected || t != persisted_tau {
            return RunOutcome::Failed {
                run,
                tau_ms,
                reason: format!(
                    "orphan_events rows disagree: node {first_id} has ({owner_free},{detected},{persisted_tau}), node {id} has ({of},{d},{t})"
                ),
            };
        }
    }
    if persisted_tau as u64 != tau_ms {
        return RunOutcome::Failed {
            run,
            tau_ms,
            reason: format!("persisted tau_ms ({persisted_tau}) != configured tau_ms ({tau_ms})"),
        };
    }

    // node_ts_ns: reuse the pre-existing `nodes` table rather than adding
    // more daemon persistence. Node::ts is set once at on_alloc and never
    // mutated, so any row for this id carries the correct value.
    let node_ts_ns: Result<i64, _> =
        conn.query_row("SELECT ts FROM nodes WHERE id = ?1 LIMIT 1", [first_id], |r| r.get(0));
    let node_ts_ns = match node_ts_ns {
        Ok(v) => v,
        Err(e) => {
            return RunOutcome::Failed {
                run,
                tau_ms,
                reason: format!("could not read node_ts_ns for node {first_id} from nodes table: {e}"),
            }
        }
    };

    RunOutcome::Ok(RunResult {
        run,
        tau_ms,
        node_id: first_id as u64,
        node_ts_ns: node_ts_ns as u64,
        owner_free_ts_ns: owner_free as u64,
        orphan_detected_ts_ns: detected as u64,
        detecting_tick_gap_ms: detecting_tick_gap_ms(ticks, detected as u64),
        ticks_captured: ticks.len(),
    })
}

fn binding_ts_ns(r: &RunResult) -> u64 {
    let tau_bound = r.node_ts_ns + r.tau_ms * 1_000_000;
    r.owner_free_ts_ns.max(tau_bound)
}

fn h1_latency_ms(r: &RunResult) -> f64 {
    (r.orphan_detected_ts_ns as f64 - binding_ts_ns(r) as f64) / 1_000_000.0
}

fn median_of(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

fn write_csv(outcomes: &[RunOutcome]) -> anyhow::Result<()> {
    std::fs::create_dir_all("docs/bench_results")?;
    let mut out = String::from(
        "scenario,run,tau_ms,node_ts_ns,owner_free_ts_ns,orphan_detected_ts_ns,binding_ts_ns,h1_latency_ms,detecting_tick_gap_ms,ticks_captured\n",
    );
    let mut n = 0;
    for o in outcomes {
        if let RunOutcome::Ok(r) = o {
            out.push_str(&format!(
                "orphan,{},{},{},{},{},{},{:.6},{},{}\n",
                r.run,
                r.tau_ms,
                r.node_ts_ns,
                r.owner_free_ts_ns,
                r.orphan_detected_ts_ns,
                binding_ts_ns(r),
                h1_latency_ms(r),
                r.detecting_tick_gap_ms.map(|g| format!("{g:.6}")).unwrap_or_default(),
                r.ticks_captured
            ));
            n += 1;
        }
    }
    std::fs::write("docs/bench_results/h1_latency.csv", out)?;
    println!("\nwrote docs/bench_results/h1_latency.csv ({n} rows)");
    Ok(())
}

/// Runs the daemon once with debug-level tick logging enabled, harness-side
/// timestamping each "tick" log line at the moment it's read from the piped
/// stdout, to show whether ticks fire on a steady ~tick_ms cadence or as a
/// drained backlog. Prints inter-tick deltas; does not write to the CSV.
async fn diagnose_tick_cadence(daemon_exe: &Path, workload_exe: &Path, tau_ms: u64) -> anyhow::Result<()> {
    println!("=== tick cadence diagnostic (tau_ms={tau_ms}) ===");
    let ws_port = BASE_WS_PORT + 999;
    let ws_addr = format!("127.0.0.1:{ws_port}");
    let db_path = std::env::temp_dir().join("heaplens_h1_diagnose.db");
    let _ = std::fs::remove_file(&db_path);

    let mut daemon_child = Command::new(daemon_exe)
        .env("HEAPLENS_DB_PATH", &db_path)
        .env("HEAPLENS_WS_ADDR", &ws_addr)
        .env("HEAPLENS_TAU_MS", tau_ms.to_string())
        .env("RUST_LOG", "heaplens_daemon=debug")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let stdout = daemon_child.stdout.take().expect("piped stdout");
    let start = Instant::now();
    let (tick_tx, tick_rx) = std::sync::mpsc::channel::<(Duration, String)>();
    let tick_task = tokio::spawn(async move {
        let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains("tick") {
                let _ = tick_tx.send((start.elapsed(), line));
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let ws_url = format!("ws://{ws_addr}");
    let (hit_tx, hit_rx) = oneshot::channel::<u64>();
    let ws_task = tokio::spawn(watch_ws(ws_url, hit_tx));

    let mut workload_child = Command::new(workload_exe)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let hit = timeout(WATCH_TIMEOUT, hit_rx).await;
    ws_task.abort();
    let hit_at = start.elapsed();
    match &hit {
        Ok(Ok(id)) => println!("WS hit: node {id} observed orphaned at t={hit_at:?} (harness wall clock since daemon spawn)"),
        _ => println!("WS hit: none (timeout or error) — cadence data collected up to this point still reported below"),
    }

    // Let a little more log drain in, then stop everything.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = daemon_child.kill().await;
    let _ = daemon_child.wait().await;
    let _ = workload_child.kill().await;
    let _ = workload_child.wait().await;
    let _ = std::fs::remove_file(&db_path);

    let _ = tick_task.await;
    let mut ticks: Vec<(Duration, String)> = Vec::new();
    while let Ok(t) = tick_rx.try_recv() {
        ticks.push(t);
    }

    println!("\ncaptured {} tick log lines", ticks.len());
    if ticks.len() < 2 {
        println!("not enough ticks captured to compute cadence");
        return Ok(());
    }

    let mut deltas_ms: Vec<f64> = Vec::new();
    for w in ticks.windows(2) {
        let d = (w[1].0.as_secs_f64() - w[0].0.as_secs_f64()) * 1000.0;
        deltas_ms.push(d);
    }

    println!("\nfirst 10 ticks (harness-observed wall time since daemon spawn):");
    for (t, line) in ticks.iter().take(10) {
        println!("  t={t:8.3?}  {line}");
    }
    println!("\nticks around the WS hit (t={hit_at:?}):");
    for (t, line) in ticks.iter().filter(|(t, _)| t.as_secs_f64() >= hit_at.as_secs_f64() - 0.1) {
        println!("  t={t:8.3?}  {line}");
    }

    let mut sorted = deltas_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = median_of(&sorted);
    let near_zero = deltas_ms.iter().filter(|&&d| d < 5.0).count();
    println!(
        "\ninter-tick delta (ms): median={:.3} min={:.3} max={:.3}; {}/{} deltas < 5ms",
        median,
        sorted.first().unwrap(),
        sorted.last().unwrap(),
        near_zero,
        deltas_ms.len()
    );
    if near_zero > deltas_ms.len() / 4 {
        println!(
            "*** a substantial fraction of inter-tick deltas are near-zero — consistent with backlog \
             draining rather than a clean ~tick_ms cadence. Do not trust a tau-bound H1 number without \
             accounting for this. ***"
        );
    } else {
        println!("ticks are firing on a steady cadence close to the configured tick_ms — no evidence of backlog draining.");
    }

    Ok(())
}
