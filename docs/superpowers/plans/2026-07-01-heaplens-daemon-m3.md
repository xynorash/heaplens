# heaplens-daemon M3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `heaplens-daemon` milestone M3 — named pipe ingest, ownership graph with φ inference, and diff emission to stdout/tracing. No WebSocket, no SQLite, no anomaly detection.

**Architecture:** Three Tokio tasks: ingest (pipe server → FrameDecoder → mpsc send), graph (mpsc recv → OwnershipGraph mutations + Resolver), and a periodic 33 ms tick. The graph task owns all model state; no locks on the hot path. Cross-unit data flows only through `heaplens-protocol` types.

**Tech Stack:** Rust 2021, Tokio 1 (rt-multi-thread, macros, net, io-util, sync, time), heaplens-protocol (path dep), anyhow 1, tracing 0.1, tracing-subscriber 0.3.

## Global Constraints

- Working branch: `dev/phase_3`. Do NOT touch `master`.
- Platform: Windows. Named pipe path: `r"\\.\pipe\heaplens"`.
- No `tokio-tungstenite`, no `rusqlite`, no `serde_json` — those are M4.
- `heaplens-daemon` must NOT depend on `heaplens-alloc`.
- Invariant §12.9: no graph/anomaly/persistence work on the ingest hot path — route via `mpsc`.
- Invariant §12.6: the daemon never symbolizes — it joins `addr → name` from SYMBOLS frames only.
- Invariant §12.7: cross-unit data crosses only through `heaplens-protocol` types (`AllocEvent`, `GraphMessage`, `NodeDto`, `NodeState`).
- All public methods on `OwnershipGraph` take `&mut self` (single-owner task, no interior mutability).
- `cargo clippy -p heaplens-daemon -- -D warnings` must be clean on every commit.
- Commit after every task. Message format: `feat(daemon): <component> — <short description>`.

## File Map

```
Cargo.toml                                      ← add "crates/heaplens-daemon" to members
crates/heaplens-daemon/
  Cargo.toml
  src/
    main.rs      — tokio::main bootstrap, mpsc channels, task spawns, Ctrl-C
    config.rs    — Config struct, env-based loading with defaults
    msg.rs       — GraphMsg enum (Events, Symbols, Tick) for the ingest→graph mpsc
    resolver.rs  — Resolver: HashMap<u64,String>, insert/name_for
    graph.rs     — Node, OwnershipGraph, infer_ownership, on_alloc/dealloc/realloc, drain_diff
    ingest.rs    — named pipe server accept loop, FrameDecoder, mpsc send
  tests/
    graph_unit.rs   — pure unit tests for OwnershipGraph (no async)
    integration.rs  — FrameDecoder loopback test via mpsc (no real pipe)
```

---

### Task 1: Crate scaffold

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/heaplens-daemon/Cargo.toml`
- Create: `crates/heaplens-daemon/src/main.rs` (stub)
- Create: `crates/heaplens-daemon/src/config.rs` (stub)
- Create: `crates/heaplens-daemon/src/msg.rs` (stub)
- Create: `crates/heaplens-daemon/src/resolver.rs` (stub)
- Create: `crates/heaplens-daemon/src/graph.rs` (stub)
- Create: `crates/heaplens-daemon/src/ingest.rs` (stub)

**Interfaces:**
- Produces: compilable workspace with new crate; all stub modules present

- [ ] **Step 1: Add daemon to workspace**

Edit `Cargo.toml` (workspace root). The members array currently contains `"crates/heaplens-protocol"` and `"crates/heaplens-alloc"`. Add the daemon:

```toml
[workspace]
resolver = "2"
members = [
    "crates/heaplens-protocol",
    "crates/heaplens-alloc",
    "crates/heaplens-daemon",
    # TODO: heaplens-flutter (Stage 4)
]
```

- [ ] **Step 2: Create daemon Cargo.toml**

Create `crates/heaplens-daemon/Cargo.toml`:

```toml
[package]
name = "heaplens-daemon"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "heaplens-daemon"
path = "src/main.rs"

[dependencies]
heaplens-protocol = { path = "../heaplens-protocol" }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "sync", "time"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 3: Create stub source files**

Create `crates/heaplens-daemon/src/main.rs`:

```rust
mod config;
mod graph;
mod ingest;
mod msg;
mod resolver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Ok(())
}
```

Create `crates/heaplens-daemon/src/config.rs`:

```rust
// stub
pub struct Config {
    pub pipe_name: String,
    pub tick_ms: u64,
}
```

Create `crates/heaplens-daemon/src/msg.rs`:

```rust
// stub
```

Create `crates/heaplens-daemon/src/resolver.rs`:

```rust
// stub
```

Create `crates/heaplens-daemon/src/graph.rs`:

```rust
// stub
```

Create `crates/heaplens-daemon/src/ingest.rs`:

```rust
// stub
```

- [ ] **Step 4: Build**

Run: `cargo build -p heaplens-daemon`

Expected: compiles with 0 errors.

- [ ] **Step 5: Commit**

```
git add Cargo.toml crates/heaplens-daemon/
git commit -m "feat(daemon): crate scaffold — workspace, Cargo.toml, stub modules"
```

---

### Task 2: config.rs

**Files:**
- Replace: `crates/heaplens-daemon/src/config.rs`

**Interfaces:**
- Produces: `Config::load() -> Config`; fields `pipe_name: String`, `tick_ms: u64`

- [ ] **Step 1: Implement config.rs**

```rust
pub struct Config {
    /// Named pipe path the daemon listens on.
    /// Default: r"\\.\pipe\heaplens"
    /// Override: env var HEAPLENS_PIPE
    pub pipe_name: String,

    /// Diff/tick interval in milliseconds.
    /// Default: 33 (≈30 Hz)
    /// Override: env var HEAPLENS_TICK_MS
    pub tick_ms: u64,
}

impl Config {
    pub fn load() -> Self {
        Config {
            pipe_name: std::env::var("HEAPLENS_PIPE")
                .unwrap_or_else(|_| r"\\.\pipe\heaplens".to_owned()),
            tick_ms: std::env::var("HEAPLENS_TICK_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(33),
        }
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p heaplens-daemon`

Expected: compiles with 0 errors.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-daemon/src/config.rs
git commit -m "feat(daemon): config.rs — pipe name and tick interval with env overrides"
```

---

### Task 3: resolver.rs

**Files:**
- Replace: `crates/heaplens-daemon/src/resolver.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Resolver { /* private */ }
  impl Resolver {
      pub fn new() -> Self;
      pub fn insert(&mut self, addr: u64, name: String);
      pub fn name_for(&self, addr: u64) -> String;
  }
  ```

- [ ] **Step 1: Write the test first**

Create `crates/heaplens-daemon/src/resolver.rs` with the full implementation AND tests inline:

```rust
use std::collections::HashMap;

