# HeapLens — Technical Build Specification

**Audience:** Claude Code (autonomous development).
**Goal:** Build HeapLens end-to-end, module by module, with strict separation of concerns.
**Platform:** Windows. **Languages:** Rust (system + daemon), Dart/Flutter (visualization).

> Read this whole document before writing code. Build in the order given in §9. Never violate the invariants in §12.

---

## 0. How to use this document

The system is split into three deployable units plus one shared contract crate. Each unit is a **separate concern** and must not reach into another's internals. The only things they share are the wire contracts defined in §3. Build the contract first, then the producer, then the consumer, then the UI.

```
heaplens-protocol   ← shared contract (data only, zero logic)
      │
      ├── heaplens-alloc    ← concern: interception + transport-out
      └── heaplens-daemon   ← concern: aggregation + modeling + detection + serving
                  │
                  └── heaplens-flutter ← concern: visualization (no Rust knowledge)
```

---

## 1. Architecture and separation of concerns

### 1.1 The four units and their single responsibilities

| Unit | Single responsibility | Must NOT know about |
|------|----------------------|---------------------|
| `heaplens-protocol` | Define shared data types and wire formats. Zero behavior. | Anything. It is pure data + (de)serialization. |
| `heaplens-alloc` | Intercept every (de/re)allocation and ship raw events off-process without blocking. | Graphs, ownership, anomalies, rendering. |
| `heaplens-daemon` | Ingest events, build the ownership graph, detect anomalies, persist, broadcast diffs. | How the allocator captures events; how Flutter renders. |
| `heaplens-flutter` | Render the graph in real time and expose controls. | Rust, allocators, the daemon's internals. |

### 1.2 The seams (the only coupling points)

There are exactly **two** coupling contracts. Everything else is private.

1. **Binary frame protocol** (`heaplens-alloc` → `heaplens-daemon`), defined in `heaplens-protocol` (§3.1–3.3).
2. **JSON diff protocol** (`heaplens-daemon` → `heaplens-flutter`), defined in §3.4 and mirrored as Dart models.

If a change is needed in how two units talk, it changes the contract in one place. No unit parses another unit's private structures.

### 1.3 Why the allocator and daemon are separate processes

The allocator lives **inside the observed program's process**. The daemon is a **separate process**. This isolation guarantees the observed program cannot be slowed or destabilized by graph construction, symbol resolution, persistence, or WebSocket traffic. The boundary between them is the named pipe.

---

## 2. Repository layout

A single Cargo workspace for the Rust side, plus a sibling Flutter project.

```
heaplens/
├── Cargo.toml                  # workspace manifest
├── README.md
├── crates/
│   ├── heaplens-protocol/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # re-exports
│   │       ├── event.rs        # AllocEvent, EventKind, byte layout
│   │       ├── frame.rs        # frame encode/decode (length-prefixed)
│   │       └── diff.rs         # GraphDiff / NodeDto serde types (JSON)
│   │
│   ├── heaplens-alloc/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # HeapLensAlloc, #[global_allocator] guidance
│   │       ├── ring.rs         # SPSC ring buffer
│   │       ├── guard.rs        # thread-local recursion guard
│   │       ├── capture.rs      # stack capture + timestamp
│   │       └── writer.rs       # writer thread: drain ring → named pipe (+ symbols)
│   │
│   └── heaplens-daemon/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs         # bootstrap: spawn pipe server + ws server
│           ├── ingest.rs       # named pipe server: bytes → AllocEvent
│           ├── graph.rs        # OwnershipGraph: the model (N, A, φ)
│           ├── resolver.rs     # address → symbol table (joins SymbolDef stream)
│           ├── anomaly.rs      # orphan / growth / storm heuristics
│           ├── store.rs        # SQLite time-series persistence
│           ├── server.rs       # WebSocket server: broadcast GraphDiff
│           └── config.rs       # thresholds (tau, growth %, storm rate)
│
├── examples/                   # synthetic leak programs (separate bins)
│   ├── leak_rc_cycle.rs
│   ├── leak_unbounded.rs
│   └── leak_channel.rs
│
└── heaplens_flutter/           # Flutter app (separate project)
    ├── pubspec.yaml
    └── lib/
        ├── main.dart
        ├── providers/
        │   ├── ws_provider.dart
        │   └── graph_provider.dart
        ├── models/
        │   ├── node.dart
        │   └── graph_diff.dart
        ├── simulation/
        │   └── force_layout.dart
        └── widgets/
            ├── graph_canvas.dart
            ├── memory_map.dart
            ├── control_bar.dart
            └── node_detail.dart
```

