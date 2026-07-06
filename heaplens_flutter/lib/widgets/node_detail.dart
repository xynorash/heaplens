import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/node.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import 'node_colors.dart';

/// Bounded ring-buffer capacity for the size-over-time sparkline.
const int kSparklineCapacity = 120;

/// One (ts, size) sample recorded for the sparkline.
typedef SparklineSample = ({int ts, int size});

/// Bounded ring buffer of [SparklineSample]s for a single node, keyed by
/// that node's id. Exposed as its own class (rather than inlined private
/// state in [NodeDetail]) so its reset/bound-growth behavior can be unit
/// tested directly, without driving the whole widget tree.
class SparklineBuffer {
  int? _nodeId;
  final List<SparklineSample> _samples = [];

  /// Which node id this buffer's samples belong to, or `null` if empty/unset.
  int? get nodeId => _nodeId;

  /// Read-only view of the currently buffered samples, oldest first.
  List<SparklineSample> get samples => List.unmodifiable(_samples);

  /// Records a sample for [node]. If [node.id] differs from the id the
  /// buffer currently tracks, the buffer is reset (old samples discarded)
  /// before recording. Consecutive samples with an unchanged `size` are
  /// deduplicated (only the first is kept) so the sparkline reflects actual
  /// size changes rather than every message. Bounded to
  /// [kSparklineCapacity] entries; oldest is evicted once full.
  void record(NodeDto node) {
    if (_nodeId != node.id) {
      _nodeId = node.id;
      _samples.clear();
    }
    if (_samples.isEmpty || _samples.last.size != node.size) {
      _samples.add((ts: node.ts, size: node.size));
      if (_samples.length > kSparklineCapacity) {
        _samples.removeAt(0);
      }
    }
  }

  /// Discards all samples and forgets which node they belonged to.
  void reset() {
    _nodeId = null;
    _samples.clear();
  }
}

/// Side panel showing full detail for the currently selected node
/// ([selectedNodeIdProvider]): symbol, ptr (hex), size, age, state, owner,
/// edge count, and a size-over-time sparkline.
///
/// Renders a placeholder when nothing is selected, or when the selected id
/// no longer exists in the node map (e.g. it was removed by a `remove`
/// diff) — never crashes on a stale selection.
///
/// This is a `StatefulWidget` (rather than adding a new Riverpod provider)
/// because the sparkline ring buffer is transient, per-selection UI state
/// that's naturally scoped to this widget's lifetime: it must reset the
/// instant the selection changes, and nothing else in the app needs to read
/// it. Keeping it local avoids adding provider surface for state with a
/// single reader.
class NodeDetail extends ConsumerStatefulWidget {
  const NodeDetail({super.key});

  @override
  ConsumerState<NodeDetail> createState() => _NodeDetailState();
}

class _NodeDetailState extends ConsumerState<NodeDetail> {
  /// Ring buffer of (ts, size) samples for the currently selected node.
  /// Keyed/reset internally by [SparklineBuffer.record] whenever the node id
  /// it's fed changes.
  final SparklineBuffer _buffer = SparklineBuffer();

  @override
  Widget build(BuildContext context) {
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final selectedId = ref.watch(selectedNodeIdProvider);

    if (selectedId == null || !nodes.containsKey(selectedId)) {
      // Selection cleared, or the selected node no longer exists in state
      // (e.g. a `remove` diff deleted it) -- discard any stale buffer and
      // show a placeholder rather than risk a null lookup.
      _buffer.reset();
      return const _NoSelectionPlaceholder();
    }

    final node = nodes[selectedId]!;
    _buffer.record(node);

    int maxTsSeen = node.ts;
    int? ownerId;
    for (final n in nodes.values) {
      if (n.ts > maxTsSeen) maxTsSeen = n.ts;
      if (n.edges.contains(selectedId)) ownerId = n.id;
    }
    final age = maxTsSeen - node.ts;

    return Padding(
      padding: const EdgeInsets.all(12),
      child: SingleChildScrollView(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          mainAxisSize: MainAxisSize.min,
          children: [
            Text(
              node.symbol,
              style: Theme.of(
                context,
              ).textTheme.titleMedium?.copyWith(fontWeight: FontWeight.bold),
            ),
            const SizedBox(height: 8),
            _DetailRow('ptr', '0x${node.ptr.toRadixString(16)}'),
            _DetailRow('size', '${node.size}'),
            _DetailRow('age', '$age'),
            _DetailRow('state', node.state.name),
            _DetailRow('owner', ownerId == null ? 'none' : '$ownerId'),
            _DetailRow('edges', '${node.edges.length}'),
            const SizedBox(height: 16),
            Text(
              'size over time',
              style: Theme.of(context).textTheme.labelMedium,
            ),
            const SizedBox(height: 8),
            SizedBox(
              height: 120,
              child: _Sparkline(
                samples: _buffer.samples,
                color: colorForState(node.state),
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _NoSelectionPlaceholder extends StatelessWidget {
  const _NoSelectionPlaceholder();

  @override
  Widget build(BuildContext context) {
    return const Center(child: Text('no node selected'));
  }
}

class _DetailRow extends StatelessWidget {
  const _DetailRow(this.label, this.value);

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Row(
        children: [
          SizedBox(
            width: 48,
            child: Text(label, style: Theme.of(context).textTheme.bodySmall),
          ),
          Expanded(
            child: Text(value, style: Theme.of(context).textTheme.bodyMedium),
          ),
        ],
      ),
    );
  }
}

class _Sparkline extends StatelessWidget {
  const _Sparkline({required this.samples, required this.color});

  final List<SparklineSample> samples;
  final Color color;

  @override
  Widget build(BuildContext context) {
    if (samples.length < 2) {
      return const Center(child: Text('collecting data...'));
    }
    final spots = <FlSpot>[
      for (var i = 0; i < samples.length; i++)
        FlSpot(i.toDouble(), samples[i].size.toDouble()),
    ];
    return LineChart(
      LineChartData(
        titlesData: const FlTitlesData(show: false),
        gridData: const FlGridData(show: false),
        borderData: FlBorderData(show: false),
        lineTouchData: const LineTouchData(enabled: false),
        lineBarsData: [
          LineChartBarData(
            spots: spots,
            isCurved: false,
            color: color,
            barWidth: 2,
            dotData: const FlDotData(show: false),
          ),
        ],
      ),
    );
  }
}
