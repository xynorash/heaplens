import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/node.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import '../theme/xynorash_theme.dart';
import 'node_colors.dart';
import 'ui_common.dart';

/// Bounded ring-buffer capacity for the size-over-time sparkline.
const int kSparklineCapacity = 120;

/// One (ts, size) sample recorded for the sparkline.
typedef SparklineSample = ({int ts, int size});

/// Bounded ring buffer of [SparklineSample]s for a single node, keyed by
/// that node's id. Exposed as its own class (rather than inlined private
/// state) so its reset/bound-growth behavior can be unit tested directly,
/// without driving a widget tree.
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

/// The "Node detail" right-rail panel: full field detail for the currently
/// selected node ([selectedNodeIdProvider]) — symbol, ptr (hex), size,
/// age, state (as a plain-language color chip, paired with the raw state
/// name via its tooltip), owner (id + ptr), edge count.
///
/// Renders an explicit [EmptyState] when nothing is selected, or when the
/// selected id no longer exists in the node map (e.g. it was removed by a
/// `remove` diff) — never crashes on a stale selection.
///
/// The size-over-time sparkline is a separate right-rail panel
/// ([NodeSparklinePanel]) per the UI refresh's panel grouping — this
/// widget only renders the raw/technical field list.
class NodeDetail extends ConsumerStatefulWidget {
  const NodeDetail({super.key});

  @override
  ConsumerState<NodeDetail> createState() => _NodeDetailState();
}

class _NodeDetailState extends ConsumerState<NodeDetail> {
  /// Client-side rolling max of `ts` across all nodes ever seen, analogous
  /// to the daemon's own `max_ts_seen` (see graph.rs:
  /// `self.max_ts_seen = self.max_ts_seen.max(ev.ts_nanos)`). Monotonic --
  /// updated only via `.max()` against the current live-node scan, so it
  /// never decreases even when the node carrying the current max is later
  /// removed by a `remove` diff. Persisted on the state (not recomputed
  /// from scratch each build) precisely so that churn like alloc-then-free
  /// can't make a selected node's displayed `age` visibly snap backwards.
  int _maxTsSeen = 0;

  @override
  Widget build(BuildContext context) {
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final selectedId = ref.watch(selectedNodeIdProvider);

    if (selectedId == null || !nodes.containsKey(selectedId)) {
      // Selection cleared, or the selected node no longer exists in state
      // (e.g. a `remove` diff deleted it) -- show a placeholder rather than
      // risk a null lookup.
      return const EmptyState(message: 'no node selected', icon: Icons.touch_app_outlined);
    }

    final node = nodes[selectedId]!;

    int scanMaxTs = node.ts;
    NodeDto? owner;
    for (final n in nodes.values) {
      if (n.ts > scanMaxTs) scanMaxTs = n.ts;
      if (n.edges.contains(selectedId)) owner = n;
    }
    _maxTsSeen = _maxTsSeen > scanMaxTs ? _maxTsSeen : scanMaxTs;
    final age = _maxTsSeen - node.ts;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        Row(
          children: [
            Expanded(
              // The symbol is a real code identifier (a resolved call-site
              // name), not prose — monospace here too, same as the field
              // values below, at a size that reads as a header.
              child: Text(
                node.symbol,
                style: XynorashTheme.mono(fontSize: 15, fontWeight: FontWeight.bold),
                overflow: TextOverflow.ellipsis,
              ),
            ),
            NodeStateChip(state: node.state),
          ],
        ),
        const SizedBox(height: 8),
        _DetailRow('ptr', '0x${node.ptr.toRadixString(16)}'),
        _DetailRow('size', '${node.size}', tooltip: 'Allocation size in bytes.'),
        _DetailRow('age', '$age', tooltip: 'How long this allocation has been live, in producer-clock ticks.'),
        _DetailRow('state', node.state.name, tooltip: kNodeStateDescriptions[node.state]!),
        _DetailRow(
          'owner',
          owner == null ? 'none' : '${owner.id}',
          trailing: owner == null ? null : '(0x${owner.ptr.toRadixString(16)})',
          tooltip: 'The allocation that owns this one, if any.',
        ),
        _DetailRow('edges', '${node.edges.length}', tooltip: 'Number of allocations this node owns.'),
      ],
    );
  }
}

