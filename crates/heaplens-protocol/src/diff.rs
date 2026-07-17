use serde::{Deserialize, Serialize};

/// Node lifecycle state. Serializes as lowercase: "healthy" | "orphan" | "hot" | "freed".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeState {
    Healthy,
    Orphan,
    Hot,
    Freed,
}

/// Single node in the ownership graph.
/// Field names are the wire contract — Flutter mirrors them verbatim.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NodeDto {
    pub id:     u64,
    pub ptr:    u64,
    pub size:   u64,
    pub ts:     u64,
    pub symbol: String,
    pub live:   bool,
    pub state:  NodeState,
    pub edges:  Vec<u64>,
}

/// Discriminated union for the daemon→Flutter message shapes.
///
/// Serializes with an inline `"type"` tag:
///   Snapshot → `{ "type": "snapshot", "ts": …, "nodes": […] }`
///   Diff     → `{ "type": "diff",     "ts": …, "add": […], "update": […], "remove": […] }`
///   Stats    → `{ "type": "stats",    "ts": …, "events_received": …, "symbols_resolved": …, "hex_fallback": …, "target_pid": …, "target_name": … }`
///
/// NOTE: `u64` fields serialize as bare JSON numbers. This is safe for the
/// Dart VM (Flutter desktop/native on Windows) where `int` is 64-bit.
/// It is NOT safe under dart2js / Flutter web (IEEE-754 doubles, max 2^53).
/// If the project retargets Flutter web, ptr/id/ts must become JSON strings.
///
/// Lenient deserialization (no `deny_unknown_fields`) is deliberate: forward-
/// compatible additions to NodeDto must not break older deserializers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GraphMessage {
    Snapshot { ts: u64, nodes: Vec<NodeDto> },
    Diff     { ts: u64, add: Vec<NodeDto>, update: Vec<NodeDto>, remove: Vec<u64> },
    /// Observability-only, periodic session counters — never consulted by
    /// phi or anomaly detection, purely for the Flutter target-diagnostics
    /// banner to classify "capturing / no events / no edges / unsymbolized".
    /// `target_pid`/`target_name` are `None` until the daemon has received a
    /// HANDSHAKE frame from the attached target.
    Stats {
        ts: u64,
        events_received: u64,
        symbols_resolved: u64,
        hex_fallback: u64,
        target_pid: Option<u64>,
        target_name: Option<String>,
    },
}
