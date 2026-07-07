# heaplens_flutter M5 — Build Plan

**Branch:** `dev/phase_5` (from `master` @ `3b6b4a2`)
**Scope:** `heaplens_flutter/` only. No Rust changes.
**Concern boundary:** the app knows only the JSON contract on `ws://127.0.0.1:9999`
(`heaplens-protocol`'s `diff.rs`). No Rust knowledge, no daemon-internals
assumptions.

## Already done (pre-Task-1 setup, committed)

- `heaplens_flutter/` scaffolded via `flutter create --platforms=windows`,
  Windows desktop target confirmed working (Visual Studio present,
  `flutter doctor` clean).
- `pubspec.yaml`: `flutter_riverpod: ^2.5.0`, `web_socket_channel: ^2.4.0`,
  `vector_math: ^2.1.4`, `fl_chart: ^0.69.0` added. No `freezed` /
  `json_serializable` / `build_runner` (locked Q2: hand-written models).
- `test/fixtures/{snapshot,diff_add,diff_remove,diff_orphan}.json` — real
  captured daemon JSON (provenance in `test/fixtures/PROVENANCE.md`).
  `diff_orphan.json`'s `state` field is the only hand-modified value in the
  fixture set; every other field across all four fixtures is real captured
  data.

## Locked decisions (do not deviate)

- **Q1:** Same repo, sibling of `crates/`. Dart VM / Flutter Windows desktop
  only — never Flutter web (u64-as-JSON-number is unsafe under dart2js per
  `diff.rs`'s documented seam note).
- **Q2:** Hand-written models (`lib/models/node.dart`, `graph_diff.dart`),
  field names mirror `diff.rs` exactly: `id, ptr, size, ts, symbol, live,
  state, edges`. Messages discriminated by `"type"` = `"snapshot"` (field
  `nodes`) | `"diff"` (fields `add`, `update`, `remove`). `NodeStateDto`
  enum from lowercase strings `healthy|orphan|hot|freed`; unknown state
  strings map to `healthy` with a debug log (lenient, forward-compatible —
  matches `diff.rs`'s lenient-deserialization comment). All numeric fields
  parsed as Dart `int` (64-bit on the VM).
- **Q3:** `graph_provider.dart` is a `Notifier` owning a private mutable
  `Map<int, NodeDto>` plus a public `int revision`, the sole watched value.
  `applyDiff`: snapshot → clear and repopulate; diff → apply add/update/
  remove in place. Each application increments `revision` exactly once.
  Widgets read the map via the notifier without copying.
- **Q4:** `simulation/force_layout.dart` owns `Map<int, SimNode>` (position,
  velocity, radius, fade), separate from `NodeDto`. New node → spawn near
  owner's current position if the owner has a `SimNode`, else near canvas
  center with jitter. Removed/freed node → fade over ~1s (driven by the
  simulation's own clock, not by diff arrival) then delete the `SimNode`.
  `NodeDto` field updates never touch position/velocity. Radius ∝
  `sqrt(size)`, clamped to a sane range.
- **Q5:** All four widgets in M5 — graph canvas, memory map, control bar,
  node detail.

## Global constraints (bind every task)

- Field names in `models/` are the wire contract — verbatim, no renaming.
- `NodeDto`/`GraphMessage` parsing is lenient: unknown `state` string →
  `healthy` + debug log; unknown JSON keys are ignored, never a parse
  error (forward-compat, matches `diff.rs`).
- No `freezed`, `json_serializable`, or `build_runner` anywhere.
- No widget imports in `simulation/force_layout.dart` — pure Dart, unit
  testable without a `WidgetTester`.
- `revision` (an `int`) is the only value any provider `watch`es off
  `graph_provider.dart` — never watch the map itself.
- Aggregation threshold (ENF9): live node count > 500 → switch to one
  aggregate node per `symbol` (sum of sizes, count badge). Document the
  switch at the call site.
- Physics tick ~30 Hz, decoupled from 60 fps render.
- `flutter analyze` must be clean after every task; do not silence lints
  with blanket `// ignore` — fix the underlying issue.
- Dart/Flutter idiomatic error handling: no silent `catch (_) {}`; log or
  surface. WS reconnect logic is the one place errors are expected and
  must be handled (backoff, not crash).

## Tasks

### Task 1 — Models + contract test
Files: `lib/models/node.dart`, `lib/models/graph_diff.dart`,
`test/models/contract_test.dart`.

- `NodeStateDto` enum `{healthy, orphan, hot, freed}` with a
  `static NodeStateDto fromWire(String s)` that maps the four lowercase
  strings and falls back to `healthy` + `debugPrint` for anything else.
- `NodeDto` class: fields `id, ptr, size, ts` (`int`), `symbol` (`String`),
  `live` (`bool`), `state` (`NodeStateDto`), `edges` (`List<int>`).
  `NodeDto.fromJson(Map<String, dynamic> json)` factory.
- `GraphMessage` sealed via a simple discriminated class (no `freezed`):
  a base class or enum-tagged union with `GraphMessage.fromJson` reading
  `json['type']` and dispatching: `"snapshot"` → `nodes` list mapped to
  `NodeDto`; `"diff"` → `add`/`update`/`remove` (remove is `List<int>`).
  Expose the parsed shape however is most idiomatic (e.g. a class with a
  nullable `nodes` for snapshot and nullable `add/update/remove` for diff,
  or two subclasses `GraphSnapshot`/`GraphDiff` under one sealed base) —
  implementer's choice, document it in the file.
- Contract test: read `test/fixtures/snapshot.json`, `diff_add.json`,
  `diff_remove.json`, `diff_orphan.json` from disk (`File(...).readAsStringSync()`
  relative to the test's working directory — `flutter test` runs from the
  package root, so `test/fixtures/...` is the correct relative path),
  `jsonDecode`, parse into the models, and assert field-for-field fidelity
  against known values from those files (at minimum: correct message type,
  correct counts, spot-check a handful of fields including one non-zero
  `edges` list, and confirm `diff_orphan.json`'s node parses to
  `NodeStateDto.orphan`).
- If any fixture fails to parse or a field is silently dropped, that is a
  **protocol bug** — do not adapt the model to accommodate it; report it
  instead of "fixing" the model to hide a real mismatch.

### Task 2 — WS provider
Files: `lib/providers/ws_provider.dart`.

- `StreamProvider<GraphMessage>` (Riverpod) connecting to
  `ws://127.0.0.1:9999` via `web_socket_channel`'s `WebSocketChannel.connect`.
- Each incoming message: `jsonDecode` → `GraphMessage.fromJson`.
- Reconnect with backoff on close/error (e.g. exponential from ~500ms,
  capped ~5s) — do not give up permanently; the daemon disconnects lagged
  clients by design (M4 Q6), and reconnecting yields a fresh snapshot,
  which `graph_provider`'s clear-and-repopulate handles correctly.
- Expose connection status (`connected` / `connecting` / `disconnected`)
  via a small separate `StateProvider` or enum notifier the control bar
  can watch independently of the message stream.

### Task 3 — Graph provider + applyDiff tests
Files: `lib/providers/graph_provider.dart`, `test/providers/graph_provider_test.dart`.

- As locked Q3. `NotifierProvider<GraphNotifier, int>` (or a hand-rolled
  `Notifier` exposing `revision` as its state) wrapping a private
  `Map<int, NodeDto> _nodes`.
- `applyDiff(GraphMessage msg)`: snapshot → `_nodes..clear()..addAll(...)`;
  diff → apply `add`/`update` (upsert by id) then `remove` (delete by id);
  increment `revision` by exactly 1 regardless of payload size (even an
  empty diff still represents "a message was processed" — confirm this
  against Task 2's stream: only call `applyDiff` for messages that actually
  arrived, so no revision bump on nothing received).
- Derived getters computed on demand (not cached): `orphanCount`,
  `liveNodeCount`, `totalLiveBytes` — iterate `_nodes.values` filtering
  `live` and/or `state == orphan`.
- Tests: snapshot replaces prior state entirely; diff add/update/remove
  semantics (update by id changes fields, doesn't duplicate; remove by id
  deletes; ids not present are no-ops, not errors); revision increments
  exactly once per `applyDiff` call; unknown `state` string on a raw JSON
  node → `NodeStateDto.healthy` (reuse Task 1's `fromWire` — do not
  reimplement the fallback here).

### Task 4 — Force layout + tests
Files: `lib/simulation/force_layout.dart`, `test/simulation/force_layout_test.dart`.

- As locked Q4. `SimNode { Vector2 position, velocity; double radius; double fade; }`
  (use `vector_math`'s `Vector2`).
- `ForceLayout` class owning `Map<int, SimNode> simNodes`, pure Dart (no
  `dart:ui`, no widget imports).
- `applyDiff(GraphMessage msg, Map<int, NodeDto> currentNodes)` or similar
  entry point: for each added node id, spawn a `SimNode` — near the owner's
  position if `currentNodes[ownerIdInferredFromEdges]` has a `SimNode`
  (a node's owner is whichever other node lists it in `edges`; look this
  up from the current node map, not from the diff alone), else near
  canvas center (`Vector2(centerX, centerY)`) with random jitter (small
  radius, e.g. ±20px). For each removed/freed node id: begin fade (do not
  delete immediately) — track fade progress on a per-tick clock, delete
  the `SimNode` once fade completes (~1s). Node radius = `clamp(sqrt(size),
  minRadius, maxRadius)` — pick sane constants (e.g. 4..40) and name them.
- `step(double dt)`: Verlet integration — pairwise repulsion (O(n²)),
  spring attraction along edges (rest length ~80), gentle gravity to
  center, velocity damping (~0.85/step). Advance fade timers here too.
- Aggregation (ENF9): if live node count > 500, switch to one aggregate
  `SimNode` per `symbol` — implementer's choice on exact mechanism, but it
  must be documented in a comment at the switch point and covered by a
  test proving it activates above 500 and not below/at 500.
- Tests: owner-present spawn lands within radius R of the owner's
  position; owner-absent spawn lands near center; a `NodeDto` field
  update (size change) never moves the corresponding `SimNode`'s position;
  a removed node fades then is deleted from `simNodes` (simulate several
  `step()` calls past the fade duration); aggregation activates above 500
  nodes.

### Task 5 — Graph canvas
Files: `lib/widgets/graph_canvas.dart`, `test/widgets/graph_canvas_test.dart`.

- `CustomPainter` reading `ForceLayout.simNodes` and the graph provider's
  node map (via `revision` watch) each repaint.
- Paint order: edges first (thin stroke, low alpha), then nodes on top.
- Color by `state`: `healthy` = teal fill, `orphan` = coral fill **plus** a
  pulsing ring (animate ring radius/alpha off the render clock, not the
  physics clock), `hot` = amber, `freed` = gray, alpha driven by the
  `SimNode`'s fade value.
- Hit-testing on tap: find the nearest `SimNode` within its radius of the
  tap point, set it as the selected node id (a small `StateProvider<int?>`
  or similar that `node_detail.dart` watches).
- Wrap the painter's widget in a `RepaintBoundary`.
- Test: widget builds and paints without exception given a fake/static
  provider override feeding a small graph; a tap at a known node's
  position updates the selection provider.

### Task 6 — Control bar
Files: `lib/widgets/control_bar.dart`.

- Connection status indicator (from Task 2's status provider).
- Live counters: node count, orphan count, total bytes (from Task 3's
  derived getters).
- Pause/resume: pausing stops calling `applyDiff` on incoming messages
  (drop-while-paused, per locked simplest-choice); resume resubscribes
  fresh (relies on reconnect-yields-snapshot semantics from Task 2, or
  simply stops discarding — implementer's call on the simplest correct
  wiring, document the choice inline).
- Min-size filter slider: a `StateProvider<double>` (bytes threshold)
  that `graph_canvas`/`memory_map` read to hide nodes below N bytes from
  rendering only — it must NOT filter the underlying state map.
- Orphan-only toggle: similar `StateProvider<bool>`, rendering-only filter.
- Symbol substring search: a `StateProvider<String>`, rendering-only
  filter (case-insensitive substring match against `symbol`).
- View toggle (graph canvas / memory map) — a `StateProvider<ViewMode>`
  enum that `main.dart`'s center panel reads to decide which widget to
  show.

### Task 7 — Memory map
Files: `lib/widgets/memory_map.dart`.

- Address-ordered grid: live nodes sorted by `ptr`, laid out into a fixed
  column count (e.g. responsive to available width), one cell per node —
  cell-area-proportional-to-size is explicitly NOT required for M5 (per
  spec: "one cell per node, ordered by ptr, is sufficient").
- Same state-color encoding as the graph canvas (reuse a shared color
  mapping function rather than duplicating the healthy/orphan/hot/freed
  → color logic — extract it to a small shared helper both widgets call).
- Respects the same rendering-only filters from Task 6 (min-size, orphan-
  only, symbol search).
- Tap a cell → same selection provider as Task 5's canvas.

### Task 8 — Node detail
Files: `lib/widgets/node_detail.dart`.

- Side panel bound to the selection provider from Task 5; renders nothing
  (or a placeholder) when no node is selected.
- Shows: `symbol`, `ptr` (hex, e.g. `0x...`), `size`, age (computed as
  `maxTsSeen - node.ts` — track a client-side rolling max of `ts` across
  all nodes/messages seen, analogous to the daemon's `max_ts_seen`; do not
  use wall-clock), `state`, owner (derive from other nodes' `edges`
  containing this id) / edge count.
- Size-over-time sparkline via `fl_chart`: maintain a bounded ring buffer
  (~120 samples) of `(ts, size)` recorded each time the *currently
  selected* node updates; feed it to a simple `LineChart`. Buffer must be
  keyed to the selected node and reset/discarded on selection change.

### Task 9 — Main wiring
Files: `lib/main.dart`.

- `ProviderScope` at the root, dark `ThemeData`.
- Layout: control bar (top), canvas/map center (toggled per Task 6's
  view-mode provider), node detail as a collapsible right panel (collapses
  when nothing is selected, or via an explicit toggle — implementer's
  call).
- Wire the `AnimationController` (60 fps) driving `graph_canvas` repaints
  and a physics driver (`Timer.periodic` or accumulated-dt inside the
  same ticker) calling `ForceLayout.step()` at ~30 Hz.

### Task 10 — Live end-to-end run (no new production code)

Not a subagent task — the controlling session runs this directly against
the real daemon + a producer example, since it requires this machine's
Flutter/Windows toolchain and live process orchestration:

1. `flutter analyze` clean, `flutter test` green (full suite).
2. Start `heaplens-daemon.exe`, run `wire_producer.exe` (or the capture
   helper pattern), `flutter run -d windows` against it.
3. Confirm: nodes appear in real time; ownership edges render; orphans
   turn coral and are visually distinguishable (drift is best-effort given
   the producer's short lifetime — the important assertion is the coral
   pulsing ring renders correctly when a state is orphan, whether reached
   naturally or observed via the same code path the contract test
   exercises); hot nodes amber; freed nodes fade. Memory map toggles and
   reflects the same state. Node detail shows a live sparkline for a
   selected node.
4. Kill and restart the daemon mid-session; confirm the app reconnects,
   receives a fresh snapshot, and shows no ghost nodes.
5. Report node counts, orphan visibility, and reconnect behavior.

## Acceptance for M5

- `flutter analyze` clean; `flutter test` green.
- Live run against the real daemon confirms rendering and reconnect
  behavior as in Task 10.
- No Rust changes on the branch (only `heaplens_flutter/` and this plan
  doc + ledger).

## Implementation order

Task 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10. `flutter analyze` after each
task. Stop after Task 10 (M5 complete) — do not begin Stage 6.
