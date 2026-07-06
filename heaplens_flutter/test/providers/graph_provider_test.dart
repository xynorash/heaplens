import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';

NodeDto _node({
  required int id,
  int ptr = 0,
  int size = 8,
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
  group('GraphNotifier.applyDiff', () {
    late ProviderContainer container;
    late StreamController<GraphMessage> controller;

    setUp(() {
      controller = StreamController<GraphMessage>();
      container = ProviderContainer(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
      );
      addTearDown(container.dispose);
      addTearDown(() => controller.close());
      // Activate the notifier (and its ref.listen to graphMessageProvider).
      container.listen(graphProvider, (_, _) {});
    });

    test('initial revision is 0 and node map is empty', () {
      expect(container.read(graphProvider), 0);
      expect(container.read(graphProvider.notifier).nodes, isEmpty);
    });

    test('snapshot replaces prior state entirely', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(GraphSnapshot(ts: 1, nodes: [_node(id: 1), _node(id: 2)]));
      expect(container.read(graphProvider), 1);
      expect(notifier.nodes.keys.toSet(), {1, 2});

      // A second, disjoint snapshot fully replaces the map — id 1/2 gone.
      notifier.applyDiff(GraphSnapshot(ts: 2, nodes: [_node(id: 3)]));
      expect(container.read(graphProvider), 2);
      expect(notifier.nodes.keys.toSet(), {3});
    });

    test('diff add inserts new nodes', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(
        GraphDiff(ts: 1, add: [_node(id: 1), _node(id: 2)], update: [], remove: []),
      );
      expect(container.read(graphProvider), 1);
      expect(notifier.nodes.keys.toSet(), {1, 2});
    });

    test('diff update by id changes fields without duplicating', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(
        GraphDiff(ts: 1, add: [_node(id: 1, symbol: 'first')], update: [], remove: []),
      );
      notifier.applyDiff(
        GraphDiff(
          ts: 2,
          add: [],
          update: [_node(id: 1, symbol: 'second', state: NodeStateDto.orphan)],
          remove: [],
        ),
      );

      expect(notifier.nodes, hasLength(1));
      final updated = notifier.nodes[1]!;
      expect(updated.symbol, 'second');
      expect(updated.state, NodeStateDto.orphan);
    });

    test('diff remove deletes by id', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(
        GraphDiff(ts: 1, add: [_node(id: 1), _node(id: 2)], update: [], remove: []),
      );
      notifier.applyDiff(GraphDiff(ts: 2, add: [], update: [], remove: [1]));

      expect(notifier.nodes.keys.toSet(), {2});
    });

    test('remove of an id not present is a no-op, not an error', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(
        GraphDiff(ts: 1, add: [_node(id: 1)], update: [], remove: []),
      );
      expect(
        () => notifier.applyDiff(
          GraphDiff(ts: 2, add: [], update: [], remove: [888]),
        ),
        returnsNormally,
      );
      // Existing node untouched, no crash, still one node.
      expect(notifier.nodes.keys.toSet(), {1});
      expect(container.read(graphProvider), 2);
    });

    test('update for an id not previously seen upserts it (per the '
        '"add/update are both upsert by id" contract), not a no-op', () {
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(
        GraphDiff(ts: 1, add: [], update: [_node(id: 999)], remove: []),
      );
      expect(notifier.nodes.keys, contains(999));
      expect(container.read(graphProvider), 1);
    });

    test('revision increments exactly once per applyDiff call regardless of '
        'payload size', () {
      final notifier = container.read(graphProvider.notifier);
      final manyNodes = List.generate(50, (i) => _node(id: i));
      notifier.applyDiff(GraphDiff(ts: 1, add: manyNodes, update: [], remove: []));
      expect(container.read(graphProvider), 1);

      notifier.applyDiff(GraphDiff(ts: 2, add: [], update: [], remove: []));
      expect(container.read(graphProvider), 2);
    });

    test('unknown state string on a raw JSON node falls back to healthy '
        'via NodeDto.fromJson/fromWire', () {
      final json = {
        'id': 1,
        'ptr': 0,
        'size': 8,
        'ts': 0,
        'symbol': 'sym',
        'live': true,
        'state': 'some_future_state',
        'edges': <int>[],
      };
      final node = NodeDto.fromJson(json);
      final notifier = container.read(graphProvider.notifier);
      notifier.applyDiff(GraphSnapshot(ts: 1, nodes: [node]));
      expect(notifier.nodes[1]!.state, NodeStateDto.healthy);
    });

    group('derived getters', () {
      test('orphanCount, liveNodeCount, totalLiveBytes computed on demand', () {
        final notifier = container.read(graphProvider.notifier);
        notifier.applyDiff(
          GraphSnapshot(
            ts: 1,
            nodes: [
              _node(id: 1, live: true, size: 10, state: NodeStateDto.healthy),
              _node(id: 2, live: true, size: 20, state: NodeStateDto.orphan),
              _node(id: 3, live: false, size: 30, state: NodeStateDto.orphan),
            ],
          ),
        );

        expect(notifier.orphanCount, 2);
        expect(notifier.liveNodeCount, 2);
        expect(notifier.totalLiveBytes, 30); // only live nodes: 10 + 20
      });
    });
  });

  group('GraphNotifier automatic wiring to graphMessageProvider', () {
    test('applies messages that arrive on the live stream without manual '
        'feeding, bumping revision once per message', () async {
      final controller = StreamController<GraphMessage>();
      final container = ProviderContainer(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
      );
      addTearDown(container.dispose);
      addTearDown(() => controller.close());

      final revisions = <int>[];
      container.listen(graphProvider, (_, next) => revisions.add(next));
      // Ensure the notifier (and its ref.listen) is built before pushing.
      container.read(graphProvider);

      controller.add(GraphSnapshot(ts: 1, nodes: [_node(id: 1), _node(id: 2)]));
      await Future<void>.delayed(Duration.zero);

      expect(revisions, [1]);
      expect(container.read(graphProvider.notifier).nodes.keys.toSet(), {1, 2});

      controller.add(GraphDiff(ts: 2, add: [], update: [], remove: [1]));
      await Future<void>.delayed(Duration.zero);

      expect(revisions, [1, 2]);
      expect(container.read(graphProvider.notifier).nodes.keys.toSet(), {2});
    });
  });
}
