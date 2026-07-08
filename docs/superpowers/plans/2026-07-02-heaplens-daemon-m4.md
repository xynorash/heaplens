# heaplens-daemon M4 Implementation Plan

**Branch:** dev/phase_4  
**Base commit:** 489b5ef  
**Date:** 2026-07-02

## Goal

Complete `heaplens-daemon` to Milestone M4: anomaly detection (orphan, hot-cluster, storm),
SQLite persistence, and WebSocket server broadcasting live graph diffs and snapshots.

## Locked Design Decisions (Q1–Q6 + 3 refinements)

- **Q1 — StoreMsg carries NodeDto**: the store task receives processed `NodeDto` values from
  `drain_diff` output, never raw `AllocEvent`. Decouples persistence from the event format.
- **Q2 — 100ms batched transactions**: store task accumulates rows for 100 ms, then wraps
  them in a single `BEGIN`/`COMMIT`. One dedicated task owns the `Connection`.
- **Q3 — max_ts_seen (monotonic rolling max)**: `OwnershipGraph` tracks `max_ts_seen` as
  the rolling max of every `ev.ts_nanos` across ALL events (alloc, dealloc, realloc). This
  is the "now" for orphan age: `age = max_ts_seen - node.ts`. Never use daemon wall-clock.
  Events are NOT guaranteed ts-ordered across producer threads, so always `max()`, never assign.
- **Q4 — Orphan wins over Hot**: anomaly sweep evaluates orphan first; if a node is tagged
  `Orphan`, skip to the next node without checking hot-cluster. Comment this order in code.
- **Q5 — broadcast channel for WS diffs**: `tokio::sync::broadcast` (capacity 64) distributes
  diff `GraphMessage` values from the graph task to all connected WS clients.
- **Q6 — Atomic subscribe+snapshot**: when a WS client connects, the graph task does
  `tx.subscribe()` and builds the snapshot synchronously in the same message-loop handler
  turn. This ensures no diffs are lost between snapshot and first broadcast message. The
  graph task returns `(Snapshot, Receiver<Arc<GraphMessage>>)` to the client task.

## Global Constraints

- Rust 2021 edition, `anyhow::Result` for fallible ops, `tracing` for all logging.
- No `unwrap()`/`expect()` in non-test code (use `?` or `tracing::warn!` + continue).
- All tests live in `crates/heaplens-daemon/tests/` (integration) or inline `#[cfg(test)]`
  modules (unit).
- No `#[allow(dead_code)]` added for new public items actually in use.
- Compile after each task (`cargo build -p heaplens-daemon`); full test suite at end.
- YAGNI: implement exactly what each task specifies — no forward scaffolding.
- No magic numbers in logic — all thresholds from `Config`.
- `drain_diff` invariant: add/update/remove must be pairwise disjoint (maintained from M3).
- `NodeState::Freed` is NOT emitted in M4. Only Healthy, Orphan, Hot.
- storm detection only emits `tracing::warn!` — no NodeState change for storm.

## Task List

### Task 1 — config.rs additions

Extend `crates/heaplens-daemon/src/config.rs` with M4 fields.

**Current state:** `Config` has `pipe_name: String` and `tick_ms: u64`.

**Add these fields with defaults and env-var overrides:**

| Field                  | Type     | Default             | Env var                      |
|------------------------|----------|---------------------|------------------------------|
| `tau_ms`               | `u64`    | `5000`              | `HEAPLENS_TAU_MS`            |
| `hot_cluster_threshold`| `usize`  | `32`                | `HEAPLENS_HOT_THRESHOLD`     |
| `storm_rate_threshold` | `u64`    | `1000`              | `HEAPLENS_STORM_RATE`        |
| `storm_window_ms`      | `u64`    | `1000`              | `HEAPLENS_STORM_WINDOW_MS`   |
| `ws_addr`              | `String` | `"127.0.0.1:9999"` | `HEAPLENS_WS_ADDR`           |
| `db_path`              | `String` | `"heaplens.db"`    | `HEAPLENS_DB_PATH`           |

