use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use heaplens_daemon::{
    anomaly::{sweep, StormTracker},
    config::Config,
    graph::OwnershipGraph,
    ingest,
    msg::{ConnectRequest, GraphMsg, StoreMsg},
    resolver::Resolver,
    server,
    store,
};
use heaplens_protocol::GraphMessage;

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

    // 6. WS server.
    tokio::spawn(server::run(config.ws_addr.clone(), connect_tx));

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

    // Graph loop — single-threaded owner of all graph state.
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(GraphMsg::Events(events)) => {
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
                            1 => graph.on_dealloc(ev.ptr),
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
                Some(GraphMsg::Tick) => {
                    warned_sites.clear();
                    // Anomaly sweep — returns ids of nodes whose state changed.
                    let max_ts = graph.max_ts_seen;
                    storm_tracker.evict_idle(max_ts, &config);
                    let changed = sweep(graph.nodes_mut(), max_ts, &config);
                    for id in changed {
                        graph.mark_updated(id);
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
        GraphMessage::Snapshot { .. } => false,
    }
}
