import 'package:flutter/material.dart';

import '../models/node.dart';

/// Fill colors by [NodeStateDto], shared by every widget that renders nodes
/// (the graph canvas and the memory map). This is the single source of truth
/// for the healthy/orphan/hot/freed -> color mapping; do not duplicate this
/// map elsewhere.
///
/// Presentation effects specific to one widget (e.g. the graph canvas's
/// orphan pulsing ring, or freed-node fade alpha) are NOT part of this
/// shared mapping -- only the base fill color is shared.
const Map<NodeStateDto, Color> kNodeStateColors = {
  NodeStateDto.healthy: Color(0xFF2DD4BF), // teal
  NodeStateDto.orphan: Color(0xFFFF7F50), // coral
  NodeStateDto.hot: Color(0xFFFFC107), // amber
  NodeStateDto.freed: Color(0xFF9E9E9E), // gray
};

/// Returns the shared base fill color for a given node [state].
Color colorForState(NodeStateDto state) => kNodeStateColors[state]!;
