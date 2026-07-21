use serde::{Deserialize, Serialize};

/// A process the picker can offer as an attach target.
/// Field names are the wire contract — Flutter mirrors them verbatim.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// "x64", "x86", or "unknown" — `OpenProcess` can legitimately fail for
    /// protected/elevated processes the daemon has no rights to query;
    /// "unknown" is reported rather than dropping the process from the list.
    pub arch: String,
}

/// Client → daemon control requests, sent as WS text frames on the same
/// connection graph snapshots/diffs are sent on.
///
/// Serializes with an inline `"type"` tag:
///   `{ "type": "list_processes" }`
///   `{ "type": "attach_target", "pid": 1234 }`
///   `{ "type": "detach_target" }`
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlRequest {
    ListProcesses,
    AttachTarget { pid: u32 },
    DetachTarget,
}

/// Daemon → client control responses, sent as WS text frames alongside
/// `GraphMessage` snapshots/diffs (distinguished by their own `"type"` tag,
/// so a lenient client can `match` on either shape from the same stream).
///
/// `ProcessList`/`AttachResult`/`DetachResult` are replies to a specific
/// `ControlRequest`. `TargetExited` is different in kind — an unprompted
/// push, broadcast to every connected client the moment the daemon detects
/// the attached target process has exited (§4.4), the same way graph diffs
/// are pushed rather than polled.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResponse {
    ProcessList { processes: Vec<ProcessInfo> },
    AttachResult { ok: bool, message: String },
    DetachResult { ok: bool, message: String },
    TargetExited { pid: u32 },
}
