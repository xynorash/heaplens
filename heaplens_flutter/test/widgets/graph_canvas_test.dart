import 'dart:async';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/selection_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/simulation/force_layout.dart';
import 'package:heaplens_flutter/widgets/graph_canvas.dart';

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
  testWidgets('paints a small static graph without throwing', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(1));
    final nodeA = _node(id: 1, state: NodeStateDto.healthy, edges: [2]);
    final nodeB = _node(id: 2, state: NodeStateDto.orphan);
    final currentNodes = {1: nodeA, 2: nodeB};
    layout.addNode(nodeA, currentNodes);
    layout.addNode(nodeB, currentNodes);
    layout.step(0.016);

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
        child: MaterialApp(
          home: Scaffold(
            body: SizedBox(
              width: 400,
              height: 400,
              child: GraphCanvas(layout: layout),
            ),
          ),
        ),
      ),
    );

    // Populate the graph provider's node map so paint has NodeDto data to
    // cross-reference against the layout's SimNodes.
    final container = ProviderScope.containerOf(
      tester.element(find.byType(GraphCanvas)),
    );
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [nodeA, nodeB]));
    await tester.pump();

    // A couple more pumps to exercise the pulsing-ring AnimatedBuilder path.
    await tester.pump(const Duration(milliseconds: 100));
    await tester.pump(const Duration(milliseconds: 100));

    expect(find.byType(GraphCanvas), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets('tap at a known node position selects that node', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(2));
    final node = _node(id: 42, state: NodeStateDto.healthy);
    layout.addNode(node, {42: node});
    // Pin the node's position/radius so we know exactly where to tap.
    layout.simNodes[42]!.position.setValues(150, 150);
    layout.simNodes[42]!.radius = 20;

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
                  child: GraphCanvas(layout: layout),
                ),
              ),
            );
          },
        ),
      ),
    );

    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [node]));
    await tester.pump();

    expect(container.read(selectedNodeIdProvider), isNull);

    // The GraphCanvas is laid out starting at the SizedBox's top-left. Since
    // there's no extra offset above it in this tree, node-local (150, 150)
    // maps directly to that same global point.
    await tester.tapAt(const Offset(150, 150));
    await tester.pump();

    expect(container.read(selectedNodeIdProvider), 42);
  });

  testWidgets(
      'tap at a SimNode with no NodeDto backing does not select it',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(3));
    // Create a node and add it to the layout so it exists in simNodes.
    final ghostNode = _node(id: 99, state: NodeStateDto.healthy);
    layout.addNode(ghostNode, {99: ghostNode});
    // Pin the node's position/radius so we know exactly where to tap.
    layout.simNodes[99]!.position.setValues(250, 250);
    layout.simNodes[99]!.radius = 20;

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
                  child: GraphCanvas(layout: layout),
                ),
              ),
            );
          },
        ),
      ),
    );

    // Intentionally do NOT add ghostNode to the node map via applyDiff.
    // This simulates the transient state where a SimNode exists but its
    // NodeDto has been removed (e.g., mid-fade-out after removal).
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: []));
    await tester.pump();

    expect(container.read(selectedNodeIdProvider), isNull);

    // Try to tap on the ghost node. The tap handler should find it in
    // simNodes but must NOT select it because it has no NodeDto backing.
    await tester.tapAt(const Offset(250, 250));
    await tester.pump();

    // selectedNodeIdProvider should remain null/unchanged.
    expect(container.read(selectedNodeIdProvider), isNull);
  });
}
