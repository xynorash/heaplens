import '../models/node.dart';

enum InsightSeverity { critical, warning, info }

/// One deterministic, rule-based observation derived from the graph's
/// current node map — no AI, no daemon round-trip, purely computed from
/// data the client already holds ([NodeDto.state]/`edges`/`size`, all
/// already server-computed where relevant — this never re-derives φ or
/// the orphan/hot classification itself, only reads it).
class Insight {
  const Insight({
    required this.id,
    required this.severity,
    required this.title,
    required this.detail,
    required this.implicatedNodeId,
  });

  /// Stable key (not a display value) so the selected-insight provider can
  /// survive a rebuild that recomputes the same insight from fresh data.
  final String id;
  final InsightSeverity severity;

  /// Short list-item title (severity dot + this, in the left column).
  final String title;

  /// Full explanation + concrete suggestion, shown in the right column.
  final String detail;

  /// The node this insight is "about" — selecting the insight selects
  /// this node in the graph. `null` only if every implicated node
  /// vanished between computation and click (race, not expected in
  /// practice since insights are recomputed every rebuild).
  final int? implicatedNodeId;
}

/// Minimum fraction of total live bytes one node must hold to be flagged
/// as a dominant consumer. Empirically chosen (structural heuristic, like
/// the daemon's own hot-cluster/storm thresholds), not derived.
const double kDominantConsumerFraction = 0.20;

/// Computes every active insight from the current live node map. Pure and
/// synchronous — safe to call on every build; the caller decides how
/// often that is (typically once per graph-provider revision change).
List<Insight> computeInsights(Map<int, NodeDto> nodes) {
  final insights = <Insight>[];
  final live = nodes.values.where((n) => n.live).toList();

  // --- Orphan leak: high-confidence, worded plainly (not hedged) ---
  // Grouped by symbol — a leak at one call site is one actionable item,
  // not N separate list rows for N allocations from the same place.
  final orphansBySymbol = <String, List<NodeDto>>{};
  for (final n in live) {
    if (n.state == NodeStateDto.orphan) {
      orphansBySymbol.putIfAbsent(n.symbol, () => []).add(n);
    }
  }
  for (final entry in orphansBySymbol.entries) {
    final symbol = entry.key;
    final group = entry.value;
    final totalBytes = group.fold<int>(0, (sum, n) => sum + n.size);
    // Representative node for "select in graph" — the largest one, since
    // that's the most consequential single allocation to look at first.
    final representative = group.reduce((a, b) => a.size >= b.size ? a : b);
    insights.add(
      Insight(
        id: 'orphan:$symbol',
        severity: InsightSeverity.critical,
        title: 'Probable leak at $symbol',
        detail: 'Probable leak: ${group.length} allocation${group.length == 1 ? '' : 's'} '
            'totaling $totalBytes bytes lost their owner and were never freed. '
            'Site: $symbol.',
        implicatedNodeId: representative.id,
      ),
    );
  }

  // --- Unbounded growth: hot clusters. Worded as a hedged suggestion —
  // structural signal, not a certainty like an orphan. ---
  for (final n in live) {
    if (n.state == NodeStateDto.hot) {
      insights.add(
        Insight(
          id: 'hot:${n.id}',
          severity: InsightSeverity.warning,
          title: 'Growing cluster at ${n.symbol}',
          detail: 'Cluster at ${n.symbol} has grown to ${n.edges.length} children and keeps '
              'growing — likely an unbounded collection; check whether entries are ever removed.',
          implicatedNodeId: n.id,
        ),
      );
    }
  }

  // --- Dominant consumer: one node holding a large share of live bytes.
  // Informational — worded as an observation, not a problem claim. ---
  final totalLiveBytes = live.fold<int>(0, (sum, n) => sum + n.size);
  if (totalLiveBytes > 0) {
    for (final n in live) {
      final fraction = n.size / totalLiveBytes;
      if (fraction >= kDominantConsumerFraction) {
        final pct = (fraction * 100).toStringAsFixed(0);
        insights.add(
          Insight(
            id: 'dominant:${n.id}',
            severity: InsightSeverity.info,
            title: 'Dominant consumer at ${n.symbol}',
            detail: 'One allocation at ${n.symbol} holds $pct% of live heap — '
                'the primary consumer to optimize if memory use needs to come down.',
            implicatedNodeId: n.id,
          ),
        );
      }
    }
  }

  return insights;
}
