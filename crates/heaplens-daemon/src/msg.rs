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

/// Messages sent to the store task.
pub enum StoreMsg {
    /// A batch of node snapshots to persist.
    Nodes(Vec<NodeDto>),
    /// Trigger an early commit of the current batch (used in tests).
    Flush,
    /// Commit remaining batch and exit the store thread.
    Shutdown,
}