---

## 3. Shared contract — `heaplens-protocol`

This crate is **data only**. No threads, no I/O, no logic beyond (de)serialization. Both `heaplens-alloc` and `heaplens-daemon` depend on it.

### 3.1 `AllocEvent` — exact byte layout

`#[repr(C)]`, fixed size **104 bytes**, alignment 8. Field order is chosen to pack with no implicit tail padding.

```rust
// event.rs
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Alloc   = 0,
    Dealloc = 1,
    Realloc = 2,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct AllocEvent {
    pub kind: u8,         // @0   EventKind as u8
    pub stack_len: u8,    // @1   number of valid frames in `stack`
    pub _pad: [u8; 2],    // @2   explicit padding
    pub align: u32,       // @4   allocation alignment
    pub ptr: u64,         // @8   allocated pointer (0 on failure)
    pub old_ptr: u64,     // @16  previous pointer (Realloc only, else 0)
    pub size: u64,        // @24  size in bytes
    pub ts_nanos: u64,    // @32  monotonic timestamp (ns since process start)
    pub stack: [u64; 8],  // @40  raw return addresses; unused slots = 0
}                          // total size = 104, align = 8
```

Provide zero-copy views (used by the allocator's writer thread — never on the critical path):

```rust
impl AllocEvent {
    pub const SIZE: usize = 104;
    pub fn as_bytes(&self) -> &[u8] { /* safety: repr(C), POD */ }
    pub fn from_bytes(buf: &[u8]) -> Option<AllocEvent> { /* len check + copy */ }
}
```

A compile-time assertion must guarantee the size:

```rust
const _: () = assert!(core::mem::size_of::<AllocEvent>() == 104);
```

### 3.2 Frame protocol (allocator → daemon, over named pipe)

A length-prefixed, typed framing. Little-endian throughout.

```
Frame:
  [u32 length]      # number of bytes that follow (type byte + payload)
  [u8  frame_type]
  [payload ...]     # (length - 1) bytes
```

| `frame_type` | Name | Payload |
|---|---|---|
| `0x00` | HANDSHAKE | `[u64 pid][u16 name_len][name_bytes (UTF-8)]` |
| `0x01` | EVENTS | `[u16 count][AllocEvent; count]` (count × 104 bytes) |
| `0x02` | SYMBOLS | `[u16 count][SymbolDef; count]` |

`SymbolDef` (variable length):
```
[u64 addr][u16 name_len][name_bytes (UTF-8)]
```

`frame.rs` provides `encode_events(&[AllocEvent]) -> Vec<u8>`, `encode_symbols(&[(u64,&str)]) -> Vec<u8>`, `encode_handshake(...)`, and a streaming `FrameDecoder` for the daemon that yields `Frame` values from a byte stream (handles partial reads).

### 3.3 Batching rules (producer side)

- The writer flushes an EVENTS frame when it has accumulated **64 events** or every **1 ms**, whichever comes first.
- SYMBOLS frames are emitted lazily: the first time the writer sees a new return address, it resolves and emits one `SymbolDef`. Each address is sent **once**.
- HANDSHAKE is the first frame sent after the pipe connects.

### 3.4 Diff protocol (daemon → Flutter, over WebSocket, JSON)

`diff.rs` defines serde types. Two message shapes, discriminated by `type`.

`NodeDto`:
```json
{
  "id": 140234,
  "ptr": 140212098345984,
  "size": 128,
  "symbol": "alloc::vec::Vec<T>::push",
  "ts": 1719240000000,
  "live": true,
  "state": "healthy",          // healthy | orphan | hot | freed
  "edges": [140300, 140301]    // ids this node owns
}
```

Snapshot (sent once on connect):
```json
{ "type": "snapshot", "ts": 1719240000000, "nodes": [ NodeDto, ... ] }
```

Diff (sent every ~33 ms when there are changes):
```json
{
  "type": "diff",
  "ts": 1719240001000,
  "add":    [ NodeDto, ... ],
  "update": [ NodeDto, ... ],
  "remove": [ 140100, 140101 ]
}
```

JSON field names are the contract. Dart models in §6 mirror them exactly.

---

## 4. Component A — `heaplens-alloc`

**Concern:** intercept allocations and ship raw events off-process without blocking. Nothing else.

### 4.1 Public surface

The only integration the user performs:

```rust
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();
```

No macros, no other API. `HeapLensAlloc::new()` must be `const`.

### 4.2 `lib.rs` — the allocator

Wrap `std::alloc::System`. Instrument every path. The critical path does the minimum: real alloc, then `record`.

```rust
use std::alloc::{GlobalAlloc, Layout, System};

pub struct HeapLensAlloc;

impl HeapLensAlloc { pub const fn new() -> Self { HeapLensAlloc } }

unsafe impl GlobalAlloc for HeapLensAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        record(EventKind::Alloc, ptr as u64, 0, layout);
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        record(EventKind::Dealloc, ptr as u64, 0, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        record_realloc(ptr as u64, new_ptr as u64, new_size, layout.align());
        new_ptr
    }
}
```

`record` must:
1. Check the recursion guard (§4.3). If already inside, return immediately — do nothing.
2. Set the guard.
3. Capture stack + timestamp (§4.5) into an `AllocEvent`.
4. `ring::push(event)` — non-blocking; on full ring, drop silently.
5. Clear the guard.

`record` performs **no heap allocation** and **no locking**.

### 4.3 `guard.rs` — recursion guard

```rust
use std::cell::Cell;
thread_local! { static IN_ALLOC: Cell<bool> = const { Cell::new(false) }; }

pub fn enter() -> bool { IN_ALLOC.with(|f| if f.get() { false } else { f.set(true); true }) }
pub fn leave() { IN_ALLOC.with(|f| f.set(false)); }
pub fn force_enter_permanent() { IN_ALLOC.with(|f| f.set(true)); } // writer thread uses this
```

The guard is checked **first** in `record`. If TLS initialization itself allocates, the nested call sees `IN_ALLOC` not yet set and proceeds once — acceptable; document it. The **writer thread** calls `force_enter_permanent()` at startup so none of its own allocations (pipe, buffers, symbol resolution) are ever recorded.

### 4.4 `ring.rs` — SPSC ring buffer

- Fixed capacity `CAP = 65_536` (power of two), single producer (allocator threads via the guard serialize one-at-a-time per thread; treat as SPSC by funneling through one lock-free MPSC-safe design OR document the assumption). **Implementation:** use a bounded lock-free queue with atomic `head`/`tail` (`AtomicUsize`), `Acquire`/`Release` ordering, slots are `AllocEvent` (POD/Copy).
- **The backing storage is allocated once via `System` directly**, never via the global allocator. Use `OnceLock` + `System.alloc` for the slot array, or a `static` zeroed array.
- `push(ev) -> bool`: returns `false` if full (caller drops). Never blocks.
- `pop() -> Option<AllocEvent>`: used by the writer thread.
- Maintain an `AtomicU64 dropped` counter incremented on full-ring drops; the writer periodically ships it (optional diagnostics).

> Note on multiple threads: real programs allocate from many threads. A strict SPSC ring assumes one producer. Use a single MPSC-capable lock-free ring (e.g. a fixed-capacity Michael-Scott-style or a sharded ring per thread merged by the writer). Simplest correct choice for the prototype: **one ring per thread** (thread-local ring), each drained by the writer. Document the choice. Do not use a `Mutex`.

### 4.5 `capture.rs` — stack + time

- Timestamp: `Instant::now().duration_since(START).as_nanos() as u64`, where `START` is a process-start `Instant` in a `OnceLock`. No allocation.
- Stack: `backtrace::trace_unsynchronized(|frame| { ... })`, copy up to 8 instruction pointers into `stack`, set `stack_len`. **Do not resolve symbols here.** Capture raw IPs only.

### 4.6 `writer.rs` — drain → pipe (off critical path)

Spawned lazily on first event under the guard, or via a constructor. The thread:
1. `guard::force_enter_permanent()`.
2. Connect to the named pipe client at `\\.\pipe\heaplens` (open via `std::fs::OpenOptions` on the pipe path, or `windows-sys` `CreateFileW`). Retry until the daemon's server is up.
3. Send HANDSHAKE (pid + process name).
4. Loop: drain ring(s), accumulate up to 64 events or 1 ms, then:
   - For each new return address across the batch, resolve via `backtrace::resolve` (in-process — addresses are valid here), cache `addr → name`, and emit a SYMBOLS frame for newly-seen addresses.
   - Emit an EVENTS frame.
5. On pipe error, attempt reconnect; never panic the host process.

> Symbol resolution lives **here**, not in the daemon, because addresses are only meaningful in the observed process and the platform symbolizer (dbghelp/PDB on MSVC) resolves them in-process. This refines requirement R5: resolution is off the critical path (writer thread), and the daemon receives ready `addr → name` pairs. The daemon does **no** symbolization.

### 4.7 Dependencies (`heaplens-alloc/Cargo.toml`)

```
heaplens-protocol = { path = "../heaplens-protocol" }
backtrace = "0.3"
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Storage_FileSystem", "Win32_System_Pipes"] }
```
(`std` provides `OnceLock`, `Instant`, `thread`. No tokio here — the observed process must stay light.)

---

## 5. Component B — `heaplens-daemon`

**Concern:** turn the event stream into a live model, detect anomalies, persist, and serve diffs. Separate process, async (Tokio).

### 5.1 `main.rs` — bootstrap

- Parse config (§5.8).
- Open SQLite (`store::open`).
- Create a shared `OwnershipGraph` behind an async-friendly handle (single owning task + channels; avoid a global `Mutex` on the hot path — prefer a graph task that owns the data and receives commands over an `mpsc` channel).
- Spawn the named-pipe ingest task (§5.2).
- Spawn the WebSocket server task (§5.7).
- Spawn a periodic diff/anomaly tick (every 33 ms).

### 5.2 `ingest.rs` — named pipe server

- Create the server with `tokio::net::windows::named_pipe::ServerOptions` on `\\.\pipe\heaplens`.
- Accept one client (the observed process). Read bytes into a `FrameDecoder` (from `heaplens-protocol`).
- For each decoded frame:
  - HANDSHAKE → record process metadata.
  - SYMBOLS → forward `(addr, name)` pairs to `resolver`.
  - EVENTS → forward each `AllocEvent` to the graph task.
- This module **only** decodes and routes. It contains no graph logic.

### 5.3 `graph.rs` — the model (OwnershipGraph)

This is the heart and the realization of GrapheTas = (N, A, φ). It owns all model state.

```rust
pub struct Node {
    pub id: u64,
    pub ptr: u64,
    pub size: u64,
    pub symbol: String,      // filled via resolver join (addr of stack[0])
    pub ts: u64,
    pub live: bool,
    pub edges_out: Vec<u64>, // ids this node owns
    pub owner: Option<u64>,  // back-reference for orphan detection
    pub state: NodeState,    // Healthy | Hot | Orphan | Freed
    pub stack: [u64; 8],
    pub stack_len: u8,
}

pub enum NodeState { Healthy, Hot, Orphan, Freed }

pub struct OwnershipGraph {
    nodes: HashMap<u64, Node>,        // keyed by ptr (the live id)
    next_id: u64,
    // pending diff accumulators:
    added: Vec<u64>, updated: HashSet<u64>, removed: Vec<u64>,
}
```

Responsibilities (methods):
- `on_alloc(ev)`: create a `Node`; run `infer_ownership` (φ) using the current set of live nodes and `ev.stack`; set `owner` + push to owner's `edges_out`; mark added.
- `on_dealloc(ptr)`: mark node freed; for each live child whose `owner == this`, set child `owner = None` and mark it an **orphan candidate** (final orphan status decided by the age check in `anomaly`); mark updated/removed.
- `on_realloc(old, new, size)`: migrate the node's key and update size.
- `infer_ownership(stack) -> Option<u64>` (φ): find the most recent live node whose allocation site (its `stack[0]` symbol, or any frame) appears in `stack`. Heuristic; document.
- `connected_components() -> usize`: for topological fragmentation.
- `drain_diff() -> GraphDiff`: produce and clear the pending diff (maps `Node` → `NodeDto`, attaching `symbol` from `resolver`).

The graph task owns this struct and processes commands from an `mpsc` receiver. No locks.

### 5.4 `resolver.rs` — symbol table

- Maintains `HashMap<u64, String>` (addr → name) populated from incoming SYMBOLS frames.
- `name_for(addr) -> String`: returns the cached name, or `format!("0x{addr:x}")` as fallback.
- Pure lookup/join. **No symbolization is performed here** (the allocator already did it).

### 5.5 `anomaly.rs` — heuristics

Operate on the graph each tick (read-only pass, then emit state changes as updates):
- **Orphan**: `node.live && node.owner.is_none() && had_owner_once && (now - node.ts) > tau` → `state = Orphan`. (Track `had_owner_once` via a flag set when an owner existed and was freed.)
- **Hot / growing cluster**: for each connected component, if total size grew `> growth_pct` over the last `window` → mark members `Hot`.
- **Storm**: if allocations from one symbol exceed `storm_rate` per second → flag.
- Thresholds come from `config`. All are parameters, never hardcoded magic numbers in the logic.

### 5.6 `store.rs` — SQLite persistence

- `rusqlite` with the `bundled` feature.
- Schema (one table is enough for the prototype):
  ```sql
  CREATE TABLE IF NOT EXISTS alloc_events (
    ts INTEGER NOT NULL, kind INTEGER NOT NULL, ptr INTEGER NOT NULL,
    size INTEGER NOT NULL, symbol TEXT
  );
  CREATE INDEX IF NOT EXISTS idx_ts ON alloc_events(ts);
  ```
- Writes happen on a dedicated task fed by a channel, **never** in the ingest or graph hot path. Batch inserts in a transaction every ~100 ms.
- `query(window) -> Stats` for historical questions (e.g. bytes per symbol over a window). This is the "data management" angle.

### 5.7 `server.rs` — WebSocket

- `tokio-tungstenite` server on `ws://localhost:9999`.
- On client connect: send a `snapshot` built from the current graph.
- Every 33 ms: if `drain_diff()` is non-empty, broadcast a `diff` message to all clients.
- Serialize with `serde_json` using the `diff.rs` types. The server **only** serializes and sends; it does not touch model logic beyond requesting a diff.

### 5.8 `config.rs`

```rust
pub struct Config {
    pub tau_ms: u64,          // orphan age threshold (default 5000)
    pub growth_pct: f32,      // cluster growth threshold (default 0.20)
    pub growth_window_ms: u64,// default 10000
    pub storm_rate: u32,      // allocs/sec from one site (default 1000)
    pub pipe_name: String,    // default \\.\pipe\heaplens
    pub ws_addr: String,      // default 127.0.0.1:9999
}
```
Load from env vars or a `heaplens.toml`; fall back to defaults.

### 5.9 Dependencies (`heaplens-daemon/Cargo.toml`)

```
heaplens-protocol = { path = "../heaplens-protocol" }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "net", "io-util", "sync", "time"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.32", features = ["bundled"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
```
(Verify and use latest compatible versions at build time.)

---

## 6. Component C — `heaplens-flutter`

**Concern:** render the live graph and expose controls. Knows only the JSON contract (§3.4).

### 6.1 Models (`models/`) — mirror the JSON exactly

```dart
enum NodeStateDto { healthy, orphan, hot, freed }

class NodeDto {
  final int id, ptr, size, ts;
  final String symbol;
  final bool live;
  final NodeStateDto state;
  final List<int> edges;
  // fromJson / toJson mirroring §3.4 field names exactly
}

class GraphDiff {
  final String type;          // "snapshot" | "diff"
  final int ts;
  final List<NodeDto> add;    // snapshot uses `nodes`; map into `add`
  final List<NodeDto> update;
  final List<int> remove;
}
```

Use `freezed` + `json_serializable`, or hand-written `fromJson`. Field names are the contract — do not rename.

### 6.2 `providers/ws_provider.dart`

- A `StreamProvider<GraphDiff>` that connects to `ws://localhost:9999` via `web_socket_channel`, decodes each JSON message into `GraphDiff`. Handles reconnect.

### 6.3 `providers/graph_provider.dart`

- A `Notifier`/`NotifierProvider` holding `Map<int, NodeDto>` (the live graph).
- `applyDiff(GraphDiff)`: snapshot replaces all; diff applies add/update/remove.
- Exposes derived selectors: orphan list, total bytes, node count.

### 6.4 `simulation/force_layout.dart`

- Verlet integration. Each node has position + velocity.
- Forces: repulsion between all visible nodes (O(n²); cap at ~500 visible, else aggregate by symbol), attraction along edges, gravity toward center, velocity damping.
- Physics tick decoupled (~30 Hz) from render (60 fps via `AnimationController`).
- Pure layout math — no networking, no widgets.

### 6.5 `widgets/`

- `graph_canvas.dart`: `CustomPainter` drawing edges then nodes. Color by `state`: healthy=teal, orphan=coral (pulsing ring), hot=amber, freed=gray (fading). Node radius ∝ `sqrt(size)`.
- `memory_map.dart`: structured grid view (address-ordered cells) as the alternate view.
- `control_bar.dart`: pause/resume, min-size filter, orphan-only toggle, symbol search, snapshot export.
- `node_detail.dart`: tap a node → show resolved stack, size, age, size-over-time sparkline.

### 6.6 Dependencies (`pubspec.yaml`)

```
flutter_riverpod: ^2.5
web_socket_channel: ^2.4
vector_math: ^2.1
fl_chart: ^0.69
freezed_annotation / json_annotation (+ build_runner, freezed, json_serializable as dev)
```

---

## 7. Cross-cutting rules

- **Error handling (Rust):** `anyhow::Result` in the daemon; the allocator never uses `Result` on the critical path and **never panics** in the host process (a panic in `alloc` aborts the program). Wrap writer-thread errors and log/reconnect.
- **Logging:** `tracing` in the daemon only. The allocator must not log on the critical path.
- **No global mutable singletons with locks** on any hot path. Use channels + single-owner tasks.
- **Time:** monotonic only (`Instant`), never wall-clock, for `ts_nanos`.
- **Endianness:** little-endian on the wire (Windows/x86_64). State it; don't rely on native casts across the boundary beyond documented `repr(C)`.

---

## 8. Synthetic test programs (`examples/`)

Each is a standalone binary that sets `HeapLensAlloc` as global allocator and triggers one leak class.

| File | Leak | Expected in HeapLens |
|------|------|----------------------|
| `leak_rc_cycle.rs` | `Rc<RefCell<…>>` mutual reference, never dropped | isolated component → orphans after the parent scope frees |
| `leak_unbounded.rs` | `Vec` that only ever `push`es | one cluster growing continuously → Hot |
| `leak_channel.rs` | a `Sender` kept alive holding queued values | persistent nodes tied to one symbol |

---

## 9. Build order and milestones

Build bottom-up so each layer is testable before the next exists.

1. **M1 — `heaplens-protocol`.** Implement `AllocEvent` (+ size assert), `EventKind`, frame encode/decode, `FrameDecoder`, `GraphDiff`/`NodeDto` serde. Unit tests: round-trip every frame type; `from_bytes(as_bytes(e)) == e`.
2. **M2 — `heaplens-alloc`.** Ring buffer, guard, capture, allocator impl, writer thread. Test in isolation by pointing the writer at a stub pipe reader that just counts frames. Verify: a program doing N allocations produces ≥ N event records (minus documented drops), and the host never deadlocks.
3. **M3 — `heaplens-daemon` ingest + graph (no UI).** Pipe server → graph → log diffs to stdout. Run a synthetic example; assert orphans appear in the logged diffs.
4. **M4 — daemon anomaly + store + WebSocket.** Add heuristics, SQLite, WS broadcast. Test WS with a CLI client (e.g. `websocat`) — confirm snapshot then diffs.
5. **M5 — `heaplens-flutter`.** Models → ws provider → graph state → force canvas → controls. Test against the running daemon with a synthetic leak; confirm orphan nodes render and drift.
6. **M6 — integration + the three examples + benchmarks.** End-to-end demo; measure overhead (control vs instrumented) and detection latency vs a reference tool.

Each milestone must compile and pass its tests before the next begins.

---

## 10. Per-component test plan

- **protocol:** property tests for frame round-trips; size/layout assertions; malformed-frame handling in `FrameDecoder`.
- **alloc:** stress test (millions of allocs, multi-thread) asserting no deadlock, bounded memory, drop-counter behavior under saturation; guard correctness (writer-thread allocations never recorded).
- **daemon:** unit tests for φ (ownership inference) on crafted stacks; orphan/growth/storm detection on synthetic event sequences; store insert+query; WS snapshot/diff shape.
- **flutter:** `applyDiff` correctness (snapshot vs diff); force-layout stability; widget smoke tests.

---

## 11. Definition of done (acceptance, ties to the cahier des charges)

- CA1 capture complete; CA2 non-blocking return; CA3 overhead under target; CA4 orphan detection per formal definition; CA5 earlier-than-reference detection; CA6 real-time smooth rendering; CA7 reproducible scenarios. (See cahier des charges §11.)

---

## 12. Invariants — never violate

1. **The allocator never heap-allocates on the critical path** (`record` and everything it calls). No `Vec`, `String`, `Box`, `format!`, or anything that allocates.
2. **The allocator never locks** on the critical path. Lock-free ring only.
3. **The allocator never panics** in the host process.
4. **The writer thread permanently holds the recursion guard**, so its own allocations are never recorded.
5. **The ring's backing store is allocated via `System` directly**, never via the global allocator.
6. **The daemon never symbolizes** — it joins `addr → name` pairs the allocator already resolved.
7. **No unit parses another unit's private types.** Cross-unit data crosses only through `heaplens-protocol` (binary) and the JSON contract (§3.4).
8. **The two seams are the single source of truth.** Change a contract in one place; never duplicate its definition.
9. **No graph/anomaly/persistence work on the ingest hot path** — route to single-owner tasks via channels.
10. **Flutter knows only JSON.** No assumptions about Rust layout, pointers, or daemon internals beyond §3.4.
