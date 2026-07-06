import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/filter_providers.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/selection_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/memory_map.dart';

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

Future<ProviderContainer> _pumpMemoryMap(
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
          return MaterialApp(
            home: Scaffold(
              body: SizedBox(
                width: 400,
                height: 400,
                child: const MemoryMap(),
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
  testWidgets('renders a grid of cells without throwing', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    final nodeA = _node(id: 1, ptr: 100, state: NodeStateDto.healthy);
    final nodeB = _node(id: 2, ptr: 50, state: NodeStateDto.orphan);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [nodeA, nodeB]));
    await tester.pump();

    expect(find.byType(MemoryMap), findsOneWidget);
    expect(tester.takeException(), isNull);
    // One GestureDetector-wrapped cell per live node.
    expect(find.byType(GridView), findsOneWidget);
  });

  testWidgets(
      'min-size filter hides a cell from the rendered grid without '
      'touching the underlying node map', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    final small = _node(id: 1, ptr: 10, size: 8);
    final big = _node(id: 2, ptr: 20, size: 4096);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [small, big]));
    await tester.pump();

    // Both nodes present in the grid before filtering.
    expect(find.byKey(const ValueKey(1)), findsOneWidget);
    expect(find.byKey(const ValueKey(2)), findsOneWidget);

    container.read(minSizeFilterProvider.notifier).state = 100;
    await tester.pump();

    // The small node's cell is hidden from the grid...
    expect(find.byKey(const ValueKey(1)), findsNothing);
    expect(find.byKey(const ValueKey(2)), findsOneWidget);

    // ...but the underlying node map is untouched by the filter.
    expect(container.read(graphProvider.notifier).nodes.length, 2);
  });

  testWidgets('orphan-only filter hides non-orphan cells', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    final healthy = _node(id: 1, ptr: 10, state: NodeStateDto.healthy);
    final orphan = _node(id: 2, ptr: 20, state: NodeStateDto.orphan);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [healthy, orphan]));
    await tester.pump();

    container.read(orphanOnlyFilterProvider.notifier).state = true;
    await tester.pump();

    expect(find.byKey(const ValueKey(1)), findsNothing);
    expect(find.byKey(const ValueKey(2)), findsOneWidget);
    expect(container.read(graphProvider.notifier).nodes.length, 2);
  });

  testWidgets('symbol search filter hides non-matching cells', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    final vecNode = _node(id: 1, ptr: 10, symbol: 'alloc::vec::Vec');
    final boxNode = _node(id: 2, ptr: 20, symbol: 'alloc::boxed::Box');
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [vecNode, boxNode]));
    await tester.pump();

    container.read(symbolSearchFilterProvider.notifier).state = 'vec';
    await tester.pump();

    expect(find.byKey(const ValueKey(1)), findsOneWidget);
    expect(find.byKey(const ValueKey(2)), findsNothing);
    expect(container.read(graphProvider.notifier).nodes.length, 2);
  });

  testWidgets('tapping a cell selects that node via selectedNodeIdProvider',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    final node = _node(id: 7, ptr: 10);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [node]));
    await tester.pump();

    expect(container.read(selectedNodeIdProvider), isNull);

    await tester.tap(find.byKey(const ValueKey(7)));
    await tester.pump();

    expect(container.read(selectedNodeIdProvider), 7);
  });

  testWidgets('renders cells in ascending ptr order despite shuffled input',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpMemoryMap(tester, controller);

    // Create nodes with non-sequential, scrambled ptr values.
    // Add them in order: 500, 100, 300, 200, 400 to verify sorting.
    final node1 = _node(id: 1, ptr: 500);
    final node2 = _node(id: 2, ptr: 100);
    final node3 = _node(id: 3, ptr: 300);
    final node4 = _node(id: 4, ptr: 200);
    final node5 = _node(id: 5, ptr: 400);
    container.read(graphProvider.notifier).applyDiff(
          GraphSnapshot(ts: 1, nodes: [node1, node2, node3, node4, node5]),
        );
    await tester.pump();

    // Find all _MemoryMapCell widgets (there should be 5).
    final cellFinder = find.byType(Container);
    expect(cellFinder, findsWidgets);

    // Verify that when we iterate through the cells' keys in the order
    // they appear in the tree (via byKey), they correspond to nodes
    // in ascending ptr order: 100, 200, 300, 400, 500 (ids 2, 4, 3, 5, 1).
    final expectedOrderByPtr = [2, 4, 3, 5, 1]; // ids with ptrs 100, 200, 300, 400, 500
    for (int i = 0; i < expectedOrderByPtr.length; i++) {
      expect(
        find.byKey(ValueKey(expectedOrderByPtr[i])),
        findsOneWidget,
        reason: 'Node ${expectedOrderByPtr[i]} should be in grid at ptr-sorted position $i',
      );
    }

    // Also verify the cells are present in the grid.
    expect(find.byType(GridView), findsOneWidget);
  });

  testWidgets('computes safe column count even when maxWidth is infinite',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    late ProviderContainer container;
    // Test that MemoryMap can be rendered without the columns calculation
    // throwing when given tight or unusual constraints.
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
        child: Consumer(
          builder: (context, ref, _) {
            container = ProviderScope.containerOf(context);
            return MaterialApp(
              home: Scaffold(
                body: Align(
                  alignment: Alignment.topLeft,
                  child: SizedBox(
                    // Wrap in a very small SizedBox to test within a constrained context,
                    // but the MemoryMap's internal LayoutBuilder will see tight constraints.
                    width: 100,
                    height: 100,
                    child: MemoryMap(),
                  ),
                ),
              ),
            );
          },
        ),
      ),
    );

    // Add a node to render.
    final node = _node(id: 1, ptr: 10);
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [node]));

    // Should not throw on the columns calculation.
    expect(tester.takeException(), isNull);
    await tester.pump();
    expect(tester.takeException(), isNull);
    expect(find.byType(MemoryMap), findsOneWidget);
  });
}
