use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use heaplens_daemon::{
    anomaly::{sweep, StormTracker},
    config::Config,
    graph::OwnershipGraph,
    ingest, injector,
    msg::{is_current_target_exit, ConnectRequest, GraphMsg, OrphanEventRecord, StoreMsg, TargetCmd},
    procs,
    resolver::Resolver,
    server,
    store,
};
use heaplens_protocol::{ControlResponse, GraphMessage, NodeState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialise tracing with EnvFilter; default to "info".
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // 2. Load config.
    let config = Config::load();

    // 3. Open SQLite store — returns a tokio mpsc sender and a join handle for
    //    the store thread so we can wait for the final batch commit on shutdown.
    let (store_tx, store_join) = store::open(&config.db_path)?;

    // 4. Broadcast channel for WS diffs (capacity 64).
    //    The initial receiver is intentionally dropped; clients subscribe via broadcast_tx.subscribe().
    let (broadcast_tx, _) = broadcast::channel::<Arc<GraphMessage>>(64);

    // 5. Connect-request channel (WS clients request snapshot + subscription).
    let (connect_tx, mut connect_rx) = mpsc::unbounded_channel::<ConnectRequest>();

    // 5b. Target-control channel (Stage 7 §3: WS clients request process
    //     list / attach / detach; the graph task is the single owner of both
    //     graph state and "which pid is currently attached" session state).
    let (target_tx, mut target_rx) = mpsc::unbounded_channel::<TargetCmd>();

    // 5c. Control-push broadcast (Stage 7 §4.4): unprompted daemon→client
    //     notifications, currently just `TargetExited`, mirroring the
    //     existing graph-diff broadcast pattern rather than a request/reply.
    let (control_push_tx, _) = broadcast::channel::<Arc<ControlResponse>>(16);

    // 6. WS server.
    tokio::spawn(server::run(config.ws_addr.clone(), connect_tx, target_tx, control_push_tx.clone()));

    // 7. Ingest channel.
    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();

    // 8. Ingest task — clone tx before moving it.
    let tick_tx = tx.clone();
    tokio::spawn(ingest::run(config.pipe_name.clone(), tx));

    // 9. Timer task — fires GraphMsg::Tick every tick_ms milliseconds.
    let tick_ms = config.tick_ms;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(tick_ms));
        loop {
            interval.tick().await;
            if tick_tx.send(GraphMsg::Tick).is_err() {
                break;
            }
        }
    });

    // 10. Graph state.
    let mut graph = OwnershipGraph::new();
    let mut resolver = Resolver::new();
    let mut storm_tracker = StormTracker::new();
    let mut warned_sites: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // 11. Attach-session state (Stage 7 §3.4: single target at a time).
    let mut attached_pid: Option<u32> = None;

    // Observability-only state for the Flutter target-diagnostics banner
    // (see heaplens_flutter/lib/providers/target_diagnostics_provider.dart).
    // None of this is read by phi or anomaly::sweep.
    let mut events_received: u64 = 0;
    let mut target_pid: Option<u64> = None;
    let mut target_name: Option<String> = None;
    let mut last_stats_sent = std::time::Instant::now();
    const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

    // Graph loop — single-threaded owner of all graph state.
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(GraphMsg::Events(events)) => {
                    // Observability-only: counts every raw event as it
                    // arrives, before any diff-visibility filtering — so it
                    // stays accurate even for nodes born and freed within
                    // the same tick, which drain_diff never surfaces (see
                    // its doc comment). Read only by the Stats broadcast
                    // below, never by phi or anomaly::sweep.
                    events_received += events.len() as u64;
                    for ev in &events {
                        match ev.kind {
                            0 => {
                                graph.on_alloc(ev, &resolver);
                                // Storm detection on alloc events with a non-empty stack.
                                if ev.stack_len > 0
                                    && storm_tracker.record(ev.stack[0], ev.ts_nanos, &config)
                                    && warned_sites.insert(ev.stack[0])
                                {
                                    warn!(
                                        addr = ev.stack[0],
                                        "allocation storm at site 0x{:x}", ev.stack[0]
                                    );
                                }
                            }
                            1 => graph.on_dealloc(ev.ptr, ev.ts_nanos),
                            2 => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size, &resolver),
                            _ => {}
                        }
                    }
                }
                Some(GraphMsg::Symbols(syms)) => {
                    for (addr, name, is_machinery) in syms {
                        resolver.insert(addr, name, is_machinery);
                    }
                }
                Some(GraphMsg::TargetConnected { pid, name }) => {
                    info!("attach session confirmed: pid={pid} name={name}");
                    // Also the sole feed for the target-diagnostics stats
                    // banner's pid/name — master's separate Handshake
                    // message carried the identical (pid, name) pair from
                    // the same underlying frame (see msg.rs); folded in
                    // here rather than sending it twice.
                    target_pid = Some(pid);
                    target_name = Some(name);
                }
                Some(GraphMsg::TargetDisconnected { pid }) => {
                    // A pipe disconnect for the *currently tracked* pid means
                    // that target exited. The pid check (not just "a pipe
                    // closed") is load-bearing: during a target switch, the
                    // old target's pipe-close event is detected
                    // asynchronously by `ingest.rs` and can arrive after
                    // `attached_pid` has already moved on to the newly
                    // attached target. Without this check, that stale event
                    // would incorrectly clear `attached_pid` and broadcast
                    // `TargetExited` for the new target — which just
                    // attached successfully and is still running.
                    if is_current_target_exit(attached_pid, pid) {
                        attached_pid = None;
                        target_pid = None;
                        target_name = None;
                        info!("target pid={pid} exited or disconnected");
                        let _ = control_push_tx.send(Arc::new(ControlResponse::TargetExited { pid: pid as u32 }));
                    }
                }
                Some(GraphMsg::Tick) => {
                    warned_sites.clear();
                    // Anomaly sweep — returns ids of nodes whose state changed.
                    let max_ts = graph.max_ts_seen;
                    // Diagnostic only (opt-in via RUST_LOG=heaplens_daemon=debug,
                    // silent at the default "info" level, no behavior change):
                    // lets an external observer reconstruct real wall-clock tick
                    // cadence during a run, to distinguish steady ~tick_ms sweep
                    // timing from queue-backlog draining. Not part of detection.
                    debug!(max_ts, "tick");
                    storm_tracker.evict_idle(max_ts, &config);
                    let changed = sweep(graph.nodes_mut(), max_ts, &config);

                    // Observability-only: for nodes that just flipped to
                    // Orphan, pair the owner-free ts (recorded on the node
                    // by on_dealloc, at the moment it happened) with this
                    // tick's ts as the detection ts. sweep() above has
                    // already fully decided `changed` and each node's
                    // `state` — this only reads that decision, it cannot
                    // feed back into it.
                    let mut orphan_events: Vec<OrphanEventRecord> = Vec::new();
                    for &id in &changed {
                        graph.mark_updated(id);
                        if let Some(node) = graph.nodes().get(&id) {
                            if node.state == NodeState::Orphan {
                                if let Some(owner_free_ts_ns) = node.owner_free_ts {
                                    orphan_events.push(OrphanEventRecord {
                                        node_id: id,
                                        owner_free_ts_ns,
                                        orphan_detected_ts_ns: max_ts,
                                        tau_ms: config.tau_ms,
                                    });
                                }
                            }
                        }
                    }
                    if !orphan_events.is_empty() {
                        let _ = store_tx.send(StoreMsg::OrphanEvents(orphan_events));
                    }

                    let diff = graph.drain_diff(&resolver);

                    // Forward new/updated nodes to the store.
                    if let GraphMessage::Diff { ref add, ref update, .. } = diff {
                        let dtos: Vec<_> = add.iter().chain(update.iter()).cloned().collect();
                        if !dtos.is_empty() {
                            let _ = store_tx.send(StoreMsg::Nodes(dtos));
                        }
                    }

                    // Broadcast non-empty diffs to WS clients.
                    if is_non_empty_diff(&diff) {
                        let _ = broadcast_tx.send(Arc::new(diff));
                    }

                    // Observability-only: broadcast session counters on a
                    // fixed 1s cadence (decoupled from tick_ms so this is a
                    // steady heartbeat, not a per-tick spam) — the Flutter
                    // target-diagnostics banner needs this even when the
                    // graph itself is producing no diffs at all, since
                    // that's exactly the "no heap activity" case it must
                    // detect. Never read by phi or anomaly::sweep.
                    if last_stats_sent.elapsed() >= STATS_INTERVAL {
                        last_stats_sent = std::time::Instant::now();
                        let (symbols_resolved, hex_fallback) = graph.symbol_stats(&resolver);
                        let stats = GraphMessage::Stats {
                            ts: max_ts,
                            events_received,
                            symbols_resolved,
                            hex_fallback,
                            target_pid,
                            target_name: target_name.clone(),
                        };
                        let _ = broadcast_tx.send(Arc::new(stats));
                    }
                }
                None => break,
            },
            req = connect_rx.recv() => {
                if let Some(ConnectRequest { reply }) = req {
                    // Q6: subscribe BEFORE taking snapshot so no diffs are lost
                    // between the two operations (no await between them).
                    let diff_rx = broadcast_tx.subscribe();
                    let snapshot = graph.snapshot(&resolver);
                    let _ = reply.send((snapshot, diff_rx));
                }
            },
            cmd = target_rx.recv() => {
                match cmd {
                    Some(TargetCmd::ListProcesses { reply }) => {
                        // Process enumeration does per-process OpenProcess/
                        // IsWow64Process2 syscalls (procs.rs) — run it off
                        // this task so a slow system doesn't stall the graph
                        // loop's other work while it walks the process list.
                        let processes = tokio::task::spawn_blocking(procs::list_processes)
                            .await
                            .unwrap_or_default();
                        let _ = reply.send(processes);
                    }
                    Some(TargetCmd::Attach { pid, reply }) => {
                        // §3.4: clean single-target transition. Detach the
                        // old target first (if any) before touching the new
                        // one or the graph.
                        if let Some(old_pid) = attached_pid.take() {
                            if let Err(e) = injector::detach(old_pid).await {
                                warn!("detach of previous target {old_pid} before switching failed: {e}");
                                // Continue anyway — refusing to attach the
                                // new target over an imperfect old detach
                                // would strand the user with no way to
                                // switch targets at all.
                            }
                        }

                        // Clear graph state before attaching — a new
                        // process is a new address space; merging
                        // topologies across processes is nonsensical.
                        // Broadcast the empty state immediately: diffs only
                        // carry changes, so without this, already-connected
                        // clients would keep showing the old target's stale
                        // nodes until the new target's own first diff.
                        graph = OwnershipGraph::new();
                        resolver = Resolver::new();
                        storm_tracker = StormTracker::new();
                        warned_sites.clear();
                        let empty = graph.snapshot(&resolver);
                        let _ = broadcast_tx.send(Arc::new(empty));

                        match injector::attach(pid).await {
                            Ok(()) => {
                                attached_pid = Some(pid);
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(e));
                            }
                        }
                    }
                    Some(TargetCmd::Detach { reply }) => {
                        match attached_pid.take() {
                            Some(pid) => match injector::detach(pid).await {
                                Ok(()) => {
                                    let _ = reply.send(Ok(()));
                                }
                                Err(e) => {
                                    // Restore tracking so a retry (or the
                                    // next attach's own detach-old step)
                                    // can try again, rather than silently
                                    // losing track of a still-live target.
                                    attached_pid = Some(pid);
                                    let _ = reply.send(Err(e));
                                }
                            },
                            None => {
                                let _ = reply.send(Ok(())); // idempotent no-op
                            }
                        }
                    }
                    None => {}
                }
            },
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                let _ = store_tx.send(StoreMsg::Shutdown);
                break;
            },
        }
    }

    // Drop the sender so the store thread sees Disconnected (or it already
    // received Shutdown from the ctrl_c arm) and flushes its final batch.
    drop(store_tx);
    let _ = store_join.join();

    Ok(())
}

fn is_non_empty_diff(msg: &GraphMessage) -> bool {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => {
            !add.is_empty() || !update.is_empty() || !remove.is_empty()
        }
        GraphMessage::Snapshot { .. } | GraphMessage::Stats { .. } => false,
    }
}