No new dependencies. No tests needed (config fields are trivial defaults; covered by build).

Commit: `feat(daemon): config.rs M4 fields — tau, hot threshold, storm, ws_addr, db_path`

---

### Task 2 — anomaly.rs + unit tests

Create `crates/heaplens-daemon/src/anomaly.rs`.  
Add `pub mod anomaly;` to `crates/heaplens-daemon/src/lib.rs`.

**The single public function:**

```rust
pub fn sweep(nodes: &mut HashMap<u64, Node>, max_ts_seen: u64, config: &Config) -> Vec<u64>
```

- Takes a mutable reference to the node map (not the full graph struct) and the current
  `max_ts_seen` from the graph.
- Returns a `Vec<u64>` of node ids whose state changed (caller marks them in `updated`).
- Imports: `use std::collections::HashMap; use crate::graph::Node; use crate::config::Config;`

**Sweep logic (evaluate in this exact order per node):**

1. **Orphan** (checked first, per Q4):
   - Condition: `node.live && node.owner.is_none() && node.had_owner_once`
     `&& max_ts_seen.saturating_sub(node.ts) > config.tau_ms * 1_000_000`
   - Action: `node.state = NodeState::Orphan`, push id to changed, **continue to next node**.
2. **Hot cluster** (only if not Orphan):
   - Condition: `node.live && node.edges_out.len() > config.hot_cluster_threshold`
   - Action: `node.state = NodeState::Hot`, push id to changed.
3. **Storm tracker** (separate pass before or after, using internal data structure):
   - Track per-`stack[0]` alloc counts within `storm_window_ms`. When a site's rate
     exceeds `storm_rate_threshold` allocs per `storm_window_ms`, emit
     `tracing::warn!(site = node.symbol_addr, "allocation storm detected")`.
   - No NodeState change. The storm tracker is a `HashMap<u64, VecDeque<u64>>` mapping
     `stack[0]` → ring of ts_nanos values within the window. Call
     `anomaly::record_alloc(storm_tracker, &ev, config)` on each alloc event (see Task 5).
   - For the sweep function, just check and warn — do not add fields to `Node`.

Actually, revise: the storm tracker state should live outside this function, owned by the
graph task. Add a separate `pub struct StormTracker` in `anomaly.rs`:

```rust
pub struct StormTracker {
    // stack[0] → sorted VecDeque of ts_nanos values within the window
    pub sites: HashMap<u64, VecDeque<u64>>,
}

impl StormTracker {
    pub fn new() -> Self { StormTracker { sites: HashMap::new() } }

    /// Record a new alloc. Returns true if this site is now storming.
    pub fn record(&mut self, addr: u64, ts: u64, config: &Config) -> bool {
        let window_ns = config.storm_window_ms * 1_000_000;
        let deque = self.sites.entry(addr).or_default();
        // Evict expired entries.
        while let Some(&front) = deque.front() {
            if ts.saturating_sub(front) > window_ns { deque.pop_front(); } else { break; }
        }
        deque.push_back(ts);
        deque.len() as u64 > config.storm_rate_threshold
    }
}
```

**Unit tests** in `crates/heaplens-daemon/tests/anomaly_unit.rs`:

1. `orphan_after_tau` — alloc node, set `had_owner_once=true`, `owner=None`, ts=0,
   max_ts_seen = tau+1 ns over threshold → state becomes Orphan.
2. `not_orphan_before_tau` — same but max_ts_seen just under threshold → still Healthy.
3. `hot_cluster_at_threshold` — node with `edges_out` count == threshold+1 → state Hot.
4. `orphan_wins_over_hot` — node satisfies BOTH orphan AND hot conditions → state Orphan.
5. `storm_tracker_detects_storm` — record storm_rate_threshold+1 allocs in window → returns true.
6. `storm_tracker_evicts_old` — fill window, advance time past window_ms, add one → returns false.

