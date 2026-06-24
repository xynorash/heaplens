# heaplens-protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bootstrap the Cargo workspace and implement the `heaplens-protocol` crate — the shared data + serialization contract for HeapLens (binary frame protocol + JSON diff protocol), with zero behavioral logic.

**Architecture:** A single Rust library crate with three focused modules: `event.rs` (POD binary type), `frame.rs` (length-prefixed framing), and `diff.rs` (serde JSON types). No threads, no I/O, no allocator/graph/UI knowledge — pure data definitions and (de)serialization.

**Tech Stack:** Rust 2021 edition, Cargo workspace, `serde` with derive, `serde_json` (dev-dep only).

**Design spec:** `docs/superpowers/specs/2026-06-24-heaplens-protocol-design.md` — all decisions locked there; do not re-derive them.

---

## File map

| Action | Path | Responsibility |
|--------|------|---------------|
| Create | `Cargo.toml` | Workspace manifest listing `crates/heaplens-protocol` |
| Create | `crates/heaplens-protocol/Cargo.toml` | Crate manifest with serde dep |
| Create | `crates/heaplens-protocol/src/lib.rs` | Re-exports only |
| Create | `crates/heaplens-protocol/src/event.rs` | `AllocEvent`, `EventKind`, `SIZE`, `as_bytes`, `from_bytes`, `new` |
| Create | `crates/heaplens-protocol/src/frame.rs` | `Frame` enum, three encoders, `FrameDecoder` two-state machine |
| Create | `crates/heaplens-protocol/src/diff.rs` | `NodeState`, `NodeDto`, `GraphMessage` serde types |
| Create | `crates/heaplens-protocol/tests/frame_roundtrip.rs` | T3 |
| Create | `crates/heaplens-protocol/tests/frame_partial.rs` | T4 |
| Create | `crates/heaplens-protocol/tests/frame_multi.rs` | T5 |
| Create | `crates/heaplens-protocol/tests/frame_resync.rs` | T6 |
| Create | `crates/heaplens-protocol/tests/diff_json.rs` | T7 |

---

## Task 1: Cargo workspace + crate scaffold

**Files:**
- Create: `Cargo.toml`
- Create: `crates/heaplens-protocol/Cargo.toml`
- Create: `crates/heaplens-protocol/src/lib.rs`

- [ ] **Step 1: Create the workspace manifest**

Create `Cargo.toml` at the repo root:

```toml
[workspace]
resolver = "2"
members = [
    "crates/heaplens-protocol",
    # TODO: crates/heaplens-alloc
    # TODO: crates/heaplens-daemon
]
```

- [ ] **Step 2: Create the crate manifest**

Create `crates/heaplens-protocol/Cargo.toml`:

```toml
[package]
name = "heaplens-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }

[dev-dependencies]
serde_json = "1"
```

- [ ] **Step 3: Create a stub lib.rs**

Create `crates/heaplens-protocol/src/lib.rs`:

```rust
pub mod event;
pub mod frame;
pub mod diff;
```

- [ ] **Step 4: Create empty module stubs so the workspace builds**

Create `crates/heaplens-protocol/src/event.rs`:

```rust
```

Create `crates/heaplens-protocol/src/frame.rs`:

```rust
```

Create `crates/heaplens-protocol/src/diff.rs`:

```rust
```

- [ ] **Step 5: Verify workspace compiles**

```
cargo build
```

Expected: compiles with zero errors (empty modules produce no warnings at this stage).

- [ ] **Step 6: Commit scaffold**

```
git init
git add Cargo.toml crates/
git commit -m "chore: init workspace + heaplens-protocol scaffold"
```

---

## Task 2: `event.rs` — `EventKind` and `AllocEvent` types

**Files:**
- Modify: `crates/heaplens-protocol/src/event.rs`

- [ ] **Step 1: Write T1 + T2 failing tests inline**

Add to `crates/heaplens-protocol/src/event.rs`:

