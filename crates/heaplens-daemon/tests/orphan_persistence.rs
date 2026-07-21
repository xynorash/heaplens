use std::time::Duration;

use heaplens_daemon::anomaly::sweep;
use heaplens_daemon::config::Config;
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::msg::{OrphanEventRecord, StoreMsg};
use heaplens_daemon::resolver::Resolver;
use heaplens_daemon::store;
use heaplens_protocol::{AllocEvent, EventKind, NodeState};

fn make_ev(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 16];
    let len = stack.len().min(16);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(kind, ptr, old_ptr, size, 8, ts, s, len as u8)
}

fn temp_db_path(name: &str) -> String {
    format!("{}\\heaplens_test_{}.db", std::env::temp_dir().display(), name)
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
}

/// End-to-end proof of the observability plumbing added for H1: drives a
/// real owner-free -> orphan sequence through `OwnershipGraph::on_dealloc`
/// and `anomaly::sweep` (the exact same calls main.rs's Tick handler makes),
/// replicates that handler's small glue step that builds an
/// `OrphanEventRecord`, and confirms what lands in SQLite is *exactly* the
/// injected event timestamps — not a derived, approximated, or wall-clock
/// value. Also checks the H1 latency formula against numbers chosen to
/// mirror the real chaos_orphan.rs shape: child allocated first, owner
/// freed ~300ms later (standing in for chaos_orphan's settle-sleep before
/// freeing), detection observed roughly one tick after that.
#[tokio::test]
async fn orphan_transition_persists_real_event_timestamps() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let mut r = Resolver::new();
    r.insert(owner_site, "owner_site".to_owned(), false);
    r.insert(leaf_site, "leaf_site".to_owned(), false);

    let config = Config {
        tau_ms: 5, // 5ms -> 5_000_000 ns
        hot_cluster_threshold: 32,
        storm_rate_threshold: 1000,
        storm_window_ms: 1000,
        pipe_name: r"\\.\pipe\heaplens".to_owned(),
        tick_ms: 33,
        ws_addr: "127.0.0.1:9999".to_owned(),
        db_path: "heaplens.db".to_owned(),
    };

    let mut g = OwnershipGraph::new();

    // Owner allocated at ts=0, child allocated at ts=1, owned by it via phi.
    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 0, &[owner_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 32, 1, &[leaf_site, owner_site]), &r);
    let child_id = g.node_by_ptr(0x2000).expect("child registered").id;

    // Owner freed 300ms later — the real dealloc event's ts_nanos, exactly
    // what H1 must measure from.
    const OWNER_FREE_TS_NS: u64 = 300_000_000;
    g.on_dealloc(0x1000, OWNER_FREE_TS_NS);

    assert_eq!(
        g.node_by_ptr(0x2000).expect("child still live").owner_free_ts,
        Some(OWNER_FREE_TS_NS),
        "on_dealloc must record the real event ts on the orphaned child"
    );

    // A later alloc (standing in for chaos_orphan.rs's post-free heartbeat
    // loop) advances max_ts_seen past owner_free_ts + tau — this is what
    // actually gates detection, since on_dealloc deliberately does not
    // touch max_ts_seen (unchanged behavior).
    const ORPHAN_DETECTED_TS_NS: u64 = 333_000_000; // ~one 33ms tick later
    g.on_alloc(&make_ev(EventKind::Alloc, 0x3000, 0, 8, ORPHAN_DETECTED_TS_NS, &[0xCCCC]), &r);

    let max_ts = g.max_ts_seen;
    assert_eq!(max_ts, ORPHAN_DETECTED_TS_NS);

    // Exactly the sweep() call main.rs's Tick handler makes.
    let changed = sweep(g.nodes_mut(), max_ts, &config);
    assert_eq!(changed, vec![child_id], "child should be the only node whose state changed");
    assert_eq!(g.nodes().get(&child_id).unwrap().state, NodeState::Orphan);

    // Exactly the glue main.rs's Tick handler runs after sweep().
    let mut orphan_events = Vec::new();
    for &id in &changed {
        if let Some(node) = g.nodes().get(&id) {
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
    assert_eq!(orphan_events.len(), 1);

    // Persist through the real store thread, exactly as production does.
    let path = temp_db_path("orphan_persistence");
    cleanup(&path);
    let (store_tx, _handle) = store::open(&path).unwrap();
    store_tx.send(StoreMsg::OrphanEvents(orphan_events)).unwrap();
    store_tx.send(StoreMsg::Flush).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let conn = rusqlite::Connection::open(&path).unwrap();
    let (persisted_node_id, persisted_owner_free_ts_ns, persisted_orphan_detected_ts_ns, persisted_tau_ms): (
        i64,
        i64,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT node_id, owner_free_ts_ns, orphan_detected_ts_ns, tau_ms FROM orphan_events",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();

    assert_eq!(persisted_node_id as u64, child_id);
    assert_eq!(persisted_owner_free_ts_ns as u64, OWNER_FREE_TS_NS);
    assert_eq!(persisted_orphan_detected_ts_ns as u64, ORPHAN_DETECTED_TS_NS);
    assert_eq!(persisted_tau_ms as u64, config.tau_ms);

    // The H1 formula itself, computed from what actually landed in SQLite.
    let latency_ns = persisted_orphan_detected_ts_ns - persisted_owner_free_ts_ns - (persisted_tau_ms * 1_000_000);
    const EXPECTED_LATENCY_NS: i64 = 28_000_000; // 333ms - 300ms - 5ms
    assert_eq!(latency_ns, EXPECTED_LATENCY_NS);

    cleanup(&path);
}
