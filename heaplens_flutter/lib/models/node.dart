import 'package:flutter/foundation.dart';

/// Node lifecycle state. Mirrors `heaplens_protocol::diff::NodeState`, which
/// serializes as lowercase: "healthy" | "orphan" | "hot" | "freed".
enum NodeStateDto {
  healthy,
  orphan,
  hot,
  freed;

  /// Maps a wire string to a [NodeStateDto]. Falls back to [healthy] (with
  /// a debug-mode warning) for any string not in the known set, so that
  /// forward-compatible additions to the wire enum don't crash the client.
  static NodeStateDto fromWire(String s) {
    switch (s) {
      case 'healthy':
        return NodeStateDto.healthy;
      case 'orphan':
        return NodeStateDto.orphan;
      case 'hot':
        return NodeStateDto.hot;
      case 'freed':
        return NodeStateDto.freed;
      default:
        debugPrint('NodeStateDto.fromWire: unknown state "$s", defaulting to healthy');
        return NodeStateDto.healthy;
    }
  }
}

/// Single node in the ownership graph.
///
/// Field names mirror `heaplens_protocol::diff::NodeDto` verbatim — this is
/// the wire contract. `id`/`ptr`/`size`/`ts` are `u64` on the wire and
/// serialize as bare JSON numbers; this is safe on the Dart VM (64-bit
/// `int`) but NOT safe under dart2js/Flutter web (double, max 2^53). See
/// the note in diff.rs.
@immutable
class NodeDto {
  final int id;
  final int ptr;
  final int size;
  final int ts;
  final String symbol;
  final bool live;
  final NodeStateDto state;
  final List<int> edges;

  const NodeDto({
    required this.id,
    required this.ptr,
    required this.size,
    required this.ts,
    required this.symbol,
    required this.live,
    required this.state,
    required this.edges,
  });

  factory NodeDto.fromJson(Map<String, dynamic> json) {
    return NodeDto(
      id: json['id'] as int,
      ptr: json['ptr'] as int,
      size: json['size'] as int,
      ts: json['ts'] as int,
      symbol: json['symbol'] as String,
      live: json['live'] as bool,
      state: NodeStateDto.fromWire(json['state'] as String),
      edges: (json['edges'] as List<dynamic>).cast<int>(),
    );
  }
}