```rust
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Alloc   = 0,
    Dealloc = 1,
    Realloc = 2,
}

impl EventKind {
    pub fn from_u8(v: u8) -> Option<EventKind> {
        match v {
            0 => Some(EventKind::Alloc),
            1 => Some(EventKind::Dealloc),
            2 => Some(EventKind::Realloc),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocEvent {
    pub kind:      u8,
    pub stack_len: u8,
    pub _pad:      [u8; 2],
    pub align:     u32,
    pub ptr:       u64,
    pub old_ptr:   u64,
    pub size:      u64,
    pub ts_nanos:  u64,
    pub stack:     [u64; 8],
}

const _: () = assert!(core::mem::size_of::<AllocEvent>() == AllocEvent::SIZE);

impl AllocEvent {
    pub const SIZE: usize = 104;

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind:      EventKind,
        ptr:       u64,
        old_ptr:   u64,
        size:      u64,
        align:     u32,
        ts_nanos:  u64,
        stack:     [u64; 8],
        stack_len: u8,
    ) -> Self {
        AllocEvent {
            kind: kind as u8,
            stack_len,
            _pad: [0, 0],
            align,
            ptr,
            old_ptr,
            size,
            ts_nanos,
            stack,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: AllocEvent is repr(C) with an explicit `_pad` field, so
        // there is no implicit compiler padding — all SIZE bytes belong to
        // initialized fields. Lifetime is tied to &self.
        // (AllocEvent::new zero-initializes `_pad` for deterministic, leak-free
        // wire output — a contract concern, not a soundness one.)
        unsafe {
            std::slice::from_raw_parts(self as *const AllocEvent as *const u8, Self::SIZE)
        }
    }

    pub fn from_bytes(buf: &[u8]) -> Option<AllocEvent> {
        if buf.len() < Self::SIZE {
            return None;
        }
        // SAFETY: length checked above; read_unaligned makes no alignment
        // assumption about the incoming byte buffer.
        Some(unsafe { (buf.as_ptr() as *const AllocEvent).read_unaligned() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T1: size_of guarantee (belt + suspenders alongside the const assert)
    #[test]
    fn t1_size_of_alloc_event() {
        assert_eq!(core::mem::size_of::<AllocEvent>(), 104);
        assert_eq!(AllocEvent::SIZE, 104);
    }

    // T2: round-trip with non-trivial values
    // - ptr and size exceed 2^32 to exercise the full u64 path
    // - stack_len < 8; trailing slots are zero
    // - non-zero old_ptr even though kind is Alloc (tests the field independently)
    #[test]
    fn t2_round_trip() {
        let mut stack = [0u64; 8];
        stack[0] = 0x0000_7fff_dead_beef;
        stack[1] = 0x0000_7fff_cafe_babe;
        stack[2] = 0x0000_7fff_1234_5678;

        let original = AllocEvent::new(
            EventKind::Alloc,
            0x0000_2000_0000_0010, // ptr > 2^32
            0x0000_1fff_ffff_fff0, // old_ptr non-zero
            0x0000_0000_0001_0000, // size = 65536
            16,
            9_999_999_999,        // ts_nanos > 2^32
            stack,
            3,
        );

        let bytes = original.as_bytes();
        assert_eq!(bytes.len(), AllocEvent::SIZE);

        let decoded = AllocEvent::from_bytes(bytes).expect("from_bytes failed");
        assert_eq!(decoded, original);
    }

    #[test]
    fn t2_from_bytes_too_short() {
        let short = [0u8; 10];
        assert!(AllocEvent::from_bytes(&short).is_none());
    }

    #[test]
    fn eventkind_from_u8() {
        assert_eq!(EventKind::from_u8(0), Some(EventKind::Alloc));
        assert_eq!(EventKind::from_u8(1), Some(EventKind::Dealloc));
        assert_eq!(EventKind::from_u8(2), Some(EventKind::Realloc));
        assert_eq!(EventKind::from_u8(3), None);
        assert_eq!(EventKind::from_u8(255), None);
    }
}
```

- [ ] **Step 2: Run tests — verify they pass**

```
cargo test -p heaplens-protocol event
```

