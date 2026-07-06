import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/graph_diff.dart';
import '../models/node.dart';
import 'ws_provider.dart';

/// Owns the live ownership-graph node map and republishes a monotonically
/// increasing [revision] counter as its Riverpod state.
///
/// INTENTIONAL DESIGN (locked decision, see M5 task-3 brief Q3): this
/// `Notifier`'s state is the `revision` int, *not* the node map itself. The
/// node map ([_nodes]) is a private `Map<int, NodeDto>` that is mutated in
/// place on every [applyDiff] call. This deliberately breaks the usual
/// Riverpod convention of treating state as immutable data — it is a
/// performance escape valve, because the daemon can push graph diffs at up
/// to ~30/sec and rebuilding/copying a large map that often is wasteful.
/// Widgets should `ref.watch(graphProvider)` (the revision int) purely to
/// know *when* to rebuild/repaint, and separately call
/// `ref.read(graphProvider.notifier).nodes` (or the derived getters below)
/// to read the current data. Do NOT "fix" this back into an immutable map
/// — that would defeat the purpose.
class GraphNotifier extends Notifier<int> {
  final Map<int, NodeDto> _nodes = <int, NodeDto>{};

  @override
  int build() {
    // Automatically wire up to the live WS message stream: every message
    // that actually arrives is applied here. Widgets never need to feed
    // messages into this notifier manually.
    ref.listen<AsyncValue<GraphMessage>>(graphMessageProvider, (previous, next) {
      next.whenData(applyDiff);
    });
    return 0;
  }

  /// Read-only view of the current node map, keyed by node id. Exposed for
  /// consumers (e.g. the graph canvas) that `ref.watch(graphProvider)` for
  /// the revision and then pull the current map via
  /// `ref.read(graphProvider.notifier).nodes`.
  Map<int, NodeDto> get nodes => _nodes;

  /// Applies one incoming [GraphMessage] to the node map and bumps
  /// [revision] by exactly 1 — regardless of how many nodes the message
  /// touches (a diff with 50 adds is still one message, one revision bump).
  void applyDiff(GraphMessage msg) {
    switch (msg) {
      case GraphSnapshot snapshot:
        _nodes
          ..clear()
          ..addEntries(snapshot.nodes.map((n) => MapEntry(n.id, n)));
      case GraphDiff diff:
        for (final n in diff.add) {
          _nodes[n.id] = n;
        }
        for (final n in diff.update) {
          _nodes[n.id] = n;
        }
        for (final id in diff.remove) {
          _nodes.remove(id);
        }
    }
    state = state + 1;
  }

  /// Number of nodes currently in [NodeStateDto.orphan] state.
  int get orphanCount =>
      _nodes.values.where((n) => n.state == NodeStateDto.orphan).length;

  /// Number of nodes currently marked `live`.
  int get liveNodeCount => _nodes.values.where((n) => n.live).length;

  /// Sum of `size` across all `live` nodes.
  int get totalLiveBytes => _nodes.values
      .where((n) => n.live)
      .fold(0, (sum, n) => sum + n.size);
}

/// Public entry point: `ref.watch(graphProvider)` for the revision int,
/// `ref.read(graphProvider.notifier)` for the node map / derived getters.
final graphProvider = NotifierProvider<GraphNotifier, int>(GraphNotifier.new);
