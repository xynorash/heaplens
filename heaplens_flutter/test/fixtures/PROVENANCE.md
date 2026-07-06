# Fixture provenance

All fixtures were captured from a live `heaplens-daemon` session:
daemon built from `dev/phase_4` (`master` at `3b6b4a2`), connected to via
`ws://127.0.0.1:9999` while `crates/heaplens-alloc/examples/wire_producer`-style
allocation traffic ran in another process. Capture was done with a small,
uncommitted Dart WebSocket client (not part of this repo).

- `snapshot.json` — real snapshot, captured mid-session with 101 live nodes.
- `diff_add.json` — real diff, `add` contains 101 freshly allocated nodes,
  `update`/`remove` empty.
- `diff_remove.json` — real diff, `remove` contains the same 101 node ids
  after they were freed together in one `drop`.
- `diff_orphan.json` — the `update[0]` entry is a **real captured `NodeDto`**
  (same node as `diff_add.json` id `500`), with only the `state` field
  hand-flipped from `"healthy"` to `"orphan"`. The capture rig frees an
  owner and all its children in the same `drop`, so no owner-freed-but-
  child-still-live window (the real precondition for orphan status) occurred
  naturally within the capture session. This fixture exists solely to pin
  `NodeStateDto.orphan` deserialization to the real wire shape; every field
  other than `state` is unmodified real data.
