use heaplens_protocol::{AllocEvent, NodeDto};

/// Messages sent from the ingest task to the graph task via mpsc.
pub enum GraphMsg {
    /// A batch of allocation events decoded from a single pipe frame.
    Events(Vec<AllocEvent>),
    /// Periodic tick from the timer — triggers drain_diff and log emission.
    Tick,
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
