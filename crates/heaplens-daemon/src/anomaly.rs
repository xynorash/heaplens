use std::collections::{HashMap, VecDeque};

use heaplens_protocol::NodeState;

use crate::config::Config;
use crate::graph::Node;

/// Run one anomaly sweep over all live nodes. Returns the ids of nodes whose
/// state changed this pass.
///
/// Evaluation order per node (Q4 invariant):
///   1. Orphan — checked first; if matched, node is tagged and the hot check
///      is skipped for this node.
///   2. Hot — only evaluated when Orphan did not match.
///
/// Storm detection is handled separately via [`StormTracker::record`] and
/// does not set `NodeState`.
pub fn sweep(nodes: &mut HashMap<u64, Node>, max_ts_seen: u64, config: &Config) -> Vec<u64> {
    let mut changed: Vec<u64> = Vec::new();

    for node in nodes.values_mut() {
        // Orphan check (must come first per Q4).
        if node.live
            && node.owner.is_none()
            && node.had_owner_once
            && max_ts_seen.saturating_sub(node.ts) > config.tau_ms * 1_000_000
        {
            if node.state != NodeState::Orphan {
                node.state = NodeState::Orphan;
                changed.push(node.id);
            }
            // Q4: orphan wins — skip hot check
            continue;
        }

        // Hot-cluster check (only reached when node is not Orphan this pass).
        if node.live && node.edges_out.len() > config.hot_cluster_threshold
            && node.state != NodeState::Hot
        {
            node.state = NodeState::Hot;
            changed.push(node.id);
        }

        // TODO: reset state to Healthy when conditions no longer hold (not yet implemented — spec is silent on reset)
    }

    changed
}

/// Tracks per-call-site allocation rates to detect allocation storms.
///
/// Each entry maps `stack[0]` (the top call-site address) to a ring of
/// nanosecond timestamps that fall within the current storm window.
pub struct StormTracker {
    /// stack[0] addr → ring of ts_nanos values within the current window.
    pub sites: HashMap<u64, VecDeque<u64>>,
}

impl Default for StormTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl StormTracker {
    pub fn new() -> Self {
        StormTracker {
            sites: HashMap::new(),
        }
    }

    /// Record a new alloc at `addr` with timestamp `ts` (nanoseconds).
    ///
    /// Evicts entries older than `storm_window_ms`, then appends `ts`.
    /// Returns `true` when this site's in-window count now exceeds
    /// `storm_rate_threshold`.
    pub fn record(&mut self, addr: u64, ts: u64, config: &Config) -> bool {
        let window_ns = config.storm_window_ms * 1_000_000;
        let deque = self.sites.entry(addr).or_default();
        while let Some(&front) = deque.front() {
            if ts.saturating_sub(front) > window_ns {
                deque.pop_front();
            } else {
                break;
            }
        }
        deque.push_back(ts);
        deque.len() as u64 > config.storm_rate_threshold
    }
}
