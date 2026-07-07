import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:vector_math/vector_math.dart' show Vector2;

import '../models/node.dart';
import '../providers/filter_providers.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import '../simulation/force_layout.dart';
import 'node_colors.dart';

/// Rendering-only filter check shared by [GraphCanvas]'s paint/hit-test path
/// and `memory_map.dart`'s grid — kept in exact lockstep with the filter
/// semantics in `filter_providers.dart` (case-insensitive substring search,
/// etc.) so the two views behave identically for the same filter values.
/// Never touches the underlying node map — callers simply skip nodes this
/// returns `false` for.
bool _passesRenderFilters(
  NodeDto node, {
  required double minSize,
  required bool orphanOnly,
  required String symbolSearchLower,
}) {
  if (!node.live) return false;
  if (node.size < minSize) return false;
  if (orphanOnly && node.state != NodeStateDto.orphan) return false;
  if (symbolSearchLower.isNotEmpty &&
      !node.symbol.toLowerCase().contains(symbolSearchLower)) {
    return false;
  }
  return true;
}

/// Renders the live memory-ownership graph: [ForceLayout.simNodes] positions
/// cross-referenced with [NodeDto]s (state, edges) from the graph provider's
/// node map.
///
/// This widget does not tick the physics simulation itself (a later task
/// owns the shared [ForceLayout] instance and its render-loop `Ticker`) — it
/// only reads [layout.simNodes] on each repaint and rebuilds whenever the
/// graph provider's revision changes. It does drive its own render-clock
/// [AnimationController] for the orphan pulsing-ring effect, independent of
/// the physics clock.
///
/// This is the default/primary view (see `viewModeProvider`), so — exactly
/// like `memory_map.dart` — it applies the Task 6 rendering-only filters
/// ([minSizeFilterProvider], [orphanOnlyFilterProvider],
/// [symbolSearchFilterProvider]) to what it paints and hit-tests: filtered
/// nodes are omitted from both the painter's node map and tap selection, but
/// never touched in the underlying `graphProvider` node map or in
/// [ForceLayout.simNodes] itself.
class GraphCanvas extends ConsumerStatefulWidget {
  const GraphCanvas({super.key, required this.layout});

  /// Shared force-directed layout instance. Ownership/ticking of this
  /// instance is out of scope here — the caller supplies it (directly, or
  /// via its own provider) and is responsible for calling `step()`.
  final ForceLayout layout;

  @override
  ConsumerState<GraphCanvas> createState() => _GraphCanvasState();
}