Expected output includes:
```
test event::tests::t1_size_of_alloc_event ... ok
test event::tests::t2_round_trip ... ok
test event::tests::t2_from_bytes_too_short ... ok
test event::tests::eventkind_from_u8 ... ok
```

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/src/event.rs
git commit -m "feat(protocol): AllocEvent, EventKind, SIZE, as_bytes/from_bytes (T1+T2)"
```

---

## Task 3: `frame.rs` — encoders

**Files:**
- Modify: `crates/heaplens-protocol/src/frame.rs`

- [ ] **Step 1: Implement Frame enum and the three encode functions**

Replace `crates/heaplens-protocol/src/frame.rs` with:

```rust
use crate::event::AllocEvent;

#[derive(Debug)]
pub enum Frame {
    Handshake { pid: u64, name: String },
    Events(Vec<AllocEvent>),
    Symbols(Vec<(u64, String)>),
}

/// Encode a HANDSHAKE frame.
/// Wire: [u32 length][0x00][u64 pid][u16 name_len][name UTF-8]
/// length = 1 + 8 + 2 + name.len()
pub fn encode_handshake(pid: u64, name: &str) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let payload_len = 1 + 8 + 2 + name_bytes.len();
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x00);
    buf.extend_from_slice(&pid.to_le_bytes());
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);
    buf
}

/// Encode an EVENTS frame.
/// Wire: [u32 length][0x01][u16 count][AllocEvent × count]
/// length = 1 + 2 + count * 104
pub fn encode_events(events: &[AllocEvent]) -> Vec<u8> {
    let payload_len = 1 + 2 + events.len() * AllocEvent::SIZE;
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x01);
    buf.extend_from_slice(&(events.len() as u16).to_le_bytes());
    for ev in events {
        buf.extend_from_slice(ev.as_bytes());
    }
    buf
}

/// Encode a SYMBOLS frame.
/// Wire: [u32 length][0x02][u16 count]([u64 addr][u16 name_len][name UTF-8] × count)
pub fn encode_symbols(symbols: &[(u64, &str)]) -> Vec<u8> {
    let payload_body: usize = symbols.iter().map(|(_, n)| 8 + 2 + n.len()).sum();
    let payload_len = 1 + 2 + payload_body;
    let mut buf = Vec::with_capacity(4 + payload_len);
    buf.extend_from_slice(&(payload_len as u32).to_le_bytes());
    buf.push(0x02);
    buf.extend_from_slice(&(symbols.len() as u16).to_le_bytes());
    for (addr, name) in symbols {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&addr.to_le_bytes());
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
    }
    buf
}
```

- [ ] **Step 2: Verify the crate still compiles**

```
cargo build -p heaplens-protocol
```

Expected: zero errors.

- [ ] **Step 3: Commit encoders**

```
git add crates/heaplens-protocol/src/frame.rs
git commit -m "feat(protocol): Frame enum + encode_handshake/events/symbols"
```

---

## Task 4: `frame.rs` — `FrameDecoder` two-state machine

**Files:**
- Modify: `crates/heaplens-protocol/src/frame.rs`

- [ ] **Step 1: Add `FrameDecoder` to `frame.rs`**

Append the following to `crates/heaplens-protocol/src/frame.rs` (after the encoders):

```rust
const MAX_FRAME_LEN: u32 = 8 * 1024 * 1024; // 8 MiB

enum DecoderState {
    NeedLength,
    NeedBody { total: usize },
}

pub struct FrameDecoder {
    state: DecoderState,
    buf:   Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        FrameDecoder { state: DecoderState::NeedLength, buf: Vec::new() }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete Frame, or None if more bytes are needed.
    /// Malformed frames are skipped silently (skip-and-continue).
    /// Incomplete frames (not enough bytes yet) return None without draining.
    pub fn next(&mut self) -> Option<Frame> {
        loop {
            match self.state {
                DecoderState::NeedLength => {
                    if self.buf.len() < 4 {
                        return None; // incomplete — not an error
                    }
                    let length = u32::from_le_bytes(self.buf[0..4].try_into().unwrap());
                    if length == 0 || length > MAX_FRAME_LEN {
                        // Untrustworthy length prefix — resync one byte at a time
                        self.buf.drain(0..1);
                        continue; // loop: try again from next byte
                    }
                    self.state = DecoderState::NeedBody { total: 4 + length as usize };
                }
                DecoderState::NeedBody { total } => {
                    if self.buf.len() < total {
                        return None; // incomplete — not an error
                    }
                    let frame_bytes: Vec<u8> = self.buf.drain(0..total).collect();
                    self.state = DecoderState::NeedLength;

                    // frame_bytes[0..4] = length prefix (already consumed for total)
                    // frame_bytes[4]    = ftype
                    // frame_bytes[5..]  = payload
                    if frame_bytes.len() < 5 {
                        continue; // skip: no ftype byte (shouldn't happen given length > 0)
                    }
                    let ftype = frame_bytes[4];
                    let payload = &frame_bytes[5..];

                    match Self::decode_payload(ftype, payload) {
                        Some(frame) => return Some(frame),
                        None => continue, // skip malformed, try next frame
                    }
                }
            }
        }
    }

