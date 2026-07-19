import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/control.dart';
import '../models/graph_diff.dart';
import '../providers/force_layout_provider.dart';
import '../providers/graph_provider.dart';
import '../providers/ws_provider.dart';
import '../theme/xynorash_theme.dart';
import 'graph_canvas.dart' show GraphPainter;
import 'node_detail.dart';
import 'ui_common.dart';

/// The right rail: four always-on, non-collapsible sections, top to
/// bottom — Node detail, Size over time, Connection & render stats,
/// Verbose logs. No accordion/collapse behavior anywhere in this widget.
/// The last section (verbose logs) expands to fill any remaining vertical
/// space so the rail's content always reaches the bottom edge of the
/// window rather than stopping partway down.
class RightRail extends StatelessWidget {
  const RightRail({super.key});

  @override
  Widget build(BuildContext context) {
    // Four always-on sections (no collapsing). The verbose-log list can't
    // be given an "Expanded, stretch to fill remaining space" treatment
    // the naive way: that requires a bounded-height Column ancestor
    // established via IntrinsicHeight, and ListView explicitly does not
    // support intrinsic-dimension queries (it asserts if asked). Simpler,
    // robust alternative: the whole rail scrolls as one unit, and the
    // verbose-log section gets a generous minimum height so it reads as
    // "the log lives here and has real room", without the fragile
    // Expanded/IntrinsicHeight/ListView combination.
    return HudFrame(
      child: Container(
        key: const Key('rightRail'),
        width: 380,
        color: XynorashTheme.bgPanel,
        child: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: const [
              RailSection(title: 'Node detail', child: NodeDetail()),
              RailSection(title: 'Size over time', child: NodeSparklinePanel()),
              RailSection(
                title: 'Connection & render stats',
                muted: true,
                child: _ConnectionStatsPanel(),
              ),
              RailSection(
                title: 'Verbose logs',
                muted: true,
                child: SizedBox(height: 260, child: _VerboseLogsPanel()),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

/// Demoted developer-diagnostic block: the same WS/msgs/revision/live
/// nodes/sim nodes/sim bounds/last-paint data the (now opt-in-only)
/// standing debug overlay shows — this is the *only* place this data is
/// always visible; the floating green-text overlay that used to show it
/// unconditionally in every debug build has been removed (see
/// debug/debug_overlay.dart's `kShowDebugOverlay` doc).
class _ConnectionStatsPanel extends ConsumerStatefulWidget {
  const _ConnectionStatsPanel();

  @override
  ConsumerState<_ConnectionStatsPanel> createState() => _ConnectionStatsPanelState();
}

class _ConnectionStatsPanelState extends ConsumerState<_ConnectionStatsPanel> {
  int _snapshotCount = 0;
  int _diffCount = 0;
  Timer? _pollTimer;

  @override
  void initState() {
    super.initState();
    // Poll rather than watch a provider directly: `ForceLayout.simNodes`
    // and `GraphPainter.lastPaintAt` are mutated from deep inside
    // non-widget code (the physics step, the painter's paint()), the same
    // "watch a mutable field via a driven rebuild" pattern the old debug
    // overlay used.
    _pollTimer = Timer.periodic(const Duration(milliseconds: 500), (_) {
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

    final lastPaintAt = GraphPainter.lastPaintAt;
    final sincePaint = lastPaintAt == null
        ? 'never'
        : '${DateTime.now().difference(lastPaintAt).inMilliseconds}ms ago';

    final style = XynorashTheme.mono(fontSize: 10, color: Colors.white38);

    return DefaultTextStyle(
      style: style,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Text('WS: ${status.name}'),
          Text('msgs: snapshot=$_snapshotCount diff=$_diffCount'),
          Text('revision: $revision'),
          Text('live nodes: $liveNodeCount'),
          Text('sim nodes: $simNodeCount'),
          Text('sim bounds: $bounds'),
          Text('last paint: $sincePaint'),
        ],
      ),
    );
  }
}

/// Full-transparency event log: every step HeapLens takes from connection
/// start onward — WS connect/connecting/disconnect transitions, each
/// snapshot/diff received, attach/detach requests and their results,
/// target-exited pushes, and any decode/connection error. Not just "no
/// events yet" — the goal is a reader can follow everything that
/// happened, in order, with timestamps. Bounded scrollback so this
/// doesn't grow forever in a long-running session.
class _VerboseLogsPanel extends ConsumerStatefulWidget {
  const _VerboseLogsPanel();

  @override
  ConsumerState<_VerboseLogsPanel> createState() => _VerboseLogsPanelState();
}

class _VerboseLogsPanelState extends ConsumerState<_VerboseLogsPanel> {
  static const int _maxLines = 300;
  final List<String> _lines = [];
  final ScrollController _scrollController = ScrollController();
  ConnectionStatus? _lastStatus;

  String _timestamp() {
    final now = DateTime.now();
    String two(int n) => n.toString().padLeft(2, '0');
    return '${two(now.hour)}:${two(now.minute)}:${two(now.second)}.${now.millisecond.toString().padLeft(3, '0')}';
  }

  void _log(String line) {
    if (!mounted) return;
    setState(() {
      _lines.add('[${_timestamp()}] $line');
      if (_lines.length > _maxLines) _lines.removeAt(0);
    });
    // Keep the newest entry in view — this is a live tail, not a document
    // the reader is expected to scroll back through by default.
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scrollController.hasClients) {
        _scrollController.jumpTo(_scrollController.position.maxScrollExtent);
      }
    });
  }

  @override
  void dispose() {
    _scrollController.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    // WS connection lifecycle: connecting / connected / disconnected.
    final status = ref.watch(connectionStatusProvider);
    if (_lastStatus != status) {
      final previous = _lastStatus;
      _lastStatus = status;
      if (previous != null) {
        // Deferred: this build-phase transition must not synchronously
        // call setState on itself mid-build.
        WidgetsBinding.instance.addPostFrameCallback((_) {
          _log('WS: ${previous.name} -> ${status.name}');
        });
      } else {
        WidgetsBinding.instance.addPostFrameCallback((_) {
          _log('WS: ${status.name}');
        });
      }
    }

    // Every snapshot/diff, and any stream error (malformed frame, socket
    // error) surfaced as an AsyncValue.error.
    ref.listen<AsyncValue<GraphMessage>>(graphMessageProvider, (previous, next) {
      next.when(
        data: (message) {
          switch (message) {
            case GraphSnapshot snapshot:
              _log('snapshot received: ${snapshot.nodes.length} nodes');
            case GraphDiff diff:
              _log(
                'diff received: +${diff.add.length} add, ~${diff.update.length} update, -${diff.remove.length} remove',
              );
          }
        },
        error: (error, stackTrace) => _log('ERROR (graph stream): $error'),
        loading: () {},
      );
    });

    // Attach/detach requests' results, and the unprompted target-exited
    // push (Stage 7 §4.4).
    ref.listen<AsyncValue<ControlResponse>>(controlResponseProvider, (previous, next) {
      next.when(
        data: (resp) {
          switch (resp) {
            case ProcessListResponse list:
              _log('process list received: ${list.processes.length} processes');
            case AttachResultResponse result:
              _log(
                result.ok
                    ? 'attach succeeded: ${result.message}'
                    : 'attach FAILED: ${result.message}',
              );
            case DetachResultResponse result:
              _log(
                result.ok
                    ? 'detach succeeded: ${result.message}'
                    : 'detach FAILED: ${result.message}',
              );
            case TargetExitedResponse exited:
              _log('target exited: pid ${exited.pid}');
          }
        },
        error: (error, stackTrace) => _log('ERROR (control stream): $error'),
        loading: () {},
      );
    });

    if (_lines.isEmpty) {
      return const EmptyState(message: 'No events yet', icon: Icons.article_outlined);
    }

    return ListView.builder(
      key: const Key('verboseLogsList'),
      controller: _scrollController,
      itemCount: _lines.length,
      itemBuilder: (context, i) => Text(
        _lines[i],
        style: XynorashTheme.mono(fontSize: 10, color: Colors.white38),
      ),
    );
  }
}