Tests use helper `make_node(id, ts, live, owner, had_owner_once, edges_out_count)`.

Commit: `feat(daemon): anomaly.rs — orphan/hot sweep, StormTracker, unit tests`

---

### Task 3 — store.rs + tests

Create `crates/heaplens-daemon/src/store.rs`.  
Add `pub mod store;` to `crates/heaplens-daemon/src/lib.rs`.  
Add to `Cargo.toml`: `rusqlite = { version = "0.32", features = ["bundled"] }`

**Channel message type** (add to `crates/heaplens-daemon/src/msg.rs`):

```rust
pub enum StoreMsg {
    Nodes(Vec<heaplens_protocol::NodeDto>),
    Flush,   // triggers early commit (used in tests)
    Shutdown,
}
```

**Schema** (created in `store::open`):

```sql
CREATE TABLE IF NOT EXISTS nodes (
    id      INTEGER NOT NULL,
    ptr     INTEGER NOT NULL,
    size    INTEGER NOT NULL,
    ts      INTEGER NOT NULL,
    symbol  TEXT    NOT NULL,
    state   TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ts ON nodes(ts);
```

**Public API:**

```rust
/// Open (or create) the SQLite database at `path`. Returns the sender channel.
/// Spawns the store task on the current tokio runtime.
pub fn open(path: &str) -> anyhow::Result<mpsc::UnboundedSender<StoreMsg>>
```

The store task:
1. Owns the `Connection` (not Send — run on `tokio::task::spawn_blocking` thread via
   `std::sync::mpsc` internally, OR use `rusqlite` in a dedicated `std::thread`).
   Simplest: spawn a `std::thread`, receive from `std::sync::mpsc::Receiver<StoreMsg>`,
   use a 100ms batch loop (`recv_timeout(Duration::from_millis(100))`).
2. Batch: collect all `StoreMsg::Nodes` that arrive within 100ms, then `BEGIN`/`INSERT`/`COMMIT`.
3. On `StoreMsg::Flush`: commit current batch immediately.
4. On `StoreMsg::Shutdown` or channel close: commit remaining, exit thread.

**Tests** in `crates/heaplens-daemon/tests/store_tests.rs`:

1. `store_inserts_nodes` — send 3 NodeDtos, send Flush, query `SELECT count(*) FROM nodes` →
   expect 3.
2. `store_batches_on_timer` — send 50 NodeDtos (no Flush), sleep 200ms, query → expect 50.
3. `store_shutdown_flushes` — send nodes, drop sender (triggers implicit shutdown), query → expect rows present.

Use an in-memory path (`:memory:`) for tests? No — rusqlite in-memory DBs are per-connection
and can't be queried from a second connection. Use a temp file path instead.

Commit: `feat(daemon): store.rs — SQLite persistence, 100ms batch, StoreMsg channel`

---

### Task 4 — server.rs (WebSocket)

Create `crates/heaplens-daemon/src/server.rs`.  
Add `pub mod server;` to `crates/heaplens-daemon/src/lib.rs`.  
Add to `Cargo.toml`:
```
tokio-tungstenite = "0.24"
futures-util = "0.3"
```
Also add `tokio` features: `signal` already present; ensure `sync` is there.

**Connect-request channel** (add to `msg.rs`):

```rust
use tokio::sync::broadcast;
use heaplens_protocol::GraphMessage;

pub struct ConnectRequest {
    /// Oneshot reply: graph task sends (snapshot, diff_receiver) back.
    pub reply: tokio::sync::oneshot::Sender<(GraphMessage, broadcast::Receiver<std::sync::Arc<GraphMessage>>)>,
}
```

**Public API:**

```rust
/// Spawn the WebSocket server task.
/// `connect_tx`: channel to request snapshot+subscription from the graph task.
pub async fn run(
    addr: String,
    connect_tx: mpsc::UnboundedSender<ConnectRequest>,
)
```

**Per-client task logic:**