    fn decode_payload(ftype: u8, payload: &[u8]) -> Option<Frame> {
        match ftype {
            0x00 => Self::decode_handshake(payload),
            0x01 => Self::decode_events(payload),
            0x02 => Self::decode_symbols(payload),
            _    => None, // unknown ftype → skip
        }
    }

    fn decode_handshake(payload: &[u8]) -> Option<Frame> {
        // payload: [u64 pid][u16 name_len][name UTF-8]
        // frame_bytes.len() - 5 == 8 + 2 + name_len
        if payload.len() < 10 {
            return None;
        }
        let pid      = u64::from_le_bytes(payload[0..8].try_into().unwrap());
        let name_len = u16::from_le_bytes(payload[8..10].try_into().unwrap()) as usize;
        if payload.len() != 10 + name_len {
            return None;
        }
        let name = std::str::from_utf8(&payload[10..10 + name_len]).ok()?.to_owned();
        Some(Frame::Handshake { pid, name })
    }

    fn decode_events(payload: &[u8]) -> Option<Frame> {
        // payload: [u16 count][AllocEvent × count]
        // payload.len() == 2 + count * 104
        if payload.len() < 2 {
            return None;
        }
        let count = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
        if payload.len() != 2 + count * AllocEvent::SIZE {
            return None;
        }
        let mut events = Vec::with_capacity(count);
        for i in 0..count {
            let start = 2 + i * AllocEvent::SIZE;
            let ev = AllocEvent::from_bytes(&payload[start..start + AllocEvent::SIZE])?;
            events.push(ev);
        }
        Some(Frame::Events(events))
    }

    fn decode_symbols(payload: &[u8]) -> Option<Frame> {
        // payload: [u16 count]([u64 addr][u16 name_len][name UTF-8] × count)
        if payload.len() < 2 {
            return None;
        }
        let count  = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
        let mut cursor = 2usize;
        let mut syms   = Vec::with_capacity(count);
        for _ in 0..count {
            if cursor + 10 > payload.len() {
                return None; // truncated mid-def → skip whole frame
            }
            let addr     = u64::from_le_bytes(payload[cursor..cursor + 8].try_into().unwrap());
            let name_len = u16::from_le_bytes(payload[cursor + 8..cursor + 10].try_into().unwrap()) as usize;
            cursor += 10;
            if cursor + name_len > payload.len() {
                return None; // truncated name → skip whole frame
            }
            let name = std::str::from_utf8(&payload[cursor..cursor + name_len]).ok()?.to_owned();
            cursor += name_len;
            syms.push((addr, name));
        }
        Some(Frame::Symbols(syms))
    }
}

impl Default for FrameDecoder {
    fn default() -> Self { Self::new() }
}
```

- [ ] **Step 2: Verify the crate compiles**

```
cargo build -p heaplens-protocol
```

Expected: zero errors.

- [ ] **Step 3: Commit decoder**

```
git add crates/heaplens-protocol/src/frame.rs
git commit -m "feat(protocol): FrameDecoder two-state machine (NeedLength/NeedBody)"
```

---

## Task 5: `diff.rs` — JSON contract types

**Files:**
- Modify: `crates/heaplens-protocol/src/diff.rs`

- [ ] **Step 1: Implement the serde types**

Replace `crates/heaplens-protocol/src/diff.rs` with:

```rust
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
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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

