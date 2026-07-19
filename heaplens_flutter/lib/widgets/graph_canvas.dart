import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:vector_math/vector_math.dart' show Vector2;

import '../models/node.dart';
import '../providers/filter_providers.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import '../simulation/force_layout.dart';
import '../theme/xynorash_theme.dart';
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
  final TransformationController _transformController = TransformationController();

  static const double _minScale = 0.5;
  static const double _maxScale = 3.0;
  static const double _zoomStep = 1.25;

  double get _currentScale => _transformController.value.getMaxScaleOnAxis();

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
    _transformController.dispose();
    super.dispose();
  }

  void _zoom(double factor) {
    final target = (_currentScale * factor).clamp(_minScale, _maxScale);
    final applied = target / _currentScale;
    if (applied == 1.0) return;
    // Anchor the zoom on the current viewport center (not the canvas
    // origin) so zooming in/out feels like it's centered on what's
    // actually visible, rather than dragging everything toward the
    // virtual canvas's top-left corner.
    final viewportSize = context.size ?? Size.zero;
    final centerViewport = Offset(viewportSize.width / 2, viewportSize.height / 2);
    final centerScene = _transformController.toScene(centerViewport);
    final updated = _transformController.value.clone()
      ..translateByDouble(centerScene.dx, centerScene.dy, 0, 1)
      ..scaleByDouble(applied, applied, applied, 1)
      ..translateByDouble(-centerScene.dx, -centerScene.dy, 0, 1);
    setState(() => _transformController.value = updated);
  }

  void _resetZoom() {
    // Restore the centered view (gravity well at viewport center), not
    // raw identity — identity would snap back to the same off-center
    // position `_scheduleInitialCentering` exists to fix.
    setState(
      () => _transformController.value = _initialTransform ?? Matrix4.identity(),
    );
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

    // The canvas is a fixed virtual size, larger than any typical viewport,
    // panned via InteractiveViewer rather than clipped to whatever screen
    // space happens to be available — previously the CustomPaint was sized
    // to exactly the viewport (`constraints.biggest`), so any node the
    // force layout pushed outside that rectangle was simply unreachable:
    // clipped, with no way to scroll to it. `constrained: false` lets the
    // child be genuinely bigger than the viewport; `panEnabled: true` gives
    // free two-axis panning. `scaleEnabled: false` disables *gesture*
    // zoom (pinch/trackpad/scroll-wheel) specifically — zoom is driven
    // only by the explicit +/-/reset buttons below, via
    // `_transformController`, so it stays predictable and doesn't fight
    // two-axis panning gestures. `minScale`/`maxScale` still have to match
    // the buttons' own clamp range: InteractiveViewer clamps any value
    // assigned to its controller into its own configured bounds, so
    // leaving these at 1.0/1.0 (as when zoom was out of scope) would
    // silently discard the buttons' work.
    return RepaintBoundary(
      child: LayoutBuilder(
        builder: (context, viewportConstraints) {
          _scheduleCentering(viewportConstraints.biggest);
          return _buildViewer(viewportConstraints, visibleNodes, selectedId);
        },
      ),
    );
  }

  /// The force layout's gravity well sits at a fixed point
  /// ([ForceLayout.centerX]/`centerY`, not viewport-relative) — on a wide
  /// window, that point can be much closer to the virtual canvas's left
  /// edge than the actual visible viewport's center, so the graph reads
  /// as pushed left instead of centered (confirmed against a real
  /// screenshot: a ~1540px-wide graph area with the gravity well fixed at
  /// x=400 put the whole cluster in roughly the left quarter of the
  /// space).
  ///
  /// Originally this only centered once, on first layout — but resizing
  /// the window afterward left the *old* fixed pixel offset in place, so
  /// growing the window drifted the graph back off-center (the offset
  /// that centered a 1540px-wide viewport doesn't center a 1900px-wide
  /// one). Fixed: re-run whenever the viewport's *size actually changes*
  /// (tracked via [_lastCenteredSize]), not just on the first build. This
  /// does mean a resize re-centers the view even if the user had panned
  /// away from center — a deliberate trade-off (predictable centering on
  /// resize, matching what was asked for) over preserving an arbitrary
  /// pan position across a size change.
  Size? _lastCenteredSize;
  Matrix4? _initialTransform;

  void _scheduleCentering(Size viewportSize) {
    if (viewportSize.isEmpty || viewportSize == _lastCenteredSize) return;
    _lastCenteredSize = viewportSize;
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (!mounted) return;
      final dx = viewportSize.width / 2 - widget.layout.centerX;
      final dy = viewportSize.height / 2 - widget.layout.centerY;
      final transform = Matrix4.translationValues(dx, dy, 0);
      _initialTransform = transform;
      setState(() => _transformController.value = transform);
    });
  }

  Widget _buildViewer(
    BoxConstraints viewportConstraints,
    Map<int, NodeDto> visibleNodes,
    int? selectedId,
  ) {
    return Stack(
        children: [
          InteractiveViewer(
            key: const Key('graphScrollView'),
            transformationController: _transformController,
            constrained: false,
            panEnabled: true,
            scaleEnabled: false,
            boundaryMargin: const EdgeInsets.all(400),
            minScale: _minScale,
            maxScale: _maxScale,
            child: SizedBox(
              width: kGraphVirtualWidth,
              height: kGraphVirtualHeight,
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
            ),
          ),
          Positioned(
            right: 12,
            bottom: 12,
            child: _ZoomControls(
              onZoomIn: () => _zoom(_zoomStep),
              onZoomOut: () => _zoom(1 / _zoomStep),
              onReset: _resetZoom,
            ),
          ),
        ],
      );
  }
}

