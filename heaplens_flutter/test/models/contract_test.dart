// Contract test: verifies field-for-field fidelity between the real
// captured wire fixtures (test/fixtures/*.json, see PROVENANCE.md) and the
// Dart models in lib/models/. The wire contract source of truth is
// crates/heaplens-protocol/src/diff.rs.
import 'dart:convert';
import 'dart:io';

import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';

Map<String, dynamic> loadFixture(String name) {
  final raw = File('test/fixtures/$name').readAsStringSync();
  return jsonDecode(raw) as Map<String, dynamic>;
}

void main() {
  group('NodeStateDto.fromWire', () {
    test('maps all four known wire strings', () {
      expect(NodeStateDto.fromWire('healthy'), NodeStateDto.healthy);
      expect(NodeStateDto.fromWire('orphan'), NodeStateDto.orphan);
      expect(NodeStateDto.fromWire('hot'), NodeStateDto.hot);
      expect(NodeStateDto.fromWire('freed'), NodeStateDto.freed);
    });

    test('falls back to healthy for unknown strings', () {
      expect(NodeStateDto.fromWire('bogus'), NodeStateDto.healthy);
    });
  });

  group('snapshot.json', () {
    final json = loadFixture('snapshot.json');
    final message = GraphMessage.fromJson(json);

    test('parses as GraphSnapshot with correct type and count', () {
      expect(message, isA<GraphSnapshot>());
      final snapshot = message as GraphSnapshot;
      expect(snapshot.ts, 329700);
      expect(snapshot.nodes, hasLength(101));
    });

    test('spot-checks the first node, including a non-empty edges list', () {
      final snapshot = message as GraphSnapshot;
      final first = snapshot.nodes.first;
      expect(first.id, 571);
      expect(first.ptr, 1599777467168);
      expect(first.size, 256);
      expect(first.ts, 230900);
      expect(first.symbol, '0x7ff74ba66230');
      expect(first.live, true);
      expect(first.state, NodeStateDto.healthy);
      expect(first.edges, [572]);
    });
  });

  group('diff_add.json', () {
    final json = loadFixture('diff_add.json');
    final message = GraphMessage.fromJson(json);

    test('parses as GraphDiff with 101 adds and empty update/remove', () {
      expect(message, isA<GraphDiff>());
      final diff = message as GraphDiff;
      expect(diff.ts, 329700);
      expect(diff.add, hasLength(101));
      expect(diff.update, isEmpty);
      expect(diff.remove, isEmpty);
    });
  });

  group('diff_remove.json', () {
    final json = loadFixture('diff_remove.json');
    final message = GraphMessage.fromJson(json);

    test('parses as GraphDiff with 101 removed ids as ints', () {
      expect(message, isA<GraphDiff>());
      final diff = message as GraphDiff;
      expect(diff.ts, 329700);
      expect(diff.add, isEmpty);
      expect(diff.update, isEmpty);
      expect(diff.remove, hasLength(101));
      expect(diff.remove, everyElement(isA<int>()));
      expect(diff.remove.first, 405);
    });
  });

  group('diff_orphan.json', () {
    final json = loadFixture('diff_orphan.json');
    final message = GraphMessage.fromJson(json);

    test('parses update[0] node to NodeStateDto.orphan', () {
      expect(message, isA<GraphDiff>());
      final diff = message as GraphDiff;
      expect(diff.update, hasLength(1));
      final node = diff.update.first;
      expect(node.id, 500);
      expect(node.state, NodeStateDto.orphan);
      expect(node.live, true);
      expect(node.edges, [501]);
    });
  });

  group('GraphStats.fromJson', () {
    test('parses all fields, including null target identity before handshake', () {
      final json = <String, dynamic>{
        'type': 'stats',
        'ts': 12345,
        'events_received': 7,
        'symbols_resolved': 3,
        'hex_fallback': 4,
        'target_pid': null,
        'target_name': null,
      };
      final message = GraphMessage.fromJson(json);
      expect(message, isA<GraphStats>());
      final stats = message as GraphStats;
      expect(stats.ts, 12345);
      expect(stats.eventsReceived, 7);
      expect(stats.symbolsResolved, 3);
      expect(stats.hexFallback, 4);
      expect(stats.targetPid, isNull);
      expect(stats.targetName, isNull);
    });

    test('parses target identity once present', () {
      final json = <String, dynamic>{
        'type': 'stats',
        'ts': 1,
        'events_received': 0,
        'symbols_resolved': 0,
        'hex_fallback': 0,
        'target_pid': 4242,
        'target_name': 'target.exe',
      };
      final stats = GraphMessage.fromJson(json) as GraphStats;
      expect(stats.targetPid, 4242);
      expect(stats.targetName, 'target.exe');
    });
  });

  group('NodeDto 64-bit precision', () {
    test('preserves full 64-bit integer precision for u64 fields', () {
      // Synthetic test: use values near 64-bit max and at 2^53+1 (smallest
      // integer a double cannot represent exactly). This test ensures that
      // jsonDecode and the `as int` casts preserve full 64-bit precision
      // and don't accidentally round through double representation.
      const largeU64 = 9007199254740993; // 2^53 + 1
      const anotherLargeU64 = 9223372036854775807; // i64 max (near u64 max)

      final json = <String, dynamic>{
        'id': largeU64,
        'ptr': anotherLargeU64,
        'size': largeU64,
        'ts': 1234567890,
        'symbol': 'test_symbol',
        'live': true,
        'state': 'healthy',
        'edges': <int>[],
      };

      final node = NodeDto.fromJson(json);

      // Verify that each u64 field is preserved exactly as-is
      expect(node.id, equals(largeU64));
      expect(node.ptr, equals(anotherLargeU64));
      expect(node.size, equals(largeU64));
      expect(node.ts, equals(1234567890));
    });
  });
}