1. Accept TCP connection, upgrade to WebSocket via `tokio_tungstenite::accept_async`.
2. Send a `ConnectRequest` through `connect_tx` with a oneshot reply channel.
3. Await the reply: `(snapshot, mut diff_rx)`.
4. Serialize snapshot as JSON, send as `Message::Text`.
5. Loop: `diff_rx.recv()` → serialize → send. On `RecvError::Lagged`, log a warning
   (client was too slow; some diffs dropped — this is acceptable). On WS send error or
   `RecvError::Closed`, exit the per-client loop.

No snapshot/diff logic in `server.rs` — it only serializes and sends what the graph task provides.

No unit tests for server.rs in this task (covered by async WS tests in Task 7).

Commit: `feat(daemon): server.rs — WebSocket accept, per-client task, connect-request channel`

---

### Task 5 — graph.rs extensions

Extend `crates/heaplens-daemon/src/graph.rs`. No new files.

**Add to `OwnershipGraph`:**

```rust
/// Rolling max of ev.ts_nanos across all received events.
pub max_ts_seen: u64,
```

Initialize to 0 in `new()`.  
Update in `on_alloc`, `on_dealloc`, `on_realloc`: `self.max_ts_seen = self.max_ts_seen.max(ev.ts_nanos);`  
For `on_dealloc(ptr)` — no `ev` struct; derive ts from the freed node's ts (acceptable since
dealloc events don't carry ts in the current protocol — the freed node's ts is available).
Actually: `on_dealloc` doesn't have a ts parameter. Keep `max_ts_seen` update only in
`on_alloc` and `on_realloc` (which have `AllocEvent` structs with `ts_nanos`). Dealloc
events carry no ts. Document this limitation.

**Rename `last_ts` → `max_ts_seen`** (the field existed as `last_ts` in M3; rename it).
Update `drain_diff` to use `self.max_ts_seen` as the diff `ts`.

**Add broadcast sender to `OwnershipGraph`** (or keep it external in graph task state):  
Keep the broadcast sender OUTSIDE the graph struct — it's wiring, not model state.
The graph task (in `main.rs`) owns the `broadcast::Sender<Arc<GraphMessage>>`.

**Add `anomaly::sweep` call in Tick handling** (in graph task, not graph struct):

In the graph task loop, on `GraphMsg::Tick`:
1. Call `anomaly::sweep(&mut graph.nodes, graph.max_ts_seen, &config)` → get changed ids.
2. Insert changed ids into `graph.updated` (via a new `pub fn mark_updated(&mut self, id: u64)`
   method — or make `updated` pub for the task).
3. Call `graph.drain_diff(&resolver)` → `diff`.
4. Forward `diff.add` + `diff.update` NodeDtos to store via `store_tx.send(StoreMsg::Nodes(...))`.
5. Wrap diff in `Arc`, broadcast via `broadcast_tx.send(Arc::new(diff))`.

**Add connect-request handling in graph task loop** (in `main.rs` Task 6, but requires
graph to expose a snapshot method):

Add to `OwnershipGraph`:
```rust
pub fn snapshot(&self, resolver: &Resolver) -> GraphMessage {
    let nodes: Vec<NodeDto> = self.nodes.values()
        .filter(|n| n.live)
        .map(|n| Self::node_to_dto(n, resolver))
        .collect();
    GraphMessage::Snapshot { ts: self.max_ts_seen, nodes }
}
```

**Mark `updated` pub** (or add `pub fn mark_updated`):
Add `pub fn mark_updated(&mut self, id: u64) { self.updated.insert(id); }`

**Node struct** — add `state: NodeState` field:
```rust
pub state: NodeState,  // default Healthy
```
Initialize to `NodeState::Healthy` in `on_alloc`.  
Update `node_to_dto` to use `n.state.clone()` instead of hardcoded `NodeState::Healthy`.

**Existing tests:** none of the 9 graph_unit tests rely on `state` being Healthy specifically
(they check structure, not state) — verify by reading the tests before changing.
Actually `drain_diff_attaches_symbol_from_resolver` and others don't check state.
`dealloc_orphans_children` checks `had_owner_once` and `owner` — not state. Safe to add field.

