import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../insights/insight_rules.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import '../theme/xynorash_theme.dart';
import 'ui_common.dart';

/// Severity -> color. Deliberately its own mapping (not reused from
/// `node_colors.dart`'s state-color language) — an insight's severity is
/// a different axis than a node's Healthy/Orphan/Hot state, even though
/// critical/warning happen to share the same [XynorashTheme] coral/orange
/// hues as orphan/hot for the same "how urgent" intuition.
const Map<InsightSeverity, Color> kInsightSeverityColors = {
  InsightSeverity.critical: XynorashTheme.coral,
  InsightSeverity.warning: XynorashTheme.orange,
  InsightSeverity.info: XynorashTheme.teal,
};

/// Insights & Suggestions panel: deterministic, rule-based observations
/// computed from the graph's current node map (see insight_rules.dart) —
/// not AI, not a daemon round-trip. Two columns: a selectable list on the
/// left, full detail + suggestion on the right. Selecting an insight also
/// selects its implicated node in the graph (via [selectedNodeIdProvider]),
/// so this panel and the graph/node-detail panel stay cross-referenced.
class InsightsPanel extends ConsumerStatefulWidget {
  const InsightsPanel({super.key});

  @override
  ConsumerState<InsightsPanel> createState() => _InsightsPanelState();
}

class _InsightsPanelState extends ConsumerState<InsightsPanel> {
  String? _selectedInsightId;

  @override
  Widget build(BuildContext context) {
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final insights = computeInsights(nodes);

    Insight? selected;
    for (final i in insights) {
      if (i.id == _selectedInsightId) {
        selected = i;
        break;
      }
    }
    // The previously-selected insight resolved (e.g. the leak was fixed,
    // or the hot cluster cooled down) — fall back to the first remaining
    // insight rather than showing a stale detail pane for something that
    // no longer applies.
    selected ??= insights.isEmpty ? null : insights.first;

    return HudFrame(
      child: Container(
      key: const Key('insightsPanel'),
      color: XynorashTheme.bgPanel,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(12, 10, 12, 6),
            child: Text(
              XynorashTheme.bracketLabel('Insights & Suggestions'),
              style: XynorashTheme.sectionHeader(fontSize: 12),
            ),
          ),
          Expanded(
            child: insights.isEmpty
                ? const EmptyState(
                    message: 'No active insights',
                    icon: Icons.check_circle_outline,
                  )
                : Row(
                    crossAxisAlignment: CrossAxisAlignment.stretch,
                    children: [
                      Expanded(
                        flex: 2,
                        child: _InsightList(
                          insights: insights,
                          selectedId: selected?.id,
                          onSelect: (insight) {
                            setState(() => _selectedInsightId = insight.id);
                            if (insight.implicatedNodeId != null) {
                              ref.read(selectedNodeIdProvider.notifier).state =
                                  insight.implicatedNodeId;
                            }
                          },
                        ),
                      ),
                      const VerticalDivider(width: 1, color: Colors.white12),
                      Expanded(
                        flex: 3,
                        child: _InsightDetail(insight: selected),
                      ),
                    ],
                  ),
          ),
        ],
      ),
      ),
    );
  }
}

class _InsightList extends StatelessWidget {
  const _InsightList({required this.insights, required this.selectedId, required this.onSelect});

  final List<Insight> insights;
  final String? selectedId;
  final void Function(Insight) onSelect;

  @override
  Widget build(BuildContext context) {
    return ListView.builder(
      key: const Key('insightsList'),
      itemCount: insights.length,
      itemBuilder: (context, i) {
        final insight = insights[i];
        final isSelected = insight.id == selectedId;
        return InkWell(
          key: Key('insightItem_${insight.id}'),
          onTap: () => onSelect(insight),
          child: Container(
            color: isSelected ? Colors.white.withValues(alpha: 0.08) : null,
            padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
            child: Row(
              children: [
                Container(
                  width: 8,
                  height: 8,
                  decoration: BoxDecoration(
                    color: kInsightSeverityColors[insight.severity],
                    shape: BoxShape.circle,
                  ),
                ),
                const SizedBox(width: 8),
                Expanded(
                  child: Text(
                    insight.title,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(color: Colors.white, fontSize: 12),
                  ),
                ),
              ],
            ),
          ),
        );
      },
    );
  }
}

class _InsightDetail extends StatelessWidget {
  const _InsightDetail({required this.insight});

  final Insight? insight;

  @override
  Widget build(BuildContext context) {
    final insight = this.insight;
    if (insight == null) {
      return const EmptyState(message: 'No insight selected', icon: Icons.info_outline);
    }
    return SingleChildScrollView(
      padding: const EdgeInsets.all(12),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisSize: MainAxisSize.min,
        children: [
          Row(
            children: [
              Container(
                width: 10,
                height: 10,
                decoration: BoxDecoration(
                  color: kInsightSeverityColors[insight.severity],
                  shape: BoxShape.circle,
                ),
              ),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  insight.title,
                  style: const TextStyle(color: Colors.white, fontWeight: FontWeight.bold),
                ),
              ),
            ],
          ),
          const SizedBox(height: 8),
          Text(insight.detail, style: const TextStyle(color: Colors.white70, fontSize: 13)),
        ],
      ),
    );
  }
}