/// Discriminated union for the two daemon→Flutter message shapes.
///
/// Serializes with an inline `"type"` tag:
///   Snapshot → `{ "type": "snapshot", "ts": …, "nodes": […] }`
///   Diff     → `{ "type": "diff",     "ts": …, "add": […], "update": […], "remove": […] }`
///
/// NOTE: `u64` fields serialize as bare JSON numbers. This is safe for the
/// Dart VM (Flutter desktop/native on Windows) where `int` is 64-bit.
/// It is NOT safe under dart2js / Flutter web (IEEE-754 doubles, max 2^53).
/// If the project retargets Flutter web, ptr/id/ts must become JSON strings.
///
/// Lenient deserialization (no `deny_unknown_fields`) is deliberate: forward-
/// compatible additions to NodeDto must not break older deserializers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum GraphMessage {
    Snapshot { ts: u64, nodes: Vec<NodeDto> },
    Diff     { ts: u64, add: Vec<NodeDto>, update: Vec<NodeDto>, remove: Vec<u64> },
}
```

- [ ] **Step 2: Verify the crate compiles**

```
cargo build -p heaplens-protocol
```

Expected: zero errors.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/src/diff.rs
git commit -m "feat(protocol): NodeState, NodeDto, GraphMessage serde types"
```

---

## Task 6: `lib.rs` — re-exports

**Files:**
- Modify: `crates/heaplens-protocol/src/lib.rs`

- [ ] **Step 1: Add public re-exports**

Replace `crates/heaplens-protocol/src/lib.rs` with:

```rust
pub mod event;
pub mod frame;
pub mod diff;

pub use event::{AllocEvent, EventKind};
pub use frame::{Frame, FrameDecoder, encode_events, encode_handshake, encode_symbols};
pub use diff::{GraphMessage, NodeDto, NodeState};
```

- [ ] **Step 2: Verify**

```
cargo build -p heaplens-protocol
```

Expected: zero errors.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/src/lib.rs
git commit -m "feat(protocol): lib.rs re-exports flat public API"
```

---

## Task 7: Integration tests — T3 frame round-trips

**Files:**
- Create: `crates/heaplens-protocol/tests/frame_roundtrip.rs`

- [ ] **Step 1: Write T3**

Create `crates/heaplens-protocol/tests/frame_roundtrip.rs`:

```rust
use heaplens_protocol::{
    AllocEvent, EventKind, Frame, FrameDecoder,
    encode_events, encode_handshake, encode_symbols,
};

fn make_decoder_with(bytes: &[u8]) -> FrameDecoder {
    let mut d = FrameDecoder::new();
    d.push(bytes);
    d
}

#[test]
fn handshake_round_trip() {
    let encoded = encode_handshake(12345, "test-process");
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 12345);
            assert_eq!(name, "test-process");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}

fn sample_event(n: u64) -> AllocEvent {
    let mut stack = [0u64; 8];
    stack[0] = 0x7fff_0000_0000_0000 + n;
    AllocEvent::new(EventKind::Alloc, 0x2000_0000_0000 + n, 0, 64 + n, 8, 1_000_000 + n, stack, 1)
}

