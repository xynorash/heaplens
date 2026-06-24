# Stage 1 Design — `heaplens-protocol`

**Date:** 2026-06-24  
**Scope:** Cargo workspace bootstrap + `heaplens-protocol` crate only. No allocator, daemon, or Flutter code.  
**Source of truth:** `docs/HeapLens_Build_Spec.md` §2–3. This document records the implementation decisions made during design review that are not fully specified there.

---

## 1. Workspace layout

```
heaplens/
├── Cargo.toml                  # workspace manifest; edition 2021
│                               # members = ["crates/heaplens-protocol"]
│                               # TODO: add crates/heaplens-alloc, crates/heaplens-daemon
└── crates/
    └── heaplens-protocol/
        ├── Cargo.toml
        └── src/
            ├── lib.rs
            ├── event.rs
            ├── frame.rs
            └── diff.rs
```

`heaplens-protocol/Cargo.toml` dependencies:
```toml
[dependencies]
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
serde_json = "1"
```

No other dependencies. No tokio, backtrace, or windows-sys in this crate.

---

## 2. `event.rs`

### Public surface

```rust
const _: () = assert!(core::mem::size_of::<AllocEvent>() == AllocEvent::SIZE);

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind { Alloc = 0, Dealloc = 1, Realloc = 2 }

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocEvent {
    pub kind: u8,        // @0
    pub stack_len: u8,   // @1
    pub _pad: [u8; 2],   // @2  must always be [0, 0] — as_bytes soundness
    pub align: u32,      // @4
    pub ptr: u64,        // @8
    pub old_ptr: u64,    // @16
    pub size: u64,       // @24
    pub ts_nanos: u64,   // @32
    pub stack: [u64; 8], // @40
}
```

### Implementation decisions

**`AllocEvent::new` (canonical constructor)**  
`SIZE` is an associated const so it is reachable as `AllocEvent::SIZE`. Enforces `_pad: [0, 0]` at every construction site. Stage 2 must use this constructor; it must never construct `AllocEvent` with struct literal syntax that could leave `_pad` uninitialised.

```rust
impl AllocEvent {
    pub const SIZE: usize = 104;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: EventKind, ptr: u64, old_ptr: u64, size: u64,
        align: u32, ts_nanos: u64, stack: [u64; 8], stack_len: u8,
    ) -> Self {
        AllocEvent { kind: kind as u8, stack_len, _pad: [0, 0],
                     align, ptr, old_ptr, size, ts_nanos, stack }
    }
}
```

**`as_bytes` — zero-copy view**  
Safe public API wrapping one `unsafe` block. Justified by `#[repr(C)]` + POD (all fields are primitive integer types with no padding beyond `_pad`).

```rust
pub fn as_bytes(&self) -> &[u8] {
    // SAFETY: AllocEvent is repr(C) with an explicit `_pad` field, so there
    // is no implicit compiler padding — all SIZE bytes belong to initialized
    // fields. The lifetime of the returned slice is tied to &self.
    // (Separately, AllocEvent::new zero-initializes `_pad` for deterministic,
    // leak-free wire output — a contract concern, not a soundness one.)
    unsafe { std::slice::from_raw_parts(self as *const AllocEvent as *const u8, Self::SIZE) }
}
```

**`from_bytes` — `read_unaligned`**  
The incoming buffer is a `&[u8]` off the wire — guaranteed only 1-byte aligned. `AllocEvent` requires 8-byte alignment (u64 fields). An aligned pointer cast would be UB; `read_unaligned` makes no alignment assumption.

```rust
pub fn from_bytes(buf: &[u8]) -> Option<AllocEvent> {
    if buf.len() < Self::SIZE { return None; }
    // SAFETY: length checked above; read_unaligned makes no alignment
    // assumption about the incoming byte buffer.
    Some(unsafe { (buf.as_ptr() as *const AllocEvent).read_unaligned() })
}
```

**`EventKind::from_u8`**  
Explicit `match`; returns `None` for any value other than 0, 1, 2.

