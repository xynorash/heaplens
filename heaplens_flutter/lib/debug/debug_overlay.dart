import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/graph_diff.dart';
import '../providers/force_layout_provider.dart';
import '../providers/graph_provider.dart';
import '../providers/ws_provider.dart';
import '../widgets/graph_canvas.dart' show GraphPainter;

/// Whether [DebugOverlay] should be built. On by default in debug builds;
/// can be forced on in a release/profile build via
/// `--dart-define=HEAPLENS_DEBUG_OVERLAY=true` — useful precisely for
/// investigating a symptom (like a blank canvas) that might not reproduce
/// under `flutter run`'s debug-mode overhead.
const bool kShowDebugOverlay =
    kDebugMode || bool.fromEnvironment('HEAPLENS_DEBUG_OVERLAY');

/// Standing on-screen diagnostic instrument (added for the fix/canvas-render
/// investigation; kept for every future visual gate, not removed once this
/// bug is fixed). Makes every stage of the daemon -> canvas pipeline
/// observable directly on screen, without logs or screenshots, so a human
/// watching the live app can localize a break to one of five seams: WS
/// connection, message arrival, provider revision, SimNode population, or
/// paint execution.
///
/// [GraphPainter.lastPaintAt] and [ForceLayout.simNodes]'s length are
/// mutated from deep inside non-widget code (the painter's `paint()`, the
/// physics step) where threading a Riverpod provider through would be
/// invasive for a diagnostic tool — this widget instead polls them on a
/// short timer and forces its own rebuild, the same "watch a mutable field
/// via a driven rebuild" pattern `graph_canvas.dart` already uses for its
/// pulse animation.
class DebugOverlay extends ConsumerStatefulWidget {
  const DebugOverlay({super.key});

  @override
  ConsumerState<DebugOverlay> createState() => _DebugOverlayState();
}

class _DebugOverlayState extends ConsumerState<DebugOverlay> {
  int _snapshotCount = 0;
  int _diffCount = 0;
  Timer? _pollTimer;

  @override
  void initState() {
    super.initState();
    _pollTimer = Timer.periodic(const Duration(milliseconds: 200), (_) {
      if (mounted) setState(() {});
    });
  }

  @override
  void dispose() {
    _pollTimer?.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // A third independent listener on the same message stream, purely to
    // count arrivals for display here. Multiple listeners on
    // graphMessageProvider are already an established, safe pattern in this
    // codebase — see main.dart's _GraphOrchestrator class doc.
    ref.listen<AsyncValue<GraphMessage>>(graphMessageProvider, (previous, next) {
      next.whenData((message) {
        setState(() {
          switch (message) {
            case GraphSnapshot _:
              _snapshotCount++;
            case GraphDiff _:
              _diffCount++;
          }
        });
      });
    });

    final status = ref.watch(connectionStatusProvider);
    final revision = ref.watch(graphProvider);
    final liveNodeCount = ref.read(graphProvider.notifier).liveNodeCount;
    final layout = ref.watch(forceLayoutProvider);
    final simNodeCount = layout.simNodes.length;

    final xs = layout.simNodes.values.map((s) => s.position.x);
    final ys = layout.simNodes.values.map((s) => s.position.y);
    final bounds = simNodeCount == 0
        ? 'n/a'
        : 'x[${xs.reduce((a, b) => a < b ? a : b).toStringAsFixed(0)}, '
            '${xs.reduce((a, b) => a > b ? a : b).toStringAsFixed(0)}] '
            'y[${ys.reduce((a, b) => a < b ? a : b).toStringAsFixed(0)}, '
            '${ys.reduce((a, b) => a > b ? a : b).toStringAsFixed(0)}]';
    final hasNaN = xs.any((v) => v.isNaN) || ys.any((v) => v.isNaN);

    final lastPaintAt = GraphPainter.lastPaintAt;
    final sincePaint = lastPaintAt == null
        ? 'never'
        : '${DateTime.now().difference(lastPaintAt).inMilliseconds}ms ago';

    return Positioned(
      right: 8,
      bottom: 8,
      child: IgnorePointer(
        child: Container(
          padding: const EdgeInsets.all(8),
          decoration: BoxDecoration(
            color: Colors.black.withValues(alpha: 0.75),
            borderRadius: BorderRadius.circular(6),
            border: Border.all(color: Colors.white24),
          ),
          child: DefaultTextStyle(
            style: const TextStyle(
              color: Colors.greenAccent,
              fontSize: 11,
              fontFamily: 'monospace',
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              mainAxisSize: MainAxisSize.min,
              children: [
                Text('WS: ${status.name}'),
                Text('msgs: snapshot=$_snapshotCount diff=$_diffCount'),
                Text('revision: $revision'),
                Text('live nodes: $liveNodeCount'),
                Text('simNodes: $simNodeCount'),
                Text('sim bounds: $bounds${hasNaN ? '  !! NaN !!' : ''}'),
                Text('last paint: $sincePaint'),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
