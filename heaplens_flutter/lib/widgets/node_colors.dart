import 'package:flutter/material.dart';

import '../models/node.dart';
import '../theme/xynorash_theme.dart';

/// Fill colors by [NodeStateDto], shared by every widget that renders nodes
/// (the graph canvas and the memory map). This is the single source of truth
/// for the healthy/orphan/hot/freed -> color mapping; do not duplicate this
/// map elsewhere.
///
/// Presentation effects specific to one widget (e.g. the graph canvas's
/// orphan pulsing ring, or freed-node fade alpha) are NOT part of this
/// shared mapping -- only the base fill color is shared.
///
/// Colors are [XynorashTheme]'s exact hues, not just "a teal"/"a coral" —
/// the previous generic Material teal/coral/amber were already close to
/// these, so this only sharpens the shade to match the rest of this
/// setup; the healthy=teal / orphan=coral / hot=amber *semantics* this
/// session's "consistent everywhere" requirement is about are unchanged.
const Map<NodeStateDto, Color> kNodeStateColors = {
  NodeStateDto.healthy: XynorashTheme.teal,
  NodeStateDto.orphan: XynorashTheme.coral,
  NodeStateDto.hot: XynorashTheme.orange,
  NodeStateDto.freed: Color(0xFF9E9E9E), // gray — no clear theme mapping, unchanged
};

/// Returns the shared base fill color for a given node [state].
Color colorForState(NodeStateDto state) => kNodeStateColors[state]!;

/// Plain-language label for each [NodeStateDto], used everywhere state is
/// surfaced as text (the node-detail state chip, tooltips) so the same word
/// always pairs with the same color — see [kNodeStateColors]'s "consistent
/// everywhere" requirement.
const Map<NodeStateDto, String> kNodeStateLabels = {
  NodeStateDto.healthy: 'Healthy',
  NodeStateDto.orphan: 'Orphan',
  NodeStateDto.hot: 'Hot',
  NodeStateDto.freed: 'Freed',
};

/// One-sentence plain definition for each [NodeStateDto], surfaced as a
/// tooltip anywhere the state (or a metric derived from it, like the top
/// bar's Orphans counter) is shown.
const Map<NodeStateDto, String> kNodeStateDescriptions = {
  NodeStateDto.healthy: 'A live allocation with a known owner.',
  NodeStateDto.orphan: 'A live allocation whose owner was freed — likely a leak.',
  NodeStateDto.hot: 'An owner whose children are growing quickly.',
  NodeStateDto.freed: 'No longer live; fading out of the graph.',
};

/// Small colored pill showing a node's state as plain-language text — the
/// state chip used in the node-detail panel. Same color mapping as every
/// other place state is drawn ([kNodeStateColors]); pairs the color with
/// the word so state reads at a glance without relying on color alone.
class NodeStateChip extends StatelessWidget {
  const NodeStateChip({super.key, required this.state});

  final NodeStateDto state;

  @override
  Widget build(BuildContext context) {
    final color = colorForState(state);
    return Tooltip(
      message: kNodeStateDescriptions[state]!,
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 8, vertical: 2),
        decoration: BoxDecoration(
          color: color.withValues(alpha: 0.18),
          border: Border.all(color: color),
          borderRadius: BorderRadius.circular(12),
        ),
        child: Text(
          kNodeStateLabels[state]!,
          style: TextStyle(
            color: color,
            fontSize: 12,
            fontWeight: FontWeight.w600,
          ),
        ),
      ),
    );
  }
}