---

## 3. `frame.rs`

### Wire format (little-endian throughout)

```
Frame = [u32 length][u8 ftype][payload of (length - 1) bytes]

ftype 0x00  HANDSHAKE  payload: [u64 pid][u16 name_len][name UTF-8]
ftype 0x01  EVENTS     payload: [u16 count][AllocEvent × count]
ftype 0x02  SYMBOLS    payload: [u16 count]([u64 addr][u16 name_len][name UTF-8] × count)
```

`length` is the byte count of everything after the u32 prefix (i.e. `1 + payload_bytes`).

### Encoders

Three pure functions, no state:

```rust
pub fn encode_handshake(pid: u64, name: &str) -> Vec<u8>
pub fn encode_events(events: &[AllocEvent]) -> Vec<u8>
pub fn encode_symbols(symbols: &[(u64, &str)]) -> Vec<u8>
```

### `Frame` enum

```rust
pub enum Frame {
    Handshake { pid: u64, name: String },
    Events(Vec<AllocEvent>),
    Symbols(Vec<(u64, String)>),
}
```

### `FrameDecoder` — two-state machine

State machine splits on the one fixed-size field (the 4-byte length prefix). `ftype` and payload are read inside `NeedBody`, not in the header step. This eliminates the `length - 1` arithmetic from the state transition.

```rust
enum DecoderState {
    NeedLength,
    NeedBody { total: usize }, // total = 4 + length; includes the 4-byte prefix
}

pub struct FrameDecoder {
    state: DecoderState,
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]);
    pub fn next(&mut self) -> Option<Frame>;
}
```

**`next` logic:**

1. `NeedLength`: if `buf.len() < 4` → return `None` (incomplete, not an error — no drain).  
   Read `length = u32::from_le_bytes(buf[0..4])`.  
   - If `length == 0 || length > MAX_FRAME_LEN` → **resync path**: drain 1 byte, stay in `NeedLength`, continue the loop within `next` (not an infinite spin — the loop exits when bytes are exhausted or a valid frame is found). (Length is untrustworthy; do NOT skip by it.)  
   - Otherwise → transition to `NeedBody { total: 4 + length as usize }`.

2. `NeedBody { total }`: if `buf.len() < total` → return `None` (incomplete, not an error — no drain).  
   Drain exactly `total` bytes into `frame_bytes`.  
   `frame_bytes[4]` = `ftype`, `frame_bytes[5..]` = payload.  
   Decode or skip, always reset to `NeedLength`.

**`MAX_FRAME_LEN`:**

```rust
const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024; // 8 MiB; generous vs ~6.6 KB for a 64-event batch
```

**Two distinct skip mechanisms — must not be conflated:**

| Cause | Action |
|-------|--------|
| Untrustworthy `length` prefix (`0` or `> MAX_FRAME_LEN`) | Drain 1 byte, resync in `NeedLength` |
| Malformed content (trustworthy length) | Drain `total` bytes, reset to `NeedLength` |

**Content validation (all offsets relative to `frame_bytes`):**

- `ftype 0x01` EVENTS: `frame_bytes.len() - 5 == 2 + count as usize * 104` must hold exactly, else skip.
- `ftype 0x00` HANDSHAKE: `frame_bytes.len() - 5 == 8 + 2 + name_len as usize`, name must be valid UTF-8, else skip.
- `ftype 0x02` SYMBOLS: parse each `SymbolDef` sequentially from `frame_bytes[5..]`; if bytes run out mid-def, skip the whole frame.
- Unknown `ftype`: skip.

**Incomplete is never an error.** `buf.len() < 4` (in `NeedLength`) and `buf.len() < total` (in `NeedBody`) are the normal partial-read path. They return `None` without draining or changing state. They must not be routed into either skip path.

---

## 4. `diff.rs`

### Types

