use std::collections::{HashMap, VecDeque};

use heaplens_protocol::NodeState;

use crate::config::Config;
use crate::graph::Node;

/// Run one anomaly sweep over all live nodes. Returns the ids of nodes whose
/// state changed this pass.
///
/// Every live node is re-evaluated against the full predicate chain each
/// sweep (Q4 invariant):
///   1. Orphan — checked first; wins if matched.
///   2. Hot — only evaluated when Orphan did not match.
///   3. Otherwise — Healthy. This is the reset path: a node whose Hot
///      predicate no longer holds (e.g. its cluster shrank back under
///      threshold) returns to Healthy on the next sweep. Orphan is
///      effectively sticky not because state is frozen, but because its
///      predicate (`owner.is_none() && had_owner_once`, age monotonically
///      increasing) cannot become false once true.
///
/// Storm detection is handled separately via [`StormTracker::record`] and
/// does not set `NodeState`.
pub fn sweep(nodes: &mut HashMap<u64, Node>, max_ts_seen: u64, config: &Config) -> Vec<u64> {
    let mut changed: Vec<u64> = Vec::new();

    for node in nodes.values_mut() {
        if !node.live {
            continue;
        }

        let is_orphan = node.owner.is_none()
            && node.had_owner_once
            && max_ts_seen.saturating_sub(node.ts) > config.tau_ms * 1_000_000;
        let is_hot = node.edges_out.len() > config.hot_cluster_threshold;

        let new_state = if is_orphan {
            NodeState::Orphan
        } else if is_hot {
            NodeState::Hot
        } else {
            NodeState::Healthy
        };

        if node.state != new_state {
            node.state = new_state;
            changed.push(node.id);
        }
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

    /// Evict sites whose entries have all fallen outside the storm window
    /// relative to `max_ts_seen`. Call once per Tick so the site map does not
    /// grow unbounded with every distinct allocation site ever seen — sites
    /// that stop allocating are pruned instead of retained forever.
    pub fn evict_idle(&mut self, max_ts_seen: u64, config: &Config) {
        let window_ns = config.storm_window_ms * 1_000_000;
        self.sites.retain(|_, deque| {
            while let Some(&front) = deque.front() {
                if max_ts_seen.saturating_sub(front) > window_ns {
                    deque.pop_front();
                } else {
                    break;
                }
            }
            !deque.is_empty()
        });
    }
}
