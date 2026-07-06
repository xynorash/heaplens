// Uses `package:flutter_test`'s bare `test()`/`expect()` API (this project's
// standard test runner) but never touches `WidgetTester`, `pumpWidget`, or
// any Flutter binding: `ForceLayout` itself (lib/simulation/force_layout.dart)
// has zero Flutter/widget/networking imports, so every test below exercises
// plain Dart objects and would run identically under `package:test`'s VM
// runner with no Flutter bindings initialized.
import 'dart:math';

import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/simulation/force_layout.dart';

NodeDto _node({
  required int id,
  int ptr = 0,
  int size = 64,
  int ts = 0,
  String symbol = 'sym',
  bool live = true,
  NodeStateDto state = NodeStateDto.healthy,
  List<int> edges = const [],
}) {
  return NodeDto(
    id: id,
    ptr: ptr,
    size: size,
    ts: ts,
    symbol: symbol,
    live: live,
    state: state,
    edges: edges,
  );
}

void main() {
  group('addNode spawn positioning', () {
    test('spawns within jitter radius of owner position', () {
      final layout = ForceLayout(
        centerX: 400,
        centerY: 300,
        random: Random(42),
      );

      final owner = _node(id: 1, edges: [2]);
      final currentNodes = {1: owner, 2: _node(id: 2)};

      layout.addNode(owner, currentNodes);
      // Move the owner's SimNode away from center so we can tell the child
      // spawned near the owner, not near center.
      layout.simNodes[1]!.position.setValues(1000, 1000);

      layout.addNode(currentNodes[2]!, currentNodes);

      final child = layout.simNodes[2]!;
      final ownerPos = layout.simNodes[1]!.position;
      final dist = (child.position - ownerPos).length;

      // kSpawnJitter is applied independently on x and y, so max possible
      // distance from owner is sqrt(2) * kSpawnJitter.
      expect(dist, lessThanOrEqualTo(kSpawnJitter * 1.5));
    });

    test('spawns near canvas center when no owner exists', () {
      final layout = ForceLayout(
        centerX: 400,
        centerY: 300,
        random: Random(7),
      );

      final orphan = _node(id: 5, edges: const []);
      layout.addNode(orphan, {5: orphan});

      final sim = layout.simNodes[5]!;
      final distFromCenter = ((sim.position.x - 400).abs());
      final distFromCenterY = ((sim.position.y - 300).abs());

      expect(distFromCenter, lessThanOrEqualTo(kSpawnJitter));
      expect(distFromCenterY, lessThanOrEqualTo(kSpawnJitter));
    });
  });

  group('updateNode', () {
    test('a size change never moves position or velocity', () {
      final layout = ForceLayout(random: Random(1));
      final node = _node(id: 1, size: 64);
      layout.addNode(node, {1: node});

      final sim = layout.simNodes[1]!;
      sim.position.setValues(123.0, 456.0);
      sim.velocity.setValues(1.5, -2.5);
      final radiusBefore = sim.radius;

      final updated = _node(id: 1, size: 4096);
      layout.updateNode(updated);

      final after = layout.simNodes[1]!;
      expect(after.position.x, equals(123.0));
      expect(after.position.y, equals(456.0));
      expect(after.velocity.x, equals(1.5));
      expect(after.velocity.y, equals(-2.5));
      expect(after.radius, isNot(equals(radiusBefore)));
      expect(after.radius, equals(radiusForSize(4096)));
    });
  });

  group('removeNode fade-out', () {
    test('fades then is deleted after ~1s of simulated steps', () {
      final layout = ForceLayout(random: Random(3));
      final node = _node(id: 9);
      layout.addNode(node, {9: node});

      layout.removeNode(9);

      // Still present immediately after removeNode (fade in progress).
      expect(layout.simNodes.containsKey(9), isTrue);
      expect(layout.simNodes[9]!.fade, equals(1.0));

      // Step in small increments; node should still exist but fade should
      // be decreasing partway through.
      for (var i = 0; i < 5; i++) {
        layout.step(0.1);
      }
      expect(layout.simNodes.containsKey(9), isTrue);
      expect(layout.simNodes[9]!.fade, lessThan(1.0));
      expect(layout.simNodes[9]!.fade, greaterThan(0.0));

      // Push past the fade duration (kFadeDurationSeconds == 1.0s total;
      // 0.5s elapsed above, so 10 more 0.1s steps clears it).
      for (var i = 0; i < 10; i++) {
        layout.step(0.1);
      }
      expect(layout.simNodes.containsKey(9), isFalse);
    });
  });

  group('aggregation (ENF9)', () {
    test('does not activate at exactly the threshold', () {
      final layout = ForceLayout(random: Random(5));
      for (var i = 0; i < kAggregationThreshold; i++) {
        final n = _node(id: i, symbol: 'sym_${i % 10}');
        layout.addNode(n, {i: n});
      }
      layout.step(0.016);

      expect(layout.liveNodeCount, equals(kAggregationThreshold));
      expect(layout.isAggregated, isFalse);
      expect(layout.simNodes.length, equals(kAggregationThreshold));
    });

    test('activates strictly above the threshold, collapsing to one '
        'SimNode per symbol', () {
      final layout = ForceLayout(random: Random(5));
      const symbolCount = 10;
      for (var i = 0; i < kAggregationThreshold + 1; i++) {
        final n = _node(id: i, symbol: 'sym_${i % symbolCount}');
        layout.addNode(n, {i: n});
      }
      layout.step(0.016);

      expect(layout.liveNodeCount, equals(kAggregationThreshold + 1));
      expect(layout.isAggregated, isTrue);
      expect(layout.simNodes.length, equals(symbolCount));
    });

    test('falls back to individual mode when count drops back at/below '
        'threshold', () {
      final layout = ForceLayout(random: Random(5));
      for (var i = 0; i < kAggregationThreshold + 1; i++) {
        final n = _node(id: i, symbol: 'sym_${i % 10}');
        layout.addNode(n, {i: n});
      }
      layout.step(0.016);
      expect(layout.isAggregated, isTrue);

      layout.removeNode(kAggregationThreshold); // back down to threshold
      layout.step(0.016);

      expect(layout.liveNodeCount, equals(kAggregationThreshold));
      expect(layout.isAggregated, isFalse);
    });
  });

  group('step physics sanity', () {
    test('connected nodes drift toward rest length over time', () {
      final layout = ForceLayout(random: Random(11));
      final a = _node(id: 1, edges: [2]);
      final b = _node(id: 2);
      layout.addNode(a, {1: a, 2: b});
      layout.addNode(b, {1: a, 2: b});

      // Force them far apart initially.
      layout.simNodes[1]!.position.setValues(0, 0);
      layout.simNodes[2]!.position.setValues(500, 0);

      for (var i = 0; i < 200; i++) {
        layout.step(0.016);
      }

      final dist =
          (layout.simNodes[2]!.position - layout.simNodes[1]!.position)
              .length;
      // Should have contracted substantially from 500 toward ~restLength.
      expect(dist, lessThan(500));
    });
  });
}