```rust
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub enum NodeState { Healthy, Orphan, Hot, Freed }
// → "healthy" | "orphan" | "hot" | "freed"

#[derive(Serialize, Deserialize, Clone)]
pub struct NodeDto {
    pub id: u64, pub ptr: u64, pub size: u64, pub ts: u64,
    pub symbol: String, pub live: bool,
    pub state: NodeState, pub edges: Vec<u64>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GraphMessage {
    Snapshot { ts: u64, nodes: Vec<NodeDto> },
    Diff { ts: u64, add: Vec<NodeDto>, update: Vec<NodeDto>, remove: Vec<u64> },
}
```

Produces exactly:
```json
{ "type": "snapshot", "ts": 0, "nodes": [] }
{ "type": "diff",     "ts": 0, "add": [], "update": [], "remove": [] }
```

### Implementation decisions

**No `deny_unknown_fields`** (deliberate). Internal tagging interacts poorly with it in some serde versions, and forbidding unknown fields makes the contract brittle to forward-compatible additions. Lenient deserialization is the correct default for a protocol type that will evolve.

**`u64` as JSON numbers** — safe for this target (Dart VM, Flutter desktop/native on Windows). Dart native `int` is 64-bit; `jsonDecode` handles pointer-range values intact. **Not safe under dart2js / Flutter web**, where `int` degrades to IEEE-754 double and values above 2^53 silently corrupt. If the project ever retargets Flutter web, `ptr`, `id`, `ts` must become JSON strings.

**`serde_json` as dev-dependency only.** `diff.rs` requires only `serde` derive to compile. `serde_json` is used only in T7 tests.

---

## 5. `lib.rs`

```rust
pub mod event;
pub mod frame;
pub mod diff;

pub use event::{AllocEvent, EventKind};
pub use frame::{Frame, FrameDecoder, encode_handshake, encode_events, encode_symbols};
pub use diff::{GraphMessage, NodeDto, NodeState};
```

No logic. All public API addressable as `heaplens_protocol::AllocEvent` etc.

---

## 6. Test inventory

| ID | File | What it verifies |
|----|------|-----------------|
| T1 | `src/event.rs` `#[cfg(test)]` | `size_of::<AllocEvent>() == 104` (belt + suspenders alongside the const assert) |
| T2 | `src/event.rs` `#[cfg(test)]` | `from_bytes(e.as_bytes())` round-trip field-by-field via `AllocEvent::new`; non-trivial values: at least one u64 field > 2^32, `stack_len < 8` with trailing stack slots = 0, non-zero `old_ptr` |
| T3 | `tests/frame_roundtrip.rs` | Encode then `push`+`next` for all three frame types; EVENTS with multiple events; SYMBOLS with multiple defs |
| T4 | `tests/frame_partial.rs` | Single frame fed one byte at a time → exactly one `Frame` produced with correct contents |
| T5 | `tests/frame_multi.rs` | Three concatenated frames in one `push` → three frames decoded in order |
| T6 | `tests/frame_resync.rs` | Junk bytes with absurd length prefix sandwiched between two valid frames; one `push` → both valid frames decode (proves `MAX_FRAME_LEN` resync path) |
| T7 | `tests/diff_json.rs` | Serialize snapshot + diff; parse back as `serde_json::Value`; assert `value["type"] == "snapshot"`, `value["nodes"].is_array()`, `value["type"] == "diff"`, `value["add"].is_array()`, `value["update"].is_array()`, `value["remove"].is_array()` — key assertions on parsed Value, not substring match |

**Decision coverage:** T2 exercises `read_unaligned` + `_pad` invariant. T4 proves the incomplete-is-not-an-error path. T6 proves the `MAX_FRAME_LEN` one-byte-resync path (Question 2). T7 proves the serde attribute chain produces the exact §3.4 wire shapes.

---

## 7. Acceptance criteria

- `cargo build` and `cargo test` pass with zero failures.
- `cargo clippy -- -D warnings` is clean.
- The crate contains no behavioral logic: no threads, no I/O, no graph/allocator/UI knowledge.
- Nothing from Stage 2+ (heaplens-alloc, heaplens-daemon, heaplens-flutter) is present.