class _ZoomControls extends StatelessWidget {
  const _ZoomControls({required this.onZoomIn, required this.onZoomOut, required this.onReset});

  final VoidCallback onZoomIn;
  final VoidCallback onZoomOut;
  final VoidCallback onReset;

  @override
  Widget build(BuildContext context) {
    return Container(
      decoration: BoxDecoration(
        color: Colors.black.withValues(alpha: 0.55),
        borderRadius: BorderRadius.circular(8),
        border: Border.all(color: Colors.white24),
      ),
      child: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          IconButton(
            key: const Key('zoomInButton'),
            tooltip: 'Zoom in',
            icon: const Icon(Icons.add, size: 18, color: Colors.white70),
            onPressed: onZoomIn,
          ),
          IconButton(
            key: const Key('zoomOutButton'),
            tooltip: 'Zoom out',
            icon: const Icon(Icons.remove, size: 18, color: Colors.white70),
            onPressed: onZoomOut,
          ),
          IconButton(
            key: const Key('zoomResetButton'),
            tooltip: 'Reset zoom',
            icon: const Icon(Icons.center_focus_strong, size: 18, color: Colors.white70),
            onPressed: onReset,
          ),
        ],
      ),
    );
  }
}

/// Virtual canvas dimensions the graph is painted onto — generously larger
/// than a typical window so a spread-out simulation (many disconnected
/// roots, per the overlap fix) has real room, panned into view via
/// [InteractiveViewer] rather than clipped. [ForceLayout]'s default
/// gravity well sits at (400, 300); these are sized well beyond that in
/// both directions so the graph can spread without immediately hitting an
/// edge.
const double kGraphVirtualWidth = 1600;
const double kGraphVirtualHeight = 1200;

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

  // Circuit-trace tinted, not plain white — ties the graph's own edges
  // into the same cyan HUD signal color as everything else, instead of a
  // generic neutral line.
  static final Paint _edgePaint = Paint()
    ..color = XynorashTheme.cyan.withValues(alpha: 0.16)
    ..strokeWidth = 1.0
    ..style = PaintingStyle.stroke;

  static final Paint _gridPaint = Paint()
    ..color = XynorashTheme.cyan.withValues(alpha: 0.035)
    ..strokeWidth = 1.0;

  /// Spacing (px) of the faint background HUD grid — a scale reference for
  /// the canvas, the same reason a cockpit display or oscilloscope grids
  /// its background, not decoration for its own sake.
  static const double _gridSpacing = 48.0;

  /// Wall-clock time of the most recent [paint] call. Diagnostic-only (see
  /// `debug_overlay.dart`, fix/canvas-render branch): lets an on-screen
  /// overlay show whether the paint pipeline is actually being driven at
  /// all, independent of whether anything currently visible gets drawn.
  static DateTime? lastPaintAt;

  @override
  void paint(Canvas canvas, Size size) {
    lastPaintAt = DateTime.now();
    _paintGrid(canvas, size);
    _paintEdges(canvas);
    _paintNodes(canvas);
  }

  void _paintGrid(Canvas canvas, Size size) {
    for (var x = 0.0; x < size.width; x += _gridSpacing) {
      canvas.drawLine(Offset(x, 0), Offset(x, size.height), _gridPaint);
    }
    for (var y = 0.0; y < size.height; y += _gridSpacing) {
      canvas.drawLine(Offset(0, y), Offset(size.width, y), _gridPaint);
    }
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
      final clampedAlpha = alpha.clamp(0.0, 1.0);

      // Soft neon bloom behind the node — a blurred, larger, dimmer copy
      // of the fill color underneath the crisp circle. This is what turns
      // "a filled circle" into "a HUD blip"; drawn first so the crisp
      // fill on top stays sharp.
      final glowPaint = Paint()
        ..color = baseColor.withValues(alpha: 0.30 * clampedAlpha)
        ..maskFilter = MaskFilter.blur(BlurStyle.normal, sim.radius * 0.55);
      canvas.drawCircle(center, sim.radius * 1.1, glowPaint);

      final fillPaint = Paint()..color = baseColor.withValues(alpha: clampedAlpha);
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
        // Selection ring glows cyan (the app's own "this is active" HUD
        // signal color) rather than plain white, so a selected node reads
        // as "focused" the same way a focused control does everywhere
        // else in the app.
        final selectionGlow = Paint()
          ..color = XynorashTheme.cyan.withValues(alpha: 0.5)
          ..maskFilter = const MaskFilter.blur(BlurStyle.normal, 4);
        canvas.drawCircle(
          center,
          sim.radius + 3,
          selectionGlow
            ..style = PaintingStyle.stroke
            ..strokeWidth = 2.0,
        );
        final selectionPaint = Paint()
          ..color = XynorashTheme.cyan
          ..style = PaintingStyle.stroke
          ..strokeWidth = 1.6;
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
