import 'package:flutter/foundation.dart';

import 'node.dart';

/// Discriminated union for the two daemon->Flutter message shapes, mirroring
/// `heaplens_protocol::diff::GraphMessage` (see diff.rs). Wire messages are
/// tagged with an inline `"type"` field: "snapshot" or "diff".
///
/// Implemented as a Dart 3 `sealed` class with two subclasses rather than
/// `freezed` (forbidden for this project) — callers get exhaustive-switch
/// checking via `switch (message) { GraphSnapshot s => ..., GraphDiff d => ... }`.
@immutable
sealed class GraphMessage {
  final int ts;

  const GraphMessage({required this.ts});

  /// Parses a decoded JSON map, dispatching on `json['type']`.
  factory GraphMessage.fromJson(Map<String, dynamic> json) {
    final type = json['type'] as String;
    switch (type) {
      case 'snapshot':
        return GraphSnapshot.fromJson(json);
      case 'diff':
        return GraphDiff.fromJson(json);
      default:
        throw FormatException('GraphMessage.fromJson: unknown type "$type"');
    }
  }
}

/// Full-state message: `{ "type": "snapshot", "ts": ..., "nodes": [...] }`.
final class GraphSnapshot extends GraphMessage {
  final List<NodeDto> nodes;

  const GraphSnapshot({required super.ts, required this.nodes});

  factory GraphSnapshot.fromJson(Map<String, dynamic> json) {
    return GraphSnapshot(
      ts: json['ts'] as int,
      nodes: (json['nodes'] as List<dynamic>)
          .map((n) => NodeDto.fromJson(n as Map<String, dynamic>))
          .toList(),
    );
  }
}

/// Incremental update: `{ "type": "diff", "ts": ..., "add": [...], "update": [...], "remove": [...] }`.
/// `add`/`update` are full `NodeDto`s; `remove` is a list of node ids.
final class GraphDiff extends GraphMessage {
  final List<NodeDto> add;
  final List<NodeDto> update;
  final List<int> remove;

  const GraphDiff({
    required super.ts,
    required this.add,
    required this.update,
    required this.remove,
  });

  factory GraphDiff.fromJson(Map<String, dynamic> json) {
    return GraphDiff(
      ts: json['ts'] as int,
      add: (json['add'] as List<dynamic>)
          .map((n) => NodeDto.fromJson(n as Map<String, dynamic>))
          .toList(),
      update: (json['update'] as List<dynamic>)
          .map((n) => NodeDto.fromJson(n as Map<String, dynamic>))
          .toList(),
      remove: (json['remove'] as List<dynamic>).cast<int>(),
    );
  }
}
