use std::collections::HashMap;

use heaplens_daemon::anomaly::{sweep, StormTracker};
use heaplens_daemon::config::Config;
use heaplens_daemon::graph::Node;
use heaplens_protocol::NodeState;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn test_config() -> Config {
    Config {
        tau_ms: 5,
        hot_cluster_threshold: 2,
        storm_rate_threshold: 3,
        storm_window_ms: 100,
        pipe_name: r"\\.\pipe\heaplens".to_owned(),
        tick_ms: 33,
        ws_addr: "127.0.0.1:9999".to_owned(),
        db_path: "heaplens.db".to_owned(),
    }
}

fn make_node(
    id: u64,
    ts: u64,
    live: bool,
    has_owner: bool,
    had_owner_once: bool,
    edges_out_count: usize,
) -> Node {
    Node {
        id,
        ptr: id,
        size: 64,
        ts,
        live,
        stack: [0u64; 16],
        stack_len: 0,
        owner: if has_owner { Some(9999) } else { None },
        edges_out: vec![0u64; edges_out_count],
        had_owner_once,
        state: NodeState::Healthy,
        owner_free_ts: None,
    }
}

fn single_node_map(node: Node) -> HashMap<u64, Node> {
    let mut m = HashMap::new();
    m.insert(node.id, node);
    m
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A node past the tau threshold should be tagged Orphan and its id returned.
#[test]
fn orphan_after_tau() {
    let config = test_config();
    // tau_ms = 5 → threshold_ns = 5_000_000
    // max_ts_seen = 5_000_001 → delta = 5_000_001 > 5_000_000  ✓
    let max_ts_seen = config.tau_ms * 1_000_000 + 1;
    let node = make_node(1, 0, true, false, true, 0);
    let mut nodes = single_node_map(node);

    let changed = sweep(&mut nodes, max_ts_seen, &config);

    assert_eq!(changed, vec![1]);
    assert_eq!(nodes[&1].state, NodeState::Orphan);
}

/// A node just under the tau threshold must stay Healthy.
#[test]
fn not_orphan_before_tau() {
    let config = test_config();
    // max_ts_seen = 4_999_999 → delta = 4_999_999 which is NOT > 5_000_000
    let max_ts_seen = config.tau_ms * 1_000_000 - 1;
    let node = make_node(2, 0, true, false, true, 0);
    let mut nodes = single_node_map(node);

    let changed = sweep(&mut nodes, max_ts_seen, &config);

    assert!(changed.is_empty());
    assert_eq!(nodes[&2].state, NodeState::Healthy);
}

/// A node with edges_out count above hot_cluster_threshold should be tagged Hot.
#[test]
fn hot_cluster_at_threshold() {
    let config = test_config();
    // hot_cluster_threshold = 2 → need edges_out.len() > 2 → use 3
    let node = make_node(3, 0, true, false, false, config.hot_cluster_threshold + 1);
    let mut nodes = single_node_map(node);

    let changed = sweep(&mut nodes, 0, &config);

    assert_eq!(changed, vec![3]);
    assert_eq!(nodes[&3].state, NodeState::Hot);
}

/// When a node satisfies both Orphan and Hot conditions, Orphan must win (Q4).
#[test]
fn orphan_wins_over_hot() {
    let config = test_config();
    let max_ts_seen = config.tau_ms * 1_000_000 + 1;
    // had_owner_once=true, owner=None, ts=0, edges_out > threshold
    let node = make_node(4, 0, true, false, true, config.hot_cluster_threshold + 1);
    let mut nodes = single_node_map(node);

    let changed = sweep(&mut nodes, max_ts_seen, &config);

    assert_eq!(changed, vec![4]);
    assert_eq!(nodes[&4].state, NodeState::Orphan);
}

/// A Hot node whose cluster shrinks back under threshold must return to
/// Healthy on the next sweep — the reset path is the `else` branch, not a
/// special case.
#[test]
fn hot_resets_to_healthy_when_cluster_shrinks() {
    let config = test_config();
    let node = make_node(5, 0, true, false, false, config.hot_cluster_threshold + 1);
    let mut nodes = single_node_map(node);

    // First sweep: tag Hot.
    let changed = sweep(&mut nodes, 0, &config);
    assert_eq!(changed, vec![5]);
    assert_eq!(nodes[&5].state, NodeState::Hot);

    // Cluster shrinks back under threshold.
    nodes.get_mut(&5).unwrap().edges_out = vec![0u64; config.hot_cluster_threshold];
    let changed = sweep(&mut nodes, 0, &config);
    assert_eq!(changed, vec![5]);
    assert_eq!(nodes[&5].state, NodeState::Healthy);
}

/// Recording storm_rate_threshold + 1 allocs within the window triggers storm detection.
#[test]
fn storm_tracker_detects_storm() {
    let config = test_config();
    // storm_rate_threshold = 3 → need 4 calls to exceed it
    let mut tracker = StormTracker::new();
    let addr = 0xDEAD_BEEF_u64;
    let count = config.storm_rate_threshold + 1; // 4

    let mut result = false;
    for i in 0..count {
        // Space events 1 ns apart, all within the 100 ms window.
        result = tracker.record(addr, i, &config);
    }

    assert!(result, "storm should be detected after {count} calls within window");
}

/// Old entries outside the window are evicted; after eviction a single new
/// entry should NOT exceed the threshold.
#[test]
fn storm_tracker_evicts_old() {
    let config = test_config();
    // storm_rate_threshold = 3, storm_window_ms = 100 → window_ns = 100_000_000
    let mut tracker = StormTracker::new();
    let addr = 0xCAFE_BABE_u64;
    let threshold = config.storm_rate_threshold; // 3
    let window_ns = config.storm_window_ms * 1_000_000; // 100_000_000

    // Fill window with exactly storm_rate_threshold entries (not yet storming).
    for i in 0..threshold {
        tracker.record(addr, i, &config);
    }

    // Advance time past the window so all previous entries are stale.
    let late_ts = window_ns + 1_000_001;
    let result = tracker.record(addr, late_ts, &config);

    // Only 1 entry should remain in the deque after eviction — below threshold.
    assert!(
        !result,
        "after eviction only one entry remains; should not exceed threshold"
    );
}

/// A site that stops allocating must be pruned from the map entirely once
/// its entries fall outside the storm window — otherwise the map grows
/// unbounded with every distinct call site ever seen over the daemon's
/// lifetime.
#[test]
fn storm_tracker_evict_idle_prunes_stale_sites() {
    let config = test_config();
    let window_ns = config.storm_window_ms * 1_000_000;
    let mut tracker = StormTracker::new();

    tracker.record(0xAAAA, 0, &config);
    tracker.record(0xBBBB, 0, &config);
    assert_eq!(tracker.sites.len(), 2);

    // Advance max_ts_seen well past the window without recording anything
    // new for either site.
    let max_ts_seen = window_ns + 1_000_001;
    tracker.evict_idle(max_ts_seen, &config);

    assert!(
        tracker.sites.is_empty(),
        "idle sites should be pruned from the map, got {} remaining",
        tracker.sites.len()
    );
}
