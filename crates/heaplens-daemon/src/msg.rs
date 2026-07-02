use heaplens_protocol::AllocEvent;

/// Messages sent from the ingest task to the graph task via mpsc.
pub enum GraphMsg {
    /// A batch of allocation events decoded from a single pipe frame.
    Events(Vec<AllocEvent>),
    /// Periodic tick from the timer — triggers drain_diff and log emission.
    Tick,
}