Commit: `feat(daemon): graph.rs — max_ts_seen, NodeState field, snapshot(), mark_updated()`

---

### Task 6 — main.rs wiring

Rewrite `crates/heaplens-daemon/src/main.rs` to integrate store, WS server, broadcast, and
connect-request handling.

**New structure:**

```rust
use std::sync::Arc;
use tokio::sync::{mpsc, broadcast, oneshot};
use heaplens_daemon::{anomaly, config::Config, graph::OwnershipGraph, ingest, msg::{GraphMsg, StoreMsg, ConnectRequest}, resolver::Resolver, server, store};
use heaplens_protocol::GraphMessage;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. tracing (same as M3)
    // 2. Config::load()
    // 3. store::open(&config.db_path) → store_tx
    // 4. broadcast::channel::<Arc<GraphMessage>>(64) → (broadcast_tx, _)
    //    (the receiver is dropped here; subscribers are created per-client via broadcast_tx.subscribe())
    // 5. mpsc::unbounded_channel::<ConnectRequest>() → (connect_tx, mut connect_rx)
    // 6. tokio::spawn(server::run(config.ws_addr.clone(), connect_tx))
    // 7. mpsc::unbounded_channel::<GraphMsg>() → (tx, mut rx)
    // 8. tokio::spawn(ingest::run(config.pipe_name.clone(), tx.clone()))
    // 9. Timer task (same as M3, sends GraphMsg::Tick)
    // 10. StormTracker::new()
    // 11. Graph loop with tokio::select! over rx.recv(), connect_rx.recv(), ctrl_c:
    //
    //     GraphMsg::Events(events) → for ev in events: on_alloc/dealloc/realloc,
    //         storm_tracker.record(ev.stack[0], ev.ts_nanos, &config) for alloc events
    //     GraphMsg::Tick →
    //         let changed = anomaly::sweep(&mut graph.nodes, graph.max_ts_seen, &config);
    //         for id in changed { graph.mark_updated(id); }
    //         let diff = graph.drain_diff(&resolver);
    //         // Forward to store
    //         let all_dtos: Vec<_> = match &diff { GraphMessage::Diff { add, update, .. } => add.iter().chain(update.iter()).cloned().collect(), _ => vec![] };
    //         if !all_dtos.is_empty() { let _ = store_tx.send(StoreMsg::Nodes(all_dtos)); }
    //         // Broadcast (only non-empty diffs)
    //         if is_non_empty_diff(&diff) {
    //             let _ = broadcast_tx.send(Arc::new(diff));
    //         }
    //     ConnectRequest { reply } →
    //         let snapshot = graph.snapshot(&resolver);
    //         let rx = broadcast_tx.subscribe();
    //         let _ = reply.send((snapshot, rx));
    //     None → break (channel closed)
    //
    // Helper: fn is_non_empty_diff(msg: &GraphMessage) -> bool
}
```

The `storm_tracker.record(...)` result (bool) should log a warning when true:
`tracing::warn!(addr = ev.stack[0], "allocation storm at site 0x{:x}", ev.stack[0]);`
Only log once per storm site per tick to avoid spam — add a `HashSet<u64>` of warned sites
that resets each Tick.

Commit: `feat(daemon): main.rs — store + broadcast + WS server + connect-request wiring`

---

### Task 7 — async WS tests

Create `crates/heaplens-daemon/tests/ws_tests.rs`.

These tests spin up the daemon components in-process (no child process), connect a WS client,
and verify the JSON contract.

**Setup helper (shared):**

```rust
async fn start_daemon(config: Config) -> (mpsc::UnboundedSender<GraphMsg>, SocketAddr) {
    // Creates broadcast + connect channels
    // Spawns server::run
    // Returns (graph_msg_tx, bound_addr)
    // The test drives the graph loop inline (not spawned) or via a spawned task
}
```

