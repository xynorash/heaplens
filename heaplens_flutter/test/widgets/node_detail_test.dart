import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/selection_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/node_detail.dart';

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

Future<ProviderContainer> _pumpNodeDetail(
  WidgetTester tester,
  StreamController<GraphMessage> controller,
) async {
  late ProviderContainer container;
  await tester.pumpWidget(
    ProviderScope(
      overrides: [
        graphMessageProvider.overrideWith((ref) => controller.stream),
      ],
      child: Consumer(
        builder: (context, ref, _) {
          container = ProviderScope.containerOf(context);
          return const MaterialApp(
            home: Scaffold(
              body: SizedBox(
                width: 400,
                height: 400,
                child: NodeDetail(),
              ),
            ),
          );
        },
      ),
    ),
  );
  return container;
}

void main() {
  testWidgets('renders placeholder when nothing selected', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    await _pumpNodeDetail(tester, controller);

    expect(find.text('> no node selected'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets(
      'renders placeholder when selected id no longer exists in node map',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpNodeDetail(tester, controller);

    final node = _node(id: 1, ptr: 10);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [node]));
    container.read(selectedNodeIdProvider.notifier).state = 1;
    await tester.pump();

    expect(find.text('sym'), findsOneWidget);

    // Remove the node entirely via a diff.
    container.read(graphProvider.notifier).applyDiff(
          GraphDiff(ts: 2, add: const [], update: const [], remove: const [1]),
        );
    await tester.pump();

    expect(find.text('> no node selected'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets(
      'renders symbol/ptr(hex)/size/state/owner/edge-count for a selected node',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpNodeDetail(tester, controller);

    // node 1 owns node 2 (edges contains 2); node 2 is selected.
    final owner = _node(id: 1, ptr: 0x10, symbol: 'owner_sym', edges: [2]);
    final selected = _node(
      id: 2,
      ptr: 0xABCD,
      size: 512,
      symbol: 'target_sym',
      state: NodeStateDto.orphan,
      edges: [3, 4],
    );
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [owner, selected]));
    container.read(selectedNodeIdProvider.notifier).state = 2;
    await tester.pump();

    expect(find.text('target_sym'), findsOneWidget);
    expect(find.text('0xabcd'), findsOneWidget);
    expect(find.text('512'), findsOneWidget);
    expect(find.text('orphan'), findsOneWidget);
    expect(find.text('1'), findsOneWidget); // owner id
    expect(find.text('2'), findsOneWidget); // edge count
  });

  testWidgets(
      'age computes against rolling max ts seen, not wall-clock',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpNodeDetail(tester, controller);

    // Use ts values wildly far from real wall-clock epoch millis, to prove
    // age is derived purely from observed ts values.
    const farFutureTs = 999999999999;
    final selected = _node(id: 1, ts: farFutureTs - 500, symbol: 'aged');
    final other = _node(id: 2, ts: farFutureTs, symbol: 'newer');
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [selected, other]));
    container.read(selectedNodeIdProvider.notifier).state = 1;
    await tester.pump();

    // maxTsSeen (farFutureTs, from `other`) - selected.ts (farFutureTs - 500) = 500.
    expect(find.text('500'), findsOneWidget);
  });

  testWidgets(
      'age does not decrease when the node carrying the max ts is removed',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpNodeDetail(tester, controller);

    // Selected node has ts=100. A second node arrives with ts=300 (the
    // current max), making age = 300 - 100 = 200.
    final selected = _node(id: 1, ts: 100, symbol: 'selected');
    final other = _node(id: 2, ts: 300, symbol: 'other');
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [selected, other]));
    container.read(selectedNodeIdProvider.notifier).state = 1;
    await tester.pump();

    expect(find.text('200'), findsOneWidget);

    // Remove `other` -- the node that carried the observed max ts. A naive
    // "recompute max from currently-live nodes" implementation would now
    // see only `selected` (ts=100) and report age=0, i.e. age would
    // visibly decrease. The rolling max must be persisted and monotonic,
    // so age must stay at 200.
    container.read(graphProvider.notifier).applyDiff(
          GraphDiff(ts: 2, add: const [], update: const [], remove: const [2]),
        );
    await tester.pump();

    expect(find.text('selected'), findsOneWidget);
    expect(find.text('200'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets(
      'switching selection to a different node does not crash and shows '
      'the new node\'s data', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpNodeDetail(tester, controller);

    final nodeA = _node(id: 1, ts: 1, size: 100, symbol: 'a');
    final nodeB = _node(id: 2, ts: 1, size: 200, symbol: 'b');
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [nodeA, nodeB]));
    container.read(selectedNodeIdProvider.notifier).state = 1;
    await tester.pump();

    // Grow node A's size a few times while selected.
    for (final size in [110, 120, 130]) {
      container.read(graphProvider.notifier).applyDiff(
            GraphDiff(
              ts: size,
              add: const [],
              update: [_node(id: 1, ts: size, size: size, symbol: 'a')],
              remove: const [],
            ),
          );
      await tester.pump();
    }

    container.read(selectedNodeIdProvider.notifier).state = 2;
    await tester.pump();

    expect(find.text('b'), findsOneWidget);
    expect(tester.takeException(), isNull);

    // Switch back to node A: must not crash or retain node B's samples.
    container.read(selectedNodeIdProvider.notifier).state = 1;
    await tester.pump();

    expect(find.text('a'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });
}

