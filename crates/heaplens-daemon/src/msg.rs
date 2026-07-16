use std::sync::Arc;
use tokio::sync::{broadcast, oneshot};
use heaplens_protocol::{AllocEvent, GraphMessage, NodeDto, ProcessInfo};

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
    /// A pipe client completed its HANDSHAKE frame — informational
    /// confirmation that a session actually started for this pid (§4.4).
    TargetConnected { pid: u64, name: String },
    /// The pipe connection for `pid` closed — §4.4's target-exit detection.
    ///
    /// Carries the pid specifically (not just "the pipe closed") because a
    /// target *switch* (`TargetCmd::Attach` while one is already live)
    /// detaches the old target and immediately attaches a new one — the old
    /// target's pipe-close event is detected asynchronously by `ingest.rs`
    /// and can arrive *after* the graph loop has already moved on to
    /// tracking the new pid. Without the pid here, that late event would be
    /// indistinguishable from the new target exiting, incorrectly clearing
    /// `attached_pid` and broadcasting a spurious `TargetExited` for a
    /// target that just successfully attached and is still running.
    TargetDisconnected { pid: u64 },
}

/// Messages sent from WS control-request handling (`server.rs`) to the
/// graph task, which is the single owner of both graph state (so it can
/// clear it on a target switch, §3.4) and the "which pid is currently
/// attached" session state.
pub enum TargetCmd {
    ListProcesses { reply: oneshot::Sender<Vec<ProcessInfo>> },
    Attach { pid: u32, reply: oneshot::Sender<Result<(), String>> },
    Detach { reply: oneshot::Sender<Result<(), String>> },
}

/// Whether a `TargetDisconnected { pid }` event (see its doc comment above)
/// should be treated as "the currently-attached target exited" — i.e.
/// whether `attached_pid` should be cleared and `TargetExited` broadcast.
///
/// Pulled out as a small, pure, directly-testable function specifically
/// because the race it guards against — a *stale* disconnect for an old
/// target arriving after a switch has already moved `attached_pid` on to a
/// new one — only reproduces under real async timing in the full daemon,
/// which is not something a fast, deterministic test can reliably force.
/// Pinning the actual decision rule down here means the invariant survives
/// even if nobody manages to reproduce the timing again.
pub fn is_current_target_exit(attached_pid: Option<u32>, disconnected_pid: u64) -> bool {
    attached_pid.map(u64::from) == Some(disconnected_pid)
}

#[cfg(test)]
mod target_disconnect_tests {
    use super::is_current_target_exit;

    #[test]
    fn matching_pid_is_a_real_exit() {
        assert!(is_current_target_exit(Some(4242), 4242));
    }

    #[test]
    fn no_target_currently_attached_is_never_an_exit() {
        assert!(!is_current_target_exit(None, 4242));
    }

    #[test]
    fn stale_disconnect_for_a_pid_that_is_no_longer_attached_is_ignored() {
        // The exact race this guards against: a switch already moved
        // `attached_pid` on to a new target (9999) by the time the OLD
        // target's (4242) disconnect event is processed. Must not be
        // mistaken for the new target exiting.
        assert!(!is_current_target_exit(Some(9999), 4242));
    }

    #[test]
    fn disconnect_for_the_new_target_after_a_switch_is_still_a_real_exit() {
        assert!(is_current_target_exit(Some(9999), 9999));
    }
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