#[test]
fn events_round_trip_multiple() {
    let events = vec![sample_event(1), sample_event(2), sample_event(3)];
    let encoded = encode_events(&events);
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Events(decoded) => {
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[0], events[0]);
            assert_eq!(decoded[1], events[1]);
            assert_eq!(decoded[2], events[2]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}

#[test]
fn symbols_round_trip_multiple() {
    let syms: Vec<(u64, &str)> = vec![
        (0x7fff_dead_0001, "alloc::vec::Vec::push"),
        (0x7fff_dead_0002, "std::collections::HashMap::insert"),
        (0x7fff_dead_0003, "my_crate::foo::bar"),
    ];
    let encoded = encode_symbols(&syms);
    let mut dec = make_decoder_with(&encoded);
    match dec.next().expect("expected a frame") {
        Frame::Symbols(decoded) => {
            assert_eq!(decoded.len(), 3);
            assert_eq!(decoded[0], (syms[0].0, syms[0].1.to_owned()));
            assert_eq!(decoded[1], (syms[1].0, syms[1].1.to_owned()));
            assert_eq!(decoded[2], (syms[2].0, syms[2].1.to_owned()));
        }
        other => panic!("wrong variant: {other:?}"),
    }
    assert!(dec.next().is_none());
}
```

- [ ] **Step 2: Run T3**

```
cargo test -p heaplens-protocol --test frame_roundtrip
```

Expected: all three tests pass.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/tests/frame_roundtrip.rs
git commit -m "test(protocol): T3 frame round-trips (handshake, events, symbols)"
```

---

## Task 8: Integration tests — T4 partial read

**Files:**
- Create: `crates/heaplens-protocol/tests/frame_partial.rs`

- [ ] **Step 1: Write T4**

Create `crates/heaplens-protocol/tests/frame_partial.rs`:

```rust
use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder, encode_events};

#[test]
fn single_frame_fed_one_byte_at_a_time() {
    let mut stack = [0u64; 8];
    stack[0] = 0x7fff_cafe_babe_0001;
    let event = AllocEvent::new(
        EventKind::Realloc,
        0x0000_3000_0000_0010,
        0x0000_2fff_ffff_fff0,
        256,
        16,
        5_000_000_000,
        stack,
        1,
    );
    let encoded = encode_events(&[event]);

    let mut dec = FrameDecoder::new();
    let mut frames_seen = 0usize;
    let mut result: Option<Frame> = None;

    for byte in &encoded {
        dec.push(std::slice::from_ref(byte));
        while let Some(frame) = dec.next() {
            frames_seen += 1;
            result = Some(frame);
        }
    }

    assert_eq!(frames_seen, 1, "expected exactly one frame");
    match result.expect("frame should have decoded") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
```

- [ ] **Step 2: Run T4**

```
cargo test -p heaplens-protocol --test frame_partial
```

Expected: passes.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/tests/frame_partial.rs
git commit -m "test(protocol): T4 partial-read (one byte at a time)"
```

---

## Task 9: Integration tests — T5 multi-frame single push

**Files:**
- Create: `crates/heaplens-protocol/tests/frame_multi.rs`

- [ ] **Step 1: Write T5**

Create `crates/heaplens-protocol/tests/frame_multi.rs`:

```rust
use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder,
                        encode_events, encode_handshake, encode_symbols};

#[test]
fn three_frames_in_one_push() {
    let event = AllocEvent::new(
        EventKind::Alloc, 0x1000_0000_0001, 0, 128, 8, 42_000_000, [0u64; 8], 0,
    );
    let f1 = encode_handshake(99, "multi-test");
    let f2 = encode_events(&[event]);
    let f3 = encode_symbols(&[(0x7fff_1234_5678, "some::symbol")]);

    let mut combined = Vec::new();
    combined.extend_from_slice(&f1);
    combined.extend_from_slice(&f2);
    combined.extend_from_slice(&f3);

    let mut dec = FrameDecoder::new();
    dec.push(&combined);

    match dec.next().expect("frame 1") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 99);
            assert_eq!(name, "multi-test");
        }
        other => panic!("frame 1 wrong variant: {other:?}"),
    }

    match dec.next().expect("frame 2") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("frame 2 wrong variant: {other:?}"),
    }

    match dec.next().expect("frame 3") {
        Frame::Symbols(syms) => {
            assert_eq!(syms.len(), 1);
            assert_eq!(syms[0], (0x7fff_1234_5678, "some::symbol".to_owned()));
        }
        other => panic!("frame 3 wrong variant: {other:?}"),
    }

    assert!(dec.next().is_none());
}
```

- [ ] **Step 2: Run T5**

```
cargo test -p heaplens-protocol --test frame_multi
```

Expected: passes.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/tests/frame_multi.rs
git commit -m "test(protocol): T5 multi-frame single push"
```

---

## Task 10: Integration tests — T6 resync after bad length prefix

**Files:**
- Create: `crates/heaplens-protocol/tests/frame_resync.rs`

- [ ] **Step 1: Write T6**

Create `crates/heaplens-protocol/tests/frame_resync.rs`:

