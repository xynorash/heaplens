use heaplens_protocol::{GraphMessage, NodeDto, NodeState};
use serde_json::Value;

fn sample_node(id: u64) -> NodeDto {
    NodeDto {
        id,
        ptr: 0x2000_0000_0000 + id,
        size: 128,
        ts: 1_719_240_000_000,
        symbol: "alloc::vec::Vec::push".to_owned(),
        live: true,
        state: NodeState::Healthy,
        edges: vec![id + 100, id + 101],
    }
}

#[test]
fn snapshot_json_shape() {
    let msg = GraphMessage::Snapshot {
        ts: 1_719_240_000_000,
        nodes: vec![sample_node(1), sample_node(2)],
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let v: Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(v["type"], "snapshot", "type tag must be 'snapshot'");
    assert!(v["nodes"].is_array(), "nodes must be an array");
    assert!(v.get("add").is_none(), "snapshot must not have 'add'");
    assert!(v.get("update").is_none(), "snapshot must not have 'update'");
    assert!(v.get("remove").is_none(), "snapshot must not have 'remove'");

    let first = &v["nodes"][0];
    assert!(first["id"].is_number());
    assert!(first["ptr"].is_number());
    assert!(first["size"].is_number());
    assert!(first["ts"].is_number());
    assert!(first["symbol"].is_string());
    assert!(first["live"].is_boolean());
    assert_eq!(first["state"], "healthy");
    assert!(first["edges"].is_array());
}

#[test]
fn diff_json_shape() {
    let msg = GraphMessage::Diff {
        ts: 1_719_240_001_000,
        add:    vec![sample_node(10)],
        update: vec![sample_node(11)],
        remove: vec![100, 101],
    };
    let json = serde_json::to_string(&msg).expect("serialize");
    let v: Value = serde_json::from_str(&json).expect("parse");

    assert_eq!(v["type"], "diff", "type tag must be 'diff'");
    assert!(v["add"].is_array(),    "diff must have 'add'");
    assert!(v["update"].is_array(), "diff must have 'update'");
    assert!(v["remove"].is_array(), "diff must have 'remove'");
    assert!(v.get("nodes").is_none(), "diff must not have 'nodes'");
    assert_eq!(v["remove"][0], 100);
    assert_eq!(v["remove"][1], 101);
}

#[test]
fn nodestate_serializes_lowercase() {
    let cases = [
        (NodeState::Healthy, "healthy"),
        (NodeState::Orphan,  "orphan"),
        (NodeState::Hot,     "hot"),
        (NodeState::Freed,   "freed"),
    ];
    for (state, expected) in &cases {
        let json = serde_json::to_string(state).expect("serialize");
        assert_eq!(json, format!("\"{}\"", expected));
    }
}

#[test]
fn round_trip_snapshot() {
    let original = GraphMessage::Snapshot {
        ts: 42,
        nodes: vec![sample_node(99)],
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let decoded: GraphMessage = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, original);
}