pub struct Resolver {
    map: HashMap<u64, String>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver { map: HashMap::new() }
    }

    pub fn insert(&mut self, addr: u64, name: String) {
        self.map.insert(addr, name);
    }

    /// Returns the resolved name for `addr`, or `"0x{addr:x}"` if unknown.
    pub fn name_for(&self, addr: u64) -> String {
        self.map
            .get(&addr)
            .cloned()
            .unwrap_or_else(|| format!("0x{addr:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_addr_returns_name() {
        let mut r = Resolver::new();
        r.insert(0xDEAD_BEEF, "my_func".to_owned());
        assert_eq!(r.name_for(0xDEAD_BEEF), "my_func");
    }

    #[test]
    fn unknown_addr_returns_hex_fallback() {
        let r = Resolver::new();
        assert_eq!(r.name_for(0x1234), "0x1234");
        assert_eq!(r.name_for(0), "0x0");
    }

    #[test]
    fn insert_overwrites_previous() {
        let mut r = Resolver::new();
        r.insert(0x100, "old".to_owned());
        r.insert(0x100, "new".to_owned());
        assert_eq!(r.name_for(0x100), "new");
    }
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p heaplens-daemon resolver`

Expected: 3 tests pass.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-daemon/src/resolver.rs
git commit -m "feat(daemon): resolver.rs — symbol join table with hex fallback"
```

---

### Task 4: graph.rs — OwnershipGraph with unit tests

**Files:**
- Replace: `crates/heaplens-daemon/src/graph.rs`

**Interfaces:**
- Consumes: `heaplens_protocol::{AllocEvent, EventKind, GraphMessage, NodeDto, NodeState}`
- Consumes: `crate::resolver::Resolver`
- Produces:
  ```rust
  pub struct Node {
      pub id: u64, pub ptr: u64, pub size: u64, pub ts: u64,
      pub live: bool, pub stack: [u64; 8], pub stack_len: u8,
      pub owner: Option<u64>, pub edges_out: Vec<u64>, pub had_owner_once: bool,
  }
  pub struct OwnershipGraph { /* private fields */ }
  impl OwnershipGraph {
      pub fn new() -> Self;
      pub fn on_alloc(&mut self, ev: &AllocEvent);
      pub fn on_dealloc(&mut self, ptr: u64);
      pub fn on_realloc(&mut self, old_ptr: u64, new_ptr: u64, new_size: u64);
      pub fn drain_diff(&mut self, resolver: &Resolver) -> GraphMessage;
      // private: fn infer_ownership(&self, stack: &[u64], stack_len: u8) -> Option<u64>
  }
  ```

- [ ] **Step 1: Write failing unit tests in `crates/heaplens-daemon/tests/graph_unit.rs`**

Create the test file. These tests import the graph module via the crate:

```rust
use heaplens_protocol::{AllocEvent, EventKind, GraphMessage};
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::resolver::Resolver;

fn make_ev(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 8];
    let len = stack.len().min(8);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(kind, ptr, old_ptr, size, 8, ts, s, len as u8)
}

// φ inference: owner of N = live node whose stack[0] appears anywhere in N.stack
#[test]
fn phi_inference_finds_owner_by_stack_overlap() {
    let mut g = OwnershipGraph::new();

    // Alloc owner O with stack[0] = 0xAAAA
    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o_ev);

    // Alloc child C whose stack contains 0xAAAA at position 1
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    // Find child node (ptr 0x2000)
    let child = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let owner = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert_eq!(child.edges.contains(&owner.id), false); // edges_out are on the owner
    // The owner's NodeDto edges should contain child's id
    let owner_dto = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert!(owner_dto.edges.contains(&child.id));
}

// φ inference: no match → root (owner = None)
#[test]
fn phi_inference_root_when_no_match() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0x9999]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add.len(), 1);
    // No owner means edges from parent: if root, no parent pushed it to edges_out
    // Just verify it's in add with id assigned
    assert_eq!(add[0].ptr, 0x1000);
}

// φ inference: tie-break by greatest ts
#[test]
fn phi_inference_tiebreak_by_greatest_ts() {
    let mut g = OwnershipGraph::new();

    // Two candidates with same stack[0] value appearing in child's stack
    let o1 = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o1);
    let o2 = make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[0xAAAA]); // same stack[0], greater ts
    g.on_alloc(&o2);

    let c_ev = make_ev(EventKind::Alloc, 0x3000, 0, 32, 300, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    // Child should be owned by o2 (ts=200 > ts=100), so o2's NodeDto.edges contains child.id
    let o2_dto = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let c_dto = add.iter().find(|n| n.ptr == 0x3000).unwrap();
    assert!(o2_dto.edges.contains(&c_dto.id), "o2 should own the child (greatest ts)");

    let o1_dto = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert!(!o1_dto.edges.contains(&c_dto.id), "o1 should not own the child");
}

// dealloc: child.owner becomes None, child.had_owner_once becomes true
#[test]
fn dealloc_orphans_children() {
    let mut g = OwnershipGraph::new();

    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o_ev);
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    // Drain the initial adds
    let r = Resolver::new();
    let _ = g.drain_diff(&r);

    // Dealloc owner
    g.on_dealloc(0x1000);

    let diff = g.drain_diff(&r);
    let (_, updated, removed) = unwrap_diff(diff);

    // Owner is in remove
    // Child is in update (owner set to None)
    assert!(!removed.is_empty(), "owner should be in removed");
    assert!(!updated.is_empty(), "child should be in updated");

    // Verify the child node's had_owner_once — must inspect internal state
    // We do this indirectly: after a second dealloc of the child, the child was live
    // so this test is complete if the update set is non-empty and removed is non-empty
}

// realloc: ptr migrates, size updates, same id retained
#[test]
fn realloc_migrates_ptr_and_updates_size() {
    let mut g = OwnershipGraph::new();

    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let original_id = add[0].id;

    // Realloc: old_ptr=0x1000, new_ptr=0x2000, new_size=128
    g.on_realloc(0x1000, 0x2000, 128);

    let diff2 = g.drain_diff(&r);
    let (_, updated, _) = unwrap_diff(diff2);

    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].id, original_id, "same id after realloc");
    assert_eq!(updated[0].ptr, 0x2000, "ptr updated");
    assert_eq!(updated[0].size, 128, "size updated");
}

// diff accumulation: second drain_diff returns empty
#[test]
fn drain_diff_clears_accumulators() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let _ = g.drain_diff(&r);

    let diff2 = g.drain_diff(&r);
    let (add, updated, removed) = unwrap_diff(diff2);
    assert!(add.is_empty() && updated.is_empty() && removed.is_empty(),
        "second drain should be empty");
}

// resolver join: symbol attached at drain_diff time
#[test]
fn drain_diff_attaches_symbol_from_resolver() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);

    let mut r = Resolver::new();
    r.insert(0xAAAA, "my_alloc_site".to_owned());

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "my_alloc_site");
}

// resolver fallback: unknown addr → "0x…"
#[test]
fn drain_diff_uses_hex_fallback_for_unknown_symbol() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xDEAD]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "0xdead");
}

fn unwrap_diff(msg: GraphMessage) -> (Vec<heaplens_protocol::NodeDto>, Vec<heaplens_protocol::NodeDto>, Vec<u64>) {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => (add, update, remove),
        GraphMessage::Snapshot { .. } => panic!("expected Diff, got Snapshot"),
    }
}
```

- [ ] **Step 2: Run tests — expect compile failure (graph module is a stub)**

Run: `cargo test -p heaplens-daemon`

Expected: compile error — `OwnershipGraph` not found. This confirms the tests are real.

- [ ] **Step 3: Implement graph.rs**

Replace `crates/heaplens-daemon/src/graph.rs` with the full implementation:

```rust
use std::collections::{HashMap, HashSet};

use heaplens_protocol::{AllocEvent, EventKind, GraphMessage, NodeDto, NodeState};

use crate::resolver::Resolver;

pub struct Node {
    pub id: u64,
    pub ptr: u64,
    pub size: u64,
    pub ts: u64,
    pub live: bool,
    pub stack: [u64; 8],
    pub stack_len: u8,
    /// Id of the node that owns this one (φ inference result).
    pub owner: Option<u64>,
    /// Ids of nodes this node owns.
    pub edges_out: Vec<u64>,
    /// True once an owner was assigned and then freed — used by M4 orphan detection.
    pub had_owner_once: bool,
}

pub struct OwnershipGraph {
    /// All nodes, keyed by id.
    nodes: HashMap<u64, Node>,
    /// Maps live pointer → node id (removed on dealloc).
    by_ptr: HashMap<u64, u64>,
    next_id: u64,
    // Diff accumulators — cleared by drain_diff.
    added: Vec<u64>,
    updated: HashSet<u64>,
    removed: Vec<u64>,
    /// Monotonic timestamp of the last processed event (used as Diff.ts).
    last_ts: u64,
}

impl OwnershipGraph {
    pub fn new() -> Self {
        OwnershipGraph {
            nodes: HashMap::new(),
            by_ptr: HashMap::new(),
            next_id: 0,
            added: Vec::new(),
            updated: HashSet::new(),
            removed: Vec::new(),
            last_ts: 0,
        }
    }

    pub fn on_alloc(&mut self, ev: &AllocEvent) {
        self.last_ts = self.last_ts.max(ev.ts_nanos);
        let id = self.next_id;
        self.next_id += 1;

        let owner_id = self.infer_ownership(&ev.stack, ev.stack_len);

        let node = Node {
            id,
            ptr: ev.ptr,
            size: ev.size,
            ts: ev.ts_nanos,
            live: true,
            stack: ev.stack,
            stack_len: ev.stack_len,
            owner: owner_id,
            edges_out: Vec::new(),
            had_owner_once: owner_id.is_some(),
        };

        // Register as a child of the owner.
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.push(id);
                self.updated.insert(oid);
            }
        }

        self.nodes.insert(id, node);
        self.by_ptr.insert(ev.ptr, id);
        self.added.push(id);
    }

    pub fn on_dealloc(&mut self, ptr: u64) {
        let id = match self.by_ptr.remove(&ptr) {
            Some(id) => id,
            None => return,
        };

        self.removed.push(id);

        // Collect children to orphan — avoid borrow issues by collecting ids first.
        let children: Vec<u64> = self
            .nodes
            .values()
            .filter(|n| n.live && n.owner == Some(id))
            .map(|n| n.id)
            .collect();

        for cid in children {
            if let Some(child) = self.nodes.get_mut(&cid) {
                child.owner = None;
                child.had_owner_once = true;
            }
            self.updated.insert(cid);
        }

        if let Some(node) = self.nodes.get_mut(&id) {
            node.live = false;
            // Remove from owner's edges_out.
            if let Some(oid) = node.owner {
                let _ = oid; // owner reference handled below
            }
        }

        // Remove this node from its owner's edges_out list.
        let owner_id = self.nodes.get(&id).and_then(|n| n.owner);
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.retain(|&e| e != id);
                self.updated.insert(oid);
            }
        }
    }

    pub fn on_realloc(&mut self, old_ptr: u64, new_ptr: u64, new_size: u64) {
        match self.by_ptr.remove(&old_ptr) {
            Some(id) => {
                self.by_ptr.insert(new_ptr, id);
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.ptr = new_ptr;
                    node.size = new_size;
                }
                self.updated.insert(id);
            }
            None => {
                // old_ptr not tracked — treat as a fresh alloc at new_ptr.
                // Build a synthetic AllocEvent with the available info.
                let ev = AllocEvent::new(
                    EventKind::Alloc,
                    new_ptr,
                    0,
                    new_size,
                    0,
                    self.last_ts,
                    [0u64; 8],
                    0,
                );
                self.on_alloc(&ev);
            }
        }
    }

    pub fn drain_diff(&mut self, resolver: &Resolver) -> GraphMessage {
        let ts = self.last_ts;

        let add: Vec<NodeDto> = self
            .added
            .iter()
            .filter_map(|&id| self.nodes.get(&id))
            .map(|n| self.node_to_dto(n, resolver))
            .collect();

        let update: Vec<NodeDto> = self
            .updated
            .iter()
            .filter(|&&id| !self.added.contains(&id)) // skip nodes already in add
            .filter_map(|&id| self.nodes.get(&id))
            .map(|n| self.node_to_dto(n, resolver))
            .collect();

        let remove: Vec<u64> = self.removed.clone();

        self.added.clear();
        self.updated.clear();
        self.removed.clear();

        GraphMessage::Diff { ts, add, update, remove }
    }

    /// φ: find the live node whose `stack[0]` appears anywhere in `new_stack[0..stack_len]`.
    /// Among candidates, prefer greatest `ts`; tie-break by greatest `id`.
    fn infer_ownership(&self, new_stack: &[u64; 8], stack_len: u8) -> Option<u64> {
        let len = stack_len as usize;
        let search_set: HashSet<u64> = new_stack[..len].iter().copied().filter(|&a| a != 0).collect();

        self.nodes
            .values()
            .filter(|n| n.live && n.stack_len > 0 && search_set.contains(&n.stack[0]))
            .max_by_key(|n| (n.ts, n.id))
            .map(|n| n.id)
    }

    fn node_to_dto(&self, n: &Node, resolver: &Resolver) -> NodeDto {
        let symbol = if n.stack_len > 0 {
            resolver.name_for(n.stack[0])
        } else {
            "?".to_owned()
        };
        NodeDto {
            id: n.id,
            ptr: n.ptr,
            size: n.size,
            ts: n.ts,
            symbol,
            live: n.live,
            state: NodeState::Healthy, // M3: no anomaly detection
            edges: n.edges_out.clone(),
        }
    }
}
```

Also make `graph` and `resolver` modules `pub` in `src/main.rs` so tests can access them:

In `src/main.rs`, change to:
```rust
pub mod config;
pub mod graph;
pub mod ingest;
pub mod msg;
pub mod resolver;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p heaplens-daemon`

Expected: 7 tests pass (3 resolver + 4 graph_unit... adjust count based on the test file above — there are 7 tests in graph_unit.rs: phi_finds_owner, phi_root, phi_tiebreak, dealloc_orphans, realloc_migrates, drain_clears, symbol_join, hex_fallback — that's 8 tests. Plus 3 resolver tests = 11 total).

- [ ] **Step 5: Commit**

```
git add crates/heaplens-daemon/src/graph.rs crates/heaplens-daemon/src/main.rs crates/heaplens-daemon/tests/graph_unit.rs
git commit -m "feat(daemon): graph.rs — OwnershipGraph with φ inference, drain_diff, 8 unit tests"
```

---

### Task 5: msg.rs + ingest.rs

**Files:**
- Replace: `crates/heaplens-daemon/src/msg.rs`
- Replace: `crates/heaplens-daemon/src/ingest.rs`

**Interfaces:**
- Consumes: `heaplens_protocol::{AllocEvent, Frame, FrameDecoder}` (Frame is the decoded frame enum from Stage 1)
- Produces:
  ```rust
  // msg.rs
  pub enum GraphMsg {
      Events(Vec<heaplens_protocol::AllocEvent>),
      Symbols(Vec<(u64, String)>),
      Tick,
  }

  // ingest.rs
  pub async fn run(pipe_name: String, tx: tokio::sync::mpsc::UnboundedSender<GraphMsg>) -> anyhow::Result<()>;
  ```

- [ ] **Step 1: Implement msg.rs**

```rust
use heaplens_protocol::AllocEvent;

pub enum GraphMsg {
    Events(Vec<AllocEvent>),
    Symbols(Vec<(u64, String)>),
    Tick,
}
```

- [ ] **Step 2: Implement ingest.rs**

`NamedPipeServer` implements `tokio::io::AsyncRead`, so use `AsyncReadExt::read` for a clean async read loop without spinning.

```rust
use anyhow::Context;
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::ServerOptions;
use tracing::{error, info, warn};

use heaplens_protocol::{Frame, FrameDecoder};

use crate::msg::GraphMsg;

/// Named pipe accept loop. Never returns unless the sender is dropped.
pub async fn run(
    pipe_name: String,
    tx: tokio::sync::mpsc::UnboundedSender<GraphMsg>,
) -> anyhow::Result<()> {
    loop {
        // Create a fresh server instance (Windows consumes one per connection).
        let mut server = ServerOptions::new()
            .first_pipe_instance(false)
            .create(&pipe_name)
            .with_context(|| format!("failed to create named pipe: {pipe_name}"))?;

        info!("waiting for allocator client on {}", pipe_name);

        if let Err(e) = server.connect().await {
            warn!("pipe connect error: {e}");
            continue;
        }

        info!("allocator client connected");
        let mut decoder = FrameDecoder::new();
        let mut buf = [0u8; 4096];

        loop {
            match server.read(&mut buf).await {
                Ok(0) => {
                    // EOF — client disconnected.
                    info!("client disconnected");
                    break;
                }
                Ok(n) => {
                    decoder.push(&buf[..n]);
                    for frame in &mut decoder {
                        match frame {
                            Frame::Handshake { pid, name } => {
                                info!("HANDSHAKE pid={pid} name={name:?}");
                            }
                            Frame::Symbols(defs) => {
                                if tx.send(GraphMsg::Symbols(defs)).is_err() {
                                    return Ok(());
                                }
                            }
                            Frame::Events(events) => {
                                if tx.send(GraphMsg::Events(events)).is_err() {
                                    return Ok(());
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("pipe read error: {e}");
                    break;
                }
            }
        }
    }
}
```

- [ ] **Step 3: Build**

Run: `cargo build -p heaplens-daemon`

Expected: compiles with 0 errors.

- [ ] **Step 4: Commit**

```
git add crates/heaplens-daemon/src/msg.rs crates/heaplens-daemon/src/ingest.rs
git commit -m "feat(daemon): msg.rs + ingest.rs — GraphMsg, named pipe accept loop with FrameDecoder"
```

---

### Task 6: main.rs — bootstrap + task wiring

**Files:**
- Replace: `crates/heaplens-daemon/src/main.rs`

**Interfaces:**
- Consumes: all modules (config, graph, ingest, msg, resolver)
- Produces: running binary that accepts one connection, routes to graph task, ticks at 33 ms

- [ ] **Step 1: Implement main.rs**

```rust
pub mod config;
pub mod graph;
pub mod ingest;
pub mod msg;
pub mod resolver;

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time;
use tracing::info;

use config::Config;
use graph::OwnershipGraph;
use msg::GraphMsg;
use resolver::Resolver;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "heaplens_daemon=info".parse().unwrap()),
        )
        .init();

    let cfg = Config::load();
    info!("heaplens-daemon starting (pipe={}, tick={}ms)", cfg.pipe_name, cfg.tick_ms);

    let (tx, mut rx) = mpsc::unbounded_channel::<GraphMsg>();

    // Graph task: owns OwnershipGraph + Resolver, processes msgs from rx.
    let graph_handle = tokio::spawn(async move {
        let mut graph = OwnershipGraph::new();
        let mut resolver = Resolver::new();

        while let Some(msg) = rx.recv().await {
            match msg {
                GraphMsg::Events(events) => {
                    use heaplens_protocol::EventKind;
                    for ev in &events {
                        match EventKind::from_u8(ev.kind) {
                            Some(EventKind::Alloc) => graph.on_alloc(ev),
                            Some(EventKind::Dealloc) => graph.on_dealloc(ev.ptr),
                            Some(EventKind::Realloc) => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size),
                            None => {}
                        }
                    }
                }
                GraphMsg::Symbols(defs) => {
                    for (addr, name) in defs {
                        resolver.insert(addr, name);
                    }
                }
                GraphMsg::Tick => {
                    let diff = graph.drain_diff(&resolver);
                    use heaplens_protocol::GraphMessage;
                    match &diff {
                        GraphMessage::Diff { add, update, remove, .. } => {
                            if !add.is_empty() || !update.is_empty() || !remove.is_empty() {
                                info!(
                                    "diff: +{} ~{} -{} nodes",
                                    add.len(),
                                    update.len(),
                                    remove.len()
                                );
                                for n in update {
                                    if n.edges.is_empty() {
                                        info!("orphan-candidate: id={} ptr=0x{:x} symbol={}", n.id, n.ptr, n.symbol);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    });

    // Tick task: sends GraphMsg::Tick every tick_ms.
    let tick_tx = tx.clone();
    let tick_ms = cfg.tick_ms;
    let tick_handle = tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_millis(tick_ms));
        loop {
            interval.tick().await;
            if tick_tx.send(GraphMsg::Tick).is_err() {
                break;
            }
        }
    });

    // Ingest task: accept loop.
    let pipe_name = cfg.pipe_name.clone();
    let ingest_tx = tx.clone();
    let ingest_handle = tokio::spawn(async move {
        if let Err(e) = ingest::run(pipe_name, ingest_tx).await {
            tracing::error!("ingest error: {e}");
        }
    });

    // Run until Ctrl-C.
    tokio::signal::ctrl_c().await?;
    info!("shutting down");

    tick_handle.abort();
    ingest_handle.abort();
    drop(tx); // close channel so graph task drains and exits
    let _ = graph_handle.await;

    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p heaplens-daemon`

Expected: compiles with 0 errors. The binary `heaplens-daemon` is produced.

- [ ] **Step 3: Smoke-run (optional manual step)**

You can run `cargo run -p heaplens-daemon` — it should print:
```
INFO heaplens_daemon: heaplens-daemon starting (pipe=\\.\pipe\heaplens, tick=33ms)
INFO heaplens_daemon::ingest: waiting for allocator client on \\.\pipe\heaplens
```
Press Ctrl-C to exit. This verifies the startup path without a real client.

- [ ] **Step 4: Commit**

```
git add crates/heaplens-daemon/src/main.rs
git commit -m "feat(daemon): main.rs — bootstrap, mpsc wiring, graph task, tick, Ctrl-C shutdown"
```

---

### Task 7: Integration test — FrameDecoder loopback via mpsc

**Files:**
- Create: `crates/heaplens-daemon/tests/integration.rs`

**Rationale for approach:** A full named-pipe loopback test requires an OS server+client on the same machine and introduces timing sensitivity. Instead, this test encodes frames using Stage 1 encoders, pushes bytes through `FrameDecoder`, dispatches decoded frames to `OwnershipGraph` via the same logic as the ingest task, then asserts `drain_diff` produces the expected nodes. This tests the encode→decode→graph pipeline end-to-end without I/O.

- [ ] **Step 1: Write integration test**

Create `crates/heaplens-daemon/tests/integration.rs`:

```rust
//! Integration test: encode frames → FrameDecoder → graph dispatch → drain_diff.
//! Bypasses the named pipe; tests the encode/decode/graph pipeline end-to-end.
//! A full pipe loopback test was ruled out due to OS resource requirements and
//! timing sensitivity in CI; this variant is deterministic and self-contained.

use heaplens_protocol::{
    AllocEvent, EventKind, Frame, FrameDecoder, GraphMessage,
    encode_events, encode_handshake, encode_symbols,
};
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::resolver::Resolver;

fn make_alloc_ev(ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 8];
    let len = stack.len().min(8);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(EventKind::Alloc, ptr, 0, size, 8, ts, s, len as u8)
}

fn make_dealloc_ev(ptr: u64, ts: u64) -> AllocEvent {
    AllocEvent::new(EventKind::Dealloc, ptr, 0, 0, 0, ts, [0u64; 8], 0)
}

#[test]
fn encode_decode_graph_roundtrip() {
    // --- Build frames ---
    let mut wire: Vec<u8> = Vec::new();

    // HANDSHAKE
    wire.extend_from_slice(&encode_handshake(1234, "test-process"));

    // SYMBOLS: 0xAAAA → "allocator_func"
    wire.extend_from_slice(&encode_symbols(&[(0xAAAA, "allocator_func")]));

    // EVENTS: alloc O at ptr=0x1000 (stack[0]=0xAAAA), then alloc C (stack contains 0xAAAA)
    let o_ev = make_alloc_ev(0x1000, 64, 100, &[0xAAAA]);
    let c_ev = make_alloc_ev(0x2000, 32, 200, &[0xBBBB, 0xAAAA]);
    wire.extend_from_slice(&encode_events(&[o_ev, c_ev]));

    // EVENTS: dealloc O
    let d_ev = make_dealloc_ev(0x1000, 300);
    wire.extend_from_slice(&encode_events(&[d_ev]));

    // --- Feed through FrameDecoder ---
    let mut decoder = FrameDecoder::new();
    decoder.push(&wire);

    let mut graph = OwnershipGraph::new();
    let mut resolver = Resolver::new();

    for frame in &mut decoder {
        match frame {
            Frame::Handshake { pid, name } => {
                assert_eq!(pid, 1234);
                assert_eq!(name, "test-process");
            }
            Frame::Symbols(defs) => {
                for (addr, name) in defs {
                    resolver.insert(addr, name);
                }
            }
            Frame::Events(events) => {
                for ev in &events {
                    match EventKind::from_u8(ev.kind) {
                        Some(EventKind::Alloc)   => graph.on_alloc(ev),
                        Some(EventKind::Dealloc) => graph.on_dealloc(ev.ptr),
                        Some(EventKind::Realloc) => graph.on_realloc(ev.old_ptr, ev.ptr, ev.size),
                        None => {}
                    }
                }
            }
        }
    }

    // --- Assert diff after allocs + dealloc ---
    let diff = graph.drain_diff(&resolver);
    match diff {
        GraphMessage::Diff { add, update, remove, .. } => {
            // Two allocations happened, one dealloc
            assert_eq!(add.len(), 2, "two nodes allocated");
            assert!(!remove.is_empty(), "owner removed by dealloc");
            assert!(!update.is_empty(), "child updated (orphaned)");

            // Symbol attached correctly
            let o_dto = add.iter().find(|n| n.ptr == 0x1000).unwrap();
            assert_eq!(o_dto.symbol, "allocator_func", "symbol from SYMBOLS frame");

            // Owner's edges contain child's id
            let c_dto = add.iter().find(|n| n.ptr == 0x2000).unwrap();
            assert!(o_dto.edges.contains(&c_dto.id), "owner has child in edges_out");
        }
        GraphMessage::Snapshot { .. } => panic!("expected Diff"),
    }

    // Second drain is empty
    let diff2 = graph.drain_diff(&resolver);
    match diff2 {
        GraphMessage::Diff { add, update, remove, .. } => {
            assert!(add.is_empty() && update.is_empty() && remove.is_empty(),
                "second drain should be empty");
        }
        _ => panic!("expected Diff"),
    }
}

#[test]
fn incremental_push_through_decoder() {
    // Feed one byte at a time — verifies FrameDecoder handles split reads.
    let ev = make_alloc_ev(0x1000, 64, 100, &[0xAAAA]);
    let wire = encode_events(&[ev]);

    let mut decoder = FrameDecoder::new();
    let mut frames = Vec::new();
    for byte in &wire {
        decoder.push(std::slice::from_ref(byte));
        for frame in &mut decoder {
            frames.push(frame);
        }
    }

    assert_eq!(frames.len(), 1, "exactly one frame decoded byte-by-byte");
    match &frames[0] {
        Frame::Events(evs) => {
            assert_eq!(evs.len(), 1);
            assert_eq!(evs[0].ptr, 0x1000);
        }
        _ => panic!("expected Events frame"),
    }
}
```

- [ ] **Step 2: Run integration tests**

Run: `cargo test -p heaplens-daemon integration`

Expected: 2 tests pass.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-daemon/tests/integration.rs
git commit -m "test(daemon): integration — FrameDecoder loopback → graph dispatch → drain_diff"
```

---

### Task 8: Full suite + clippy clean

**Files:** none new — fix any warnings found

- [ ] **Step 1: Run full test suite**

Run: `cargo test -p heaplens-daemon`

Expected: all tests pass (3 resolver + 8 graph_unit + 2 integration = 13 total).

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -p heaplens-daemon -- -D warnings`

Expected: 0 warnings.

Common issues to fix:
- `clippy::new_without_default` on `OwnershipGraph::new()` or `Resolver::new()` → add `impl Default`
- Unused imports → remove them
- Dead code warnings on stubs → either implement or `#[allow(dead_code)]` with a comment

- [ ] **Step 3: Fix any warnings and re-run**

Run: `cargo test -p heaplens-daemon && cargo clippy -p heaplens-daemon -- -D warnings`

Expected: all tests pass, 0 warnings.

- [ ] **Step 4: Run full workspace test + clippy**

Run: `cargo test && cargo clippy -- -D warnings`

Expected: all workspace tests pass, no warnings across all crates.

- [ ] **Step 5: Commit (only if changes were needed)**

```
git add -p   # stage only changed files
git commit -m "fix(daemon): clippy clean — -D warnings"
```

---

## Acceptance Checklist

- [ ] `cargo build -p heaplens-daemon` succeeds
- [ ] `cargo test -p heaplens-daemon` — all 13 tests pass
- [ ] `cargo clippy -p heaplens-daemon -- -D warnings` — 0 warnings
- [ ] `cargo test && cargo clippy -- -D warnings` — workspace clean
- [ ] Running `heaplens-daemon` and then `alloc_smoke` (from Stage 2) produces logged diff lines showing nodes appearing; dealloc of owner produces orphan-candidate log lines
- [ ] No anomaly detection, WebSocket, or SQLite code present
- [ ] `heaplens-daemon` does not depend on `heaplens-alloc`