```rust
use heaplens_protocol::{AllocEvent, EventKind, Frame, FrameDecoder,
                        encode_events, encode_handshake};

#[test]
fn resync_after_absurd_length_prefix() {
    // Two valid frames sandwiching junk with an absurd length prefix.
    // The junk bytes must not prevent either valid frame from decoding.
    let event = AllocEvent::new(
        EventKind::Dealloc, 0x5555_0000_0001, 0, 64, 8, 1, [0u64; 8], 0,
    );
    let valid1 = encode_handshake(1, "before-junk");
    let valid2 = encode_events(&[event]);

    // Junk: a u32 length prefix of 0xFF_FF_FF_FF (> MAX_FRAME_LEN = 8 MiB),
    // followed by a few bytes. The decoder must drain one byte at a time until
    // it resyncs onto valid2's length prefix.
    let junk: Vec<u8> = {
        let mut j = Vec::new();
        j.extend_from_slice(&u32::MAX.to_le_bytes()); // absurd length
        j.extend_from_slice(&[0xAA, 0xBB, 0xCC]);    // padding noise
        j
    };

    let mut combined = Vec::new();
    combined.extend_from_slice(&valid1);
    combined.extend_from_slice(&junk);
    combined.extend_from_slice(&valid2);

    let mut dec = FrameDecoder::new();
    dec.push(&combined);

    // First valid frame decodes before the junk
    match dec.next().expect("frame 1 (before junk)") {
        Frame::Handshake { pid, name } => {
            assert_eq!(pid, 1);
            assert_eq!(name, "before-junk");
        }
        other => panic!("frame 1 wrong variant: {other:?}"),
    }

    // Second valid frame decodes after the decoder resyncs past the junk
    match dec.next().expect("frame 2 (after junk)") {
        Frame::Events(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], event);
        }
        other => panic!("frame 2 wrong variant: {other:?}"),
    }

    assert!(dec.next().is_none());
}
```

- [ ] **Step 2: Run T6**

```
cargo test -p heaplens-protocol --test frame_resync
```

Expected: passes.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/tests/frame_resync.rs
git commit -m "test(protocol): T6 resync after absurd length prefix"
```

---

## Task 11: Integration tests — T7 JSON shape assertions

**Files:**
- Create: `crates/heaplens-protocol/tests/diff_json.rs`

- [ ] **Step 1: Write T7**

Create `crates/heaplens-protocol/tests/diff_json.rs`:

```rust
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
```

- [ ] **Step 2: Run T7**

```
cargo test -p heaplens-protocol --test diff_json
```

Expected: all four tests pass.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-protocol/tests/diff_json.rs
git commit -m "test(protocol): T7 JSON shape assertions (snapshot/diff/nodestate)"
```

---

## Task 12: Full suite + clippy clean

- [ ] **Step 1: Run the full test suite**

```
cargo test -p heaplens-protocol
```

Expected: all tests pass, zero failures.

- [ ] **Step 2: Run clippy**

```
cargo clippy -p heaplens-protocol -- -D warnings
```

Expected: zero warnings. If clippy flags anything, fix it before continuing. Common fixes:
- `#[allow(clippy::too_many_arguments)]` is already on `AllocEvent::new` — clippy should not flag it.
- If clippy flags `match` in `EventKind::from_u8`, that's correct by design; no change needed.
- If clippy suggests `impl Default for FrameDecoder` — it is already implemented in Task 4.

- [ ] **Step 3: Verify no Stage 2+ content exists**

```
cargo tree -p heaplens-protocol
```

Expected: only `serde` (and in dev builds `serde_json`) appear. No tokio, backtrace, or windows-sys.

- [ ] **Step 4: Final commit**

```
git add -p   # stage any clippy fixes
git commit -m "chore(protocol): clippy clean, Stage 1 complete"
```

---

## Acceptance checklist

- [ ] `cargo build` passes
- [ ] `cargo test -p heaplens-protocol` — all 7 test groups pass (T1–T7)
- [ ] `cargo clippy -p heaplens-protocol -- -D warnings` — zero warnings
- [ ] `cargo tree -p heaplens-protocol` — no forbidden deps (tokio, backtrace, windows-sys)
- [ ] `AllocEvent::SIZE == 104` enforced by const assert at compile time
- [ ] Both seams fully specified in code: binary frame protocol (`frame.rs`) and JSON diff protocol (`diff.rs`)
- [ ] Zero behavioral logic in the crate: no threads, no I/O, no graph/allocator/UI knowledge