class _GraphCanvasState extends ConsumerState<GraphCanvas>
    with SingleTickerProviderStateMixin {
  late final AnimationController _pulseController;

  @override
  void initState() {
    super.initState();
    _pulseController = AnimationController(
      vsync: this,
      duration: const Duration(milliseconds: 1200),
    )..repeat();
  }

  @override
  void dispose() {
    _pulseController.dispose();
    super.dispose();
  }

  void _handleTapUp(TapUpDetails details, BoxConstraints constraints) {
    final tapPos = Vector2(
      details.localPosition.dx,
      details.localPosition.dy,
    );

    int? bestId;
    var bestDist = double.infinity;
    for (final entry in widget.layout.simNodes.entries) {
      final sim = entry.value;
      final dist = (sim.position - tapPos).length;
      if (dist <= sim.radius && dist < bestDist) {
        bestDist = dist;
        bestId = entry.key;
      }
    }

    // Only select the node if it exists in the current node map AND passes
    // the current Task 6 rendering filters. A SimNode can transiently exist
    // without a matching NodeDto during diff application (e.g.,
    // mid-fade-out after removal) — avoid selecting a ghost id that has no
    // NodeDto backing. A node hidden by a filter shouldn't be selectable via
    // tap either, since it isn't visible to tap "on".
    if (bestId != null) {
      final nodes = ref.read(graphProvider.notifier).nodes;
      final node = nodes[bestId];
      if (node != null &&
          _passesRenderFilters(
            node,
            minSize: ref.read(minSizeFilterProvider),
            orphanOnly: ref.read(orphanOnlyFilterProvider),
            symbolSearchLower:
                ref.read(symbolSearchFilterProvider).toLowerCase(),
          )) {
        ref.read(selectedNodeIdProvider.notifier).state = bestId;
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    // Watched purely to know *when* to rebuild/repaint; read the fresh node
    // map below rather than caching it across revisions.
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final selectedId = ref.watch(selectedNodeIdProvider);

    // Task 6 rendering-only filters (see filter_providers.dart):
    // memory_map.dart already applies these to what it draws; the graph view
    // is the default/primary view (see viewModeProvider) and must match, or
    // the controls silently do nothing while a user is looking at this view.
    final minSize = ref.watch(minSizeFilterProvider);
    final orphanOnly = ref.watch(orphanOnlyFilterProvider);
    final symbolSearchLower =
        ref.watch(symbolSearchFilterProvider).toLowerCase();

    final visibleNodes = <int, NodeDto>{
      for (final entry in nodes.entries)
        if (_passesRenderFilters(
          entry.value,
          minSize: minSize,
          orphanOnly: orphanOnly,
          symbolSearchLower: symbolSearchLower,
        ))
          entry.key: entry.value,
    };

    return RepaintBoundary(
      child: LayoutBuilder(
        builder: (context, constraints) {
          return GestureDetector(
            onTapUp: (details) => _handleTapUp(details, constraints),
            child: AnimatedBuilder(
              animation: _pulseController,
              builder: (context, _) {
                return CustomPaint(
                  key: const Key('graphCanvasPaint'),
                  size: constraints.biggest,
                  painter: GraphPainter(
                    simNodes: widget.layout.simNodes,
                    nodes: visibleNodes,
                    pulseValue: _pulseController.value,
                    selectedId: selectedId,
                  ),
                );
              },
            ),
          );
        },
      ),
    );
  }
}

/// Paints edges (thin, low-alpha strokes) followed by nodes (colored by
/// [NodeDto.state], with a pulsing ring for `orphan` and fade-driven alpha
/// for `freed`), cross-referencing [simNodes] and [nodes] by node id.
///
/// [nodes] is expected to already be filtered down to whatever a caller
/// wants rendered/hit-testable (see `GraphCanvas`'s Task 6 filter wiring);
/// this painter itself has no filtering opinion. Separately, it also paints
/// any [simNodes] entry mid-fade-out that no longer has a [nodes] entry at
/// all (see [_paintFadingGhosts]) — those are never subject to filtering
/// since there's no live [NodeDto] left to filter on.
class GraphPainter extends CustomPainter {
  GraphPainter({
    required this.simNodes,
    required this.nodes,
    required this.pulseValue,
    required this.selectedId,
  });

  final Map<int, SimNode> simNodes;
  final Map<int, NodeDto> nodes;

  /// 0.0..1.0, looping render-clock phase driving the orphan pulsing ring.
  final double pulseValue;

  final int? selectedId;

  static final Paint _edgePaint = Paint()
    ..color = const Color(0x33FFFFFF)
    ..strokeWidth = 1.0
    ..style = PaintingStyle.stroke;

  /// Wall-clock time of the most recent [paint] call. Diagnostic-only (see
  /// `debug_overlay.dart`, fix/canvas-render branch): lets an on-screen
  /// overlay show whether the paint pipeline is actually being driven at
  /// all, independent of whether anything currently visible gets drawn.
  static DateTime? lastPaintAt;

  @override
  void paint(Canvas canvas, Size size) {
    lastPaintAt = DateTime.now();
    _paintEdges(canvas);
    _paintNodes(canvas);
  }

  void _paintEdges(Canvas canvas) {
    for (final entry in nodes.entries) {
      final self = simNodes[entry.key];
      if (self == null) continue;
      for (final targetId in entry.value.edges) {
        final target = simNodes[targetId];
        if (target == null) continue;
        canvas.drawLine(
          Offset(self.position.x, self.position.y),
          Offset(target.position.x, target.position.y),
          _edgePaint,
        );
      }
    }
  }

  void _paintNodes(Canvas canvas) {
    for (final entry in nodes.entries) {
      final id = entry.key;
      final node = entry.value;
      final sim = simNodes[id];
      if (sim == null) continue;

      final center = Offset(sim.position.x, sim.position.y);
      final baseColor = colorForState(node.state);
      final alpha = node.state == NodeStateDto.freed ? sim.fade : 1.0;

      final fillPaint = Paint()
        ..color = baseColor.withValues(alpha: alpha.clamp(0.0, 1.0));
      canvas.drawCircle(center, sim.radius, fillPaint);

      if (node.state == NodeStateDto.orphan) {
        // Pulsing ring: radius grows outward from the node's edge as
        // pulseValue sweeps 0->1, alpha fading out over the same sweep.
        final ringRadius = sim.radius + pulseValue * (sim.radius * 0.8 + 6);
        final ringAlpha = (1.0 - pulseValue).clamp(0.0, 1.0);
        final ringPaint = Paint()
          ..color = baseColor.withValues(alpha: ringAlpha)
          ..style = PaintingStyle.stroke
          ..strokeWidth = 2.0;
        canvas.drawCircle(center, ringRadius, ringPaint);
      }

      if (id == selectedId) {
        final selectionPaint = Paint()
          ..color = Colors.white
          ..style = PaintingStyle.stroke
          ..strokeWidth = 2.0;
        canvas.drawCircle(center, sim.radius + 3, selectionPaint);
      }
    }

    _paintFadingGhosts(canvas);
  }

  /// Paints nodes whose `NodeDto` has already been removed from [nodes] but
  /// whose [SimNode] is still mid-fade in [simNodes] — the normal case for
  /// every `remove` diff, since the orchestrator in main.dart calls
  /// `ForceLayout.removeNode` in the exact same step that `graph_provider`
  /// deletes the id from its own map (see main.dart's `_handleMessage`).
  ///
  /// Without this pass, [SimNode.fade] would count down entirely off-screen:
  /// the loop above requires a live [NodeDto] to paint anything, and by the
  /// time `fade` has dropped below 1.0 the `NodeDto` is already long gone.
  /// [SimNode.lastKnownState] (captured by `ForceLayout.removeNode` from the
  /// last state it was told about, before that data was discarded) is what
  /// makes this possible without a live `NodeDto`.
  void _paintFadingGhosts(Canvas canvas) {
    for (final entry in simNodes.entries) {
      final id = entry.key;
      if (nodes.containsKey(id)) continue; // already painted above
      final sim = entry.value;
      final lastState = sim.lastKnownState;
      if (lastState == null) continue; // not a fading ghost

      final center = Offset(sim.position.x, sim.position.y);
      final baseColor = colorForState(lastState);
      final fillPaint = Paint()
        ..color = baseColor.withValues(alpha: sim.fade.clamp(0.0, 1.0));
      canvas.drawCircle(center, sim.radius, fillPaint);
    }
  }

  @override
  bool shouldRepaint(covariant GraphPainter oldDelegate) => true;
}
