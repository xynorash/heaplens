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
import 'package:vector_math/vector_math.dart' show Vector2;

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
  group('radiusForSize', () {
    test('clamps to kMinNodeRadius/kMaxNodeRadius at the extremes', () {
      expect(radiusForSize(0), kMinNodeRadius);
      expect(radiusForSize(1), greaterThanOrEqualTo(kMinNodeRadius));
      expect(radiusForSize(100000000), kMaxNodeRadius);
    });

    test('is monotonically non-decreasing across realistic allocation sizes', () {
      const sizes = [1, 8, 16, 32, 64, 128, 256, 500, 1024, 4096, 65536, 1048576];
      var previous = radiusForSize(0);
      for (final size in sizes) {
        final radius = radiusForSize(size);
        expect(
          radius,
          greaterThanOrEqualTo(previous),
          reason: 'radius must not shrink as size grows (size=$size)',
        );
        previous = radius;
      }
    });

    test(
      'spreads the realistic byte-to-kilobyte range across meaningfully '
      'different radii, not clustered at the floor',
      () {
        // This is the actual bug being fixed: every workload used
        // throughout this project's own testing allocates 1-500 bytes,
        // and the old sqrt-based curve put nearly all of them within a
        // couple pixels of kMinNodeRadius. A 32-byte node and a 500-byte
        // node should now read as visibly different sizes.
        final small = radiusForSize(32);
        final medium = radiusForSize(500);
        final large = radiusForSize(65536);

        expect(medium - small, greaterThan(4.0));
        expect(large - medium, greaterThan(4.0));
      },
    );
  });

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

  group('removeNode captures lastKnownState for fade rendering', () {
    test('captures the outgoing NodeDto.state onto SimNode.lastKnownState',
        () {
      final layout = ForceLayout(random: Random(3));
      final node = _node(id: 9, state: NodeStateDto.orphan);
      layout.addNode(node, {9: node});

      expect(layout.simNodes[9]!.lastKnownState, isNull);

      layout.removeNode(9);

      expect(layout.simNodes[9]!.lastKnownState, equals(NodeStateDto.orphan));
      // The captured state must survive across the fade, since the painter
      // needs it on every frame until the SimNode is finally deleted.
      layout.step(0.5);
      expect(layout.simNodes[9]!.lastKnownState, equals(NodeStateDto.orphan));
    });

    test('aggregate bucket emptying out also captures a lastKnownState', () {
      final layout = ForceLayout(random: Random(5));
      for (var i = 0; i < kAggregationThreshold + 1; i++) {
        final n = _node(id: i, symbol: 'sym_${i % 10}');
        layout.addNode(n, {i: n});
      }
      layout.step(0.016);
      expect(layout.isAggregated, isTrue);

      // Drain every member of symbol 'sym_0' (ids 0, 10, 20, ...).
      for (var i = 0; i < kAggregationThreshold + 1; i += 10) {
        layout.removeNode(i);
      }

      // Find the (now fading) aggregate SimNode: any negative-id key still
      // present with a non-null lastKnownState.
      final fadingAgg = layout.simNodes.entries
          .where((e) => e.key < 0 && e.value.lastKnownState != null)
          .toList();
      expect(fadingAgg, isNotEmpty);
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

    test(
      'does NOT fall back to individual mode merely dropping back to the '
      'entry threshold — hysteresis dead zone',
      () {
        final layout = ForceLayout(random: Random(5));
        for (var i = 0; i < kAggregationThreshold + 1; i++) {
          final n = _node(id: i, symbol: 'sym_${i % 10}');
          layout.addNode(n, {i: n});
        }
        layout.step(0.016);
        expect(layout.isAggregated, isTrue);

        layout.removeNode(kAggregationThreshold); // back down to threshold
        layout.step(0.016);

        // Still above kAggregationExitThreshold (450), so hysteresis keeps
        // it aggregated instead of thrashing back to individual mode.
        expect(layout.liveNodeCount, equals(kAggregationThreshold));
        expect(layout.isAggregated, isTrue);
      },
    );

    test(
      'hysteresis: stays aggregated in the dead zone, only falls back once '
      'below the exit threshold',
      () {
        final layout = ForceLayout(random: Random(5));
        for (var i = 0; i < kAggregationThreshold + 1; i++) {
          final n = _node(id: i, symbol: 'sym_${i % 10}');
          layout.addNode(n, {i: n});
        }
        layout.step(0.016);
        expect(layout.isAggregated, isTrue);

        // Drop down to 480 (between kAggregationExitThreshold=450 and
        // kAggregationThreshold=500): must NOT switch back to individual.
        for (var i = kAggregationThreshold; i >= 480; i--) {
          layout.removeNode(i);
        }
        layout.step(0.016);
        expect(layout.liveNodeCount, equals(480));
        expect(layout.isAggregated, isTrue);

        // Drop further, below kAggregationExitThreshold=450: now it must
        // fall back to individual mode.
        for (var i = 479; i >= 440; i--) {
          layout.removeNode(i);
        }
        layout.step(0.016);
        expect(layout.liveNodeCount, equals(440));
        expect(layout.isAggregated, isFalse);
      },
    );

    test(
      'a node mid-fade survives a mode-switch rebuild and still deletes '
      'itself once its fade completes',
      () {
        final layout = ForceLayout(random: Random(5));
        for (var i = 0; i < kAggregationThreshold; i++) {
          final n = _node(id: i, symbol: 'sym_${i % 10}');
          layout.addNode(n, {i: n});
        }
        layout.step(0.016);
        expect(layout.isAggregated, isFalse);

        // Start a fade on one individual-mode node, then immediately push
        // live count past the aggregation threshold in the same tick's
        // worth of events (fade begins, then boundary is crossed).
        layout.removeNode(0);
        expect(layout.simNodes.containsKey(0), isTrue);
        expect(layout.simNodes[0]!.fade, equals(1.0));

        // Live count was 500 (not > 500), then dropped to 499 by the
        // removeNode above; add two new nodes to push it to 501, strictly
        // above kAggregationThreshold, before the next step.
        final extraA = _node(id: 999998, symbol: 'sym_new');
        final extraB = _node(id: 999999, symbol: 'sym_new');
        layout.addNode(extraA, {999998: extraA});
        layout.addNode(extraB, {999999: extraB});
        layout.step(0.016); // crosses into aggregated mode -> rebuild

        expect(layout.isAggregated, isTrue);
        // The fading node must have survived the mode-switch rebuild rather
        // than being dropped instantly.
        expect(layout.simNodes.containsKey(0), isTrue);
        expect(layout.simNodes[0]!.fade, lessThanOrEqualTo(1.0));
        expect(layout.simNodes[0]!.fade, greaterThan(0.0));

        // Advance enough simulated time to finish the fade; it must be
        // deleted via the normal _advanceFades path, not left dangling.
        for (var i = 0; i < 20; i++) {
          layout.step(0.1);
        }
        expect(layout.simNodes.containsKey(0), isFalse);
      },
    );
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

  group('collision resolution (overlap fix)', () {
    test(
      'disconnected roots (no edges) spread out with no overlapping circles',
      () {
        // The reported "blob" case: many healthy roots, no springs holding
        // any of them together, all initially spawned near the same point.
        final layout = ForceLayout(centerX: 400, centerY: 300, random: Random(7));
        final nodes = <int, NodeDto>{
          for (var i = 0; i < 25; i++)
            i: _node(id: i, size: 200 + i * 300, symbol: 'root_$i'),
        };
        for (final node in nodes.values) {
          layout.addNode(node, nodes);
          // Force everyone to spawn on top of each other, worse than the
          // real jittered spawn — the harder version of the reported bug.
          layout.simNodes[node.id]!.position.setValues(400, 300);
        }

        for (var i = 0; i < 400; i++) {
          layout.step(0.016);
        }

        final ids = layout.simNodes.keys.toList();
        for (var i = 0; i < ids.length; i++) {
          for (var j = i + 1; j < ids.length; j++) {
            final a = layout.simNodes[ids[i]]!;
            final b = layout.simNodes[ids[j]]!;
            final dist = (a.position - b.position).length;
            final minDist = a.radius + b.radius;
            expect(
              dist,
              greaterThanOrEqualTo(minDist - 0.5), // small float tolerance
              reason:
                  'nodes ${ids[i]} and ${ids[j]} still overlap after settling',
            );
          }
        }

        // Not just non-overlapping — actually spread into a field, not
        // pinned in a tight pile at the spawn point.
        final maxDistFromCenter = ids
            .map((id) => (layout.simNodes[id]!.position - Vector2(400, 300)).length)
            .reduce(max);
        expect(maxDistFromCenter, greaterThan(60));
      },
    );

    test(
      'high-fan-out star stays a recognizable hub-and-spokes, no overlap, '
      'no central pile-up',
      () {
        // One owner with many children — springs pull children toward the
        // owner, which is exactly the shape collision resolution must not
        // blow apart, while still not letting the children overlap each
        // other or the owner at the center.
        final layout = ForceLayout(centerX: 400, centerY: 300, random: Random(3));
        final childIds = List.generate(40, (i) => i + 1);
        final owner = _node(id: 0, size: 500, edges: childIds);
        final nodes = <int, NodeDto>{
          0: owner,
          for (final id in childIds) id: _node(id: id, size: 300),
        };

        layout.addNode(owner, nodes);
        for (final id in childIds) {
          layout.addNode(nodes[id]!, nodes);
        }

        for (var i = 0; i < 400; i++) {
          layout.step(0.016);
        }

        final ownerPos = layout.simNodes[0]!.position;
        final ownerRadius = layout.simNodes[0]!.radius;

        // No overlap anywhere, including owner-vs-child.
        final ids = layout.simNodes.keys.toList();
        for (var i = 0; i < ids.length; i++) {
          for (var j = i + 1; j < ids.length; j++) {
            final a = layout.simNodes[ids[i]]!;
            final b = layout.simNodes[ids[j]]!;
            final dist = (a.position - b.position).length;
            final minDist = a.radius + b.radius;
            expect(
              dist,
              greaterThanOrEqualTo(minDist - 0.5),
              reason:
                  'nodes ${ids[i]} and ${ids[j]} still overlap after settling',
            );
          }
        }

        // Still a star, not blown apart: every child stays within a bounded
        // radius of the owner, not scattered across the whole canvas. The
        // bound is generous (not exactly kSpringRestLength) because 40
        // non-overlapping child circles physically cannot all pack into a
        // ring at the spring's exact rest length — collision resolution
        // legitimately pushes some further out to make room. What this
        // guards against is the real "blown apart" pathology: children
        // ending up scattered arbitrarily far away.
        for (final id in childIds) {
          final dist = (layout.simNodes[id]!.position - ownerPos).length;
          expect(
            dist,
            lessThan(kSpringRestLength * 5),
            reason: 'child $id drifted far from its owner — star was blown apart',
          );
          expect(
            dist,
            greaterThanOrEqualTo(ownerRadius),
            reason: 'child $id is inside the owner\'s own drawn circle',
          );
        }
      },
    );
  });
}
