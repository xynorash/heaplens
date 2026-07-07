import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'debug/debug_overlay.dart';
import 'models/graph_diff.dart';
import 'providers/force_layout_provider.dart';
import 'providers/graph_provider.dart';
import 'providers/paused_provider.dart';
import 'providers/selection_provider.dart';
import 'providers/view_mode_provider.dart';
import 'providers/ws_provider.dart';
import 'widgets/control_bar.dart';
import 'widgets/graph_canvas.dart';
import 'widgets/memory_map.dart';
import 'widgets/node_detail.dart';

void main() {
  runApp(const ProviderScope(child: HeapLensApp()));
}

/// Root widget: `MaterialApp` with a dark theme, wrapping the whole app in
/// [_GraphOrchestrator] so the physics simulation is wired up regardless of
/// which page/route is showing.
class HeapLensApp extends StatelessWidget {
  const HeapLensApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'HeapLens',
      debugShowCheckedModeBanner: false,
      theme: ThemeData.dark(useMaterial3: true).copyWith(
        scaffoldBackgroundColor: const Color(0xFF121212),
      ),
      home: const _GraphOrchestrator(child: HeapLensHome()),
    );
  }
}

/// Owns the app-lifetime physics ticker and the raw-message -> [ForceLayout]
/// forwarding pipeline, then renders [child] beneath it.
///
/// ## Why a second, independent listener on `graphMessageProvider`
///
/// `graph_provider.dart`'s `GraphNotifier` already has its own internal
/// `ref.listen(graphMessageProvider, ...)` that mutates its node map and
/// bumps `revision`. [ForceLayout] needs to react to the exact same stream of
/// events (to spawn/update/fade `SimNode`s), but it cannot be driven by
/// diffing `graphProvider`'s revision changes, because that node map is
/// mutated in place — there is no "before" snapshot left to diff against
/// "after". So this widget sets up a *second*, independent
/// `ref.listen(graphMessageProvider, ...)` that receives every raw
/// [GraphMessage] as it arrives (in parallel with `graph_provider.dart`'s
/// own listener) and forwards the appropriate add/update/remove calls to the
/// shared [ForceLayout] from [forceLayoutProvider].
///
/// ## Listener-ordering hazard (read before touching this class)
///
/// For a `GraphDiff`'s `add` entries, [_ForceLayout.addNode] needs the
/// *current* full node map (post-this-diff) to find each new node's owner.
/// The brief instructs reading `ref.read(graphProvider.notifier).nodes` at
/// the moment this listener runs, on the assumption that
/// `graph_provider.dart`'s own listener for the same message has already run
/// (Riverpod invokes multiple listeners on one provider in the order they
/// were *registered*, not in some fixed "provider definition order"). That
/// assumption is only safe if `GraphNotifier`'s listener is registered
/// before this widget's — and widget build order does not guarantee that on
/// its own (this widget is built as the *parent* of the tree that contains
/// `ControlBar`, whose build is what normally first triggers
/// `GraphNotifier.build()`, so naively this widget's listener would actually
/// register FIRST and fire first, seeing stale state for that one message).
///
/// To make this deterministic rather than relying on incidental widget-tree
/// shape, [initState] force-reads `graphProvider` (via `ref.read`) before
/// this widget's own `build()` (and therefore its own `ref.listen` call)
/// ever runs. That guarantees `GraphNotifier.build()` — and hence its
/// internal `ref.listen` registration — happens first, every time,
/// regardless of where in the widget tree `ControlBar`/`GraphCanvas` end up.
/// This is intentional and load-bearing; do not remove the `ref.read
/// (graphProvider)` call in [initState] without re-checking this ordering
/// argument still holds.
///
/// ### Known residual risk: this guarantee is re-established only in
/// [initState], not on every rebuild
///
/// The ordering guarantee above holds at mount time, but it is not
/// re-verified afterwards, and there are (at least) two known ways for it to
/// silently stop holding for a *running* app:
///
/// 1. **`ref.invalidate(graphProvider)`** called from anywhere else in the
///    app after mount. Invalidating re-runs `GraphNotifier.build()`, which
///    re-registers its `ref.listen(graphMessageProvider, ...)` — and a
///    freshly (re-)registered listener goes to the *end* of the listener
///    list, after this orchestrator's already-registered listener. The next
///    message would then have this orchestrator's listener fire first, i.e.
///    with `graph_provider.dart` not yet updated for that message.
/// 2. **Hot reload (`debugReassemble()`), dev-only.** On hot reload, Riverpod
///    compares a source-hash of each provider's creation function; if
///    `GraphNotifier.build()`'s source changed (likely, since
///    `graph_provider.dart` is an actively-evolving file), Riverpod calls
///    `invalidateSelf()` on it as part of reassembly. That has the exact
///    same re-registration effect as case 1 above — `GraphNotifier`'s
///    listener moves to the end of the list, after this orchestrator's.
///    Critically, [initState] does **not** re-run on hot reload (only
///    `build()` does), so the fix this class relies on to establish the
///    ordering in the first place never gets a chance to re-run and restore
///    it.
///
/// In both cases the observable effect is limited to a single message: a
/// `GraphDiff`'s `add` node's owner lookup (`ref.read(graphProvider.notifier)
/// .nodes`) would see the pre-diff-application state instead of the
/// post-diff one, so that one new node would spawn positioned near the
/// canvas center instead of near its real owner. It self-corrects on the
/// very next message (registration order doesn't change again until another
/// invalidation/reload happens), it cannot occur in release builds (hot
/// reload does not exist there), and `ref.invalidate(graphProvider)` is not
/// currently called anywhere in this codebase — so this is a known,
/// accepted, cosmetic, dev-only limitation, not a production bug.
///
/// A more robust design exists that would remove this dependency on
/// listener-registration order entirely — e.g. having [_handleMessage] do
/// owner lookups against a locally-merged view of the node map (merging
/// `currentNodes` with the diff's own `add`/`update` entries before use)
/// instead of relying on `graph_provider.dart` having already applied the
/// message. That refactor is intentionally out of scope here; this comment
/// exists so a future maintainer who hits the symptom above doesn't have to
/// re-derive the cause from Riverpod's framework source.
class _GraphOrchestrator extends ConsumerStatefulWidget {
  const _GraphOrchestrator({required this.child});

