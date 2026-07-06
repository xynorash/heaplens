import 'package:flutter_riverpod/flutter_riverpod.dart';

/// Currently-selected node id (or `null` if nothing is selected), set by a
/// tap on the graph canvas. Read by `node_detail.dart` (a later task) to
/// show details for the selected node.
final selectedNodeIdProvider = StateProvider<int?>((ref) => null);
