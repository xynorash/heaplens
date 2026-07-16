use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};
use heaplens_protocol::{AllocEvent, GraphMessage, NodeDto};

/// Messages sent from the ingest task to the graph task via mpsc.
pub enum GraphMsg {
    /// A batch of allocation events decoded from a single pipe frame.
    Events(Vec<AllocEvent>),
    /// (addr, resolved name, is_machinery) tuples from a SYMBOLS frame — the
    /// writer resolves and classifies addresses off its own hot path; the
    /// daemon just records the classification into its Resolver.
    Symbols(Vec<(u64, String, bool)>),
    /// Periodic tick from the timer — triggers drain_diff and log emission.
    Tick,
}

/// Request sent to the graph task when a new WebSocket client connects.
/// The graph task replies with a snapshot and a broadcast receiver for diffs.
pub struct ConnectRequest {
    pub reply: oneshot::Sender<(GraphMessage, broadcast::Receiver<Arc<GraphMessage>>)>,
}

/// One node's orphan-state transition, captured at the instant the
/// transition happens. Both timestamps come straight from data the daemon
/// already has in hand — `owner_free_ts_ns` from the dealloc event that
/// orphaned the node (`AllocEvent::ts_nanos`, threaded through
/// `OwnershipGraph::on_dealloc`), `orphan_detected_ts_ns` from
/// `OwnershipGraph::max_ts_seen` at the tick where `anomaly::sweep` flipped
/// the node's state. Neither is inferred or measured wall-clock side —
/// this exists purely so H1 (detection-latency measurement) has a real
/// pair of timestamps to difference, instead of a workload-side proxy.
pub struct OrphanEventRecord {
    pub node_id: u64,
    pub owner_free_ts_ns: u64,
    pub orphan_detected_ts_ns: u64,
    pub tau_ms: u64,
}

/// Messages sent to the store task.
pub enum StoreMsg {
    /// A batch of node snapshots to persist.
    Nodes(Vec<NodeDto>),
    /// A batch of orphan-transition events to persist.
    OrphanEvents(Vec<OrphanEventRecord>),
    /// Trigger an early commit of the current batch (used in tests).
    Flush,
    /// Commit remaining batch and exit the store thread.
    Shutdown,
}