  final Widget child;

  @override
  ConsumerState<_GraphOrchestrator> createState() => _GraphOrchestratorState();
}

class _GraphOrchestratorState extends ConsumerState<_GraphOrchestrator> {
  /// Physics tick rate, decoupled from `graph_canvas.dart`'s own 60fps
  /// render-clock `AnimationController` (used only for the pulsing-ring
  /// effect). ~30 Hz is plenty for smooth-looking force-directed motion.
  static const Duration _physicsInterval = Duration(milliseconds: 33);

  Timer? _physicsTimer;

  @override
  void initState() {
    super.initState();

    // Force `GraphNotifier.build()` to run now, registering its internal
    // `ref.listen(graphMessageProvider, ...)` before this widget's own
    // `build()` (and its own `ref.listen` call) executes. See the ordering
    // note on the class doc above for why this matters.
    ref.read(graphProvider);

    final layout = ref.read(forceLayoutProvider);
    final dtSeconds = _physicsInterval.inMicroseconds / 1e6;
    _physicsTimer = Timer.periodic(_physicsInterval, (_) {
      layout.step(dtSeconds);
    });
  }

  @override
  void dispose() {
    _physicsTimer?.cancel();
    super.dispose();
  }

  void _handleMessage(GraphMessage message) {
    // Mirror `graph_provider.dart`'s own pause gate exactly, so the physics
    // simulation and the graph state stay consistent while paused — if only
    // one side were gated, the visual would keep animating new nodes while
    // the control bar's counters stayed frozen (or vice versa).
    if (ref.read(pausedProvider)) return;

    final layout = ref.read(forceLayoutProvider);
    final currentNodes = ref.read(graphProvider.notifier).nodes;

    switch (message) {
      case GraphSnapshot snapshot:
        // Full replace: use the snapshot's own node list (turned into a map)
        // for owner lookups rather than `currentNodes`, so this doesn't
        // depend on `graph_provider.dart` having already applied this exact
        // snapshot — it's entirely self-contained.
        final snapshotMap = {for (final n in snapshot.nodes) n.id: n};
        layout.resetFrom(snapshot.nodes, snapshotMap);
      case GraphDiff diff:
        for (final n in diff.add) {
          layout.addNode(n, currentNodes);
        }
        // Note: an `update` whose `NodeDto.state == NodeStateDto.freed`
        // intentionally does not trigger `layout.removeNode` here — only a
        // `remove` (handled below) or a fresh `resetFrom`-driven cleanup
        // does. This is currently a non-issue in practice: as of this
        // writing, heaplens-daemon's node-state machine
        // (crates/heaplens-daemon/src/anomaly.rs) only ever assigns
        // `NodeState::Healthy`, `::Orphan`, or `::Hot` — `NodeState::Freed`
        // is defined on the wire (heaplens-protocol/src/diff.rs) but nothing
        // in the daemon ever constructs it for a live node's `update`. A
        // `freed` node's lifecycle is expected to always end via a `remove`
        // diff entry instead. If the daemon's state machine changes to emit
        // `freed` on an `update`, this call site would need to start
        // treating that as a fade trigger too (calling `layout.removeNode`
        // for it) rather than leaving it solid and non-fading forever.
        for (final n in diff.update) {
          layout.updateNode(n);
        }
        for (final id in diff.remove) {
          layout.removeNode(id);
        }
    }
  }

  @override
  Widget build(BuildContext context) {
    ref.listen<AsyncValue<GraphMessage>>(graphMessageProvider, (previous, next) {
      next.whenData(_handleMessage);
    });
    return widget.child;
  }
}

/// Top-level page layout: control bar across the top, the graph canvas or
/// memory map filling the center (toggled by [viewModeProvider]), and a
/// collapsible node-detail panel on the right (hidden entirely when nothing
/// is selected).
class HeapLensHome extends ConsumerWidget {
  const HeapLensHome({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final viewMode = ref.watch(viewModeProvider);
    final selectedId = ref.watch(selectedNodeIdProvider);
    final layout = ref.watch(forceLayoutProvider);

    return Scaffold(
      body: SafeArea(
        child: Stack(
          children: [
            Column(
              children: [
                const ControlBar(),
                Expanded(
                  child: Row(
                    children: [
                      Expanded(
                        child: switch (viewMode) {
                          ViewMode.graph => GraphCanvas(layout: layout),
                          ViewMode.memoryMap => const MemoryMap(),
                        },
                      ),
                      if (selectedId != null)
                        Container(
                          key: const Key('nodeDetailPanel'),
                          width: 320,
                          color: const Color(0xFF1A1A1A),
                          child: const NodeDetail(),
                        ),
                    ],
                  ),
                ),
              ],
            ),
            if (kShowDebugOverlay) const DebugOverlay(),
          ],
        ),
      ),
    );
  }
}
