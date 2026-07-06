import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:vector_math/vector_math.dart' show Vector2;

import '../models/node.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import '../simulation/force_layout.dart';
import 'node_colors.dart';

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

    // Only select the node if it exists in the current node map.
    // A SimNode can transiently exist without a matching NodeDto during
    // diff application (e.g., mid-fade-out after removal). Avoid selecting
    // a ghost id that has no NodeDto backing.
    if (bestId != null) {
      final nodes = ref.read(graphProvider.notifier).nodes;
      if (nodes.containsKey(bestId)) {
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

    return RepaintBoundary(
      child: LayoutBuilder(
        builder: (context, constraints) {
          return GestureDetector(
            onTapUp: (details) => _handleTapUp(details, constraints),
            child: AnimatedBuilder(
              animation: _pulseController,
              builder: (context, _) {
                return CustomPaint(
                  size: constraints.biggest,
                  painter: GraphPainter(
                    simNodes: widget.layout.simNodes,
                    nodes: nodes,
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

  @override
  void paint(Canvas canvas, Size size) {
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
  }

  @override
  bool shouldRepaint(covariant GraphPainter oldDelegate) => true;
}