Actually: the cleanest approach is to test the server and graph loop together via a spawned
task that runs the graph loop (accepting GraphMsg + ConnectRequest). No store needed in these
tests (pass a fake/dropped store_tx).

**Tests:**

1. `ws_snapshot_has_type_snapshot` — connect WS client, send no graph events, receive first
   JSON message, deserialize as `GraphMessage` → assert `type == "snapshot"`.
2. `ws_diff_after_alloc` — connect WS client, receive snapshot, then inject an alloc event
   via `graph_tx`, send a Tick → receive next WS message, assert `type == "diff"` and
   `add.len() == 1`.
3. `ws_snapshot_then_diff_consistent` — alloc 3 nodes before client connects, connect client,
   receive snapshot (nodes: 3), inject dealloc of one + Tick, receive diff
   (remove: [id]) → verify remove id was in snapshot.nodes.
4. `ws_two_clients_both_get_snapshot` — connect two clients, both receive snapshot messages.

All tests bind to `127.0.0.1:0` (OS-assigned port). Use `tokio-tungstenite`'s client connect.

Add `tokio-tungstenite` is already a dependency (added in Task 4). Tests need it for the
client side.

Commit: `test(daemon): ws_tests — snapshot/diff JSON contract, two-client broadcast`

---

### Task 8 — cross-process extension

Extend `crates/heaplens-daemon/tests/cross_process_wire.rs` to verify that a WS client
receives anomaly-annotated diffs while `wire_producer` runs.

**Existing test** (`cross_process_wire.rs`): spawns `wire_producer`, starts daemon in-process,
counts 202 nodes. M4 extension:

1. Add a second assertion path: after the wire_producer's allocs complete and the daemon has
   processed them, connect a WS client to the daemon's WS server and receive a snapshot.
   Assert:
   - snapshot type is "snapshot"
   - snapshot contains ≥ 100 nodes (same threshold as M3 node count assertion)
   - at least one node has `state != "healthy"` OR the test only asserts snapshot structure
     (anomaly state depends on timing; assert structure only).

Keep the existing node-count and edge-count assertions. Add the WS snapshot assertion
as an additional block after the existing assertions.

Only modify the existing `cross_process_wire.rs` — no new files.

Commit: `test(daemon): cross_process_wire — WS snapshot assertion added`

---

### Task 9 — full suite + clippy clean

Run:
1. `cargo test -p heaplens-daemon --all-targets` — all tests must pass.
2. `cargo clippy -p heaplens-daemon -- -D warnings` — zero warnings.
3. `cargo test --workspace` — full workspace must pass.

Fix any clippy warnings or test failures. Apply `#[allow(dead_code)]` only where explicitly
justified (e.g., `StoreMsg::Flush` used only in tests). Add `Default` impls for any new
structs missing them if clippy requires. Use `by_ref()` on iterators where clippy flags
`while_let_on_iterator`.

Commit: `chore(daemon): M4 clippy clean + test suite green`

---

## Implementation Order

1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9

Tasks 1–4 are mostly independent (config, anomaly, store, server are separate files with
minimal coupling). Task 5 (graph.rs) depends on Task 2 (anomaly.rs API). Task 6 (main.rs)
depends on Tasks 3–5. Tasks 7–8 depend on Task 6. Task 9 is the final sweep.

## Acceptance Criteria

- `cargo test -p heaplens-daemon --all-targets` passes (≥ 15 existing + ≥ 12 new tests).
- `cargo clippy -p heaplens-daemon -- -D warnings` clean.
- `cargo test --workspace` passes.
- WS client at `ws://127.0.0.1:9999` receives `{"type":"snapshot",...}` then `{"type":"diff",...}` diffs.
- SQLite `heaplens.db` is created and populated when the daemon processes events.
- Orphan nodes appear in diffs with `state: "orphan"` after `tau_ms` elapses.
- Hot-cluster nodes appear with `state: "hot"` when `edges_out.len() > hot_cluster_threshold`.