/// The "Size over time" right-rail panel: a sparkline of the selected
/// node's size history. Separate widget (and separate [SparklineBuffer]
/// instance) from [NodeDetail] so the two can live in independent
/// collapsible right-rail sections — both key off the same
/// [selectedNodeIdProvider]/[graphProvider] state, so they stay in sync
/// without any shared mutable state between them.
class NodeSparklinePanel extends ConsumerStatefulWidget {
  const NodeSparklinePanel({super.key});

  @override
  ConsumerState<NodeSparklinePanel> createState() => _NodeSparklinePanelState();
}

class _NodeSparklinePanelState extends ConsumerState<NodeSparklinePanel> {
  final SparklineBuffer _buffer = SparklineBuffer();

  @override
  Widget build(BuildContext context) {
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final selectedId = ref.watch(selectedNodeIdProvider);

    if (selectedId == null || !nodes.containsKey(selectedId)) {
      _buffer.reset();
      return const EmptyState(message: 'no node selected', icon: Icons.show_chart);
    }

    final node = nodes[selectedId]!;
    _buffer.record(node);

    return SizedBox(
      height: 120,
      child: _Sparkline(samples: _buffer.samples, color: colorForState(node.state)),
    );
  }
}

class _DetailRow extends StatelessWidget {
  const _DetailRow(this.label, this.value, {this.trailing, this.tooltip});

  final String label;
  final String value;
  final String? trailing;
  final String? tooltip;

  @override
  Widget build(BuildContext context) {
    final row = Padding(
      padding: const EdgeInsets.symmetric(vertical: 2),
      child: Row(
        children: [
          SizedBox(
            width: 48,
            child: Text(label, style: const TextStyle(color: Colors.white54, fontSize: 11)),
          ),
          // Every field here — ptr, size, age, edge count — is raw data,
          // not prose; monospace throughout the value column keeps them
          // reading as data and keeps hex/decimal columns visually
          // aligned run to run.
          Expanded(child: Text(value, style: XynorashTheme.mono(fontSize: 12.5))),
          if (trailing != null)
            Text(trailing!, style: XynorashTheme.mono(fontSize: 11, color: Colors.white54)),
        ],
      ),
    );
    if (tooltip == null) return row;
    return Tooltip(message: tooltip!, child: row);
  }
}

class _Sparkline extends StatelessWidget {
  const _Sparkline({required this.samples, required this.color});

  final List<SparklineSample> samples;
  final Color color;

  @override
  Widget build(BuildContext context) {
    if (samples.isEmpty) {
      return const Center(child: Text('collecting data...'));
    }
    // A node whose size genuinely never changes only ever produces one
    // deduplicated sample (SparklineBuffer.record dedupes consecutive
    // same-size updates) — that used to fall into the same "collecting
    // data..." branch as zero samples and never resolve, since a
    // single-point line has nothing to interpolate between. That's not
    // "still collecting", it's a real, final answer: this allocation's
    // size has been constant since it appeared. Render it as a flat line
    // at that one value instead of leaving the panel looking broken.
    final spots = samples.length == 1
        ? [
            FlSpot(0, samples[0].size.toDouble()),
            FlSpot(1, samples[0].size.toDouble()),
          ]
        : [
            for (var i = 0; i < samples.length; i++)
              FlSpot(i.toDouble(), samples[i].size.toDouble()),
          ];
    return LineChart(
      LineChartData(
        titlesData: const FlTitlesData(
          show: true,
          leftTitles: AxisTitles(
            sideTitles: SideTitles(showTitles: true, reservedSize: 40),
          ),
          bottomTitles: AxisTitles(sideTitles: SideTitles(showTitles: false)),
          topTitles: AxisTitles(sideTitles: SideTitles(showTitles: false)),
          rightTitles: AxisTitles(sideTitles: SideTitles(showTitles: false)),
        ),
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
