import 'dart:async';
import 'dart:math';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/filter_providers.dart';
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

  testWidgets(
      'a removed node keeps painting (fading ghost) after its NodeDto is '
      'gone from the graph provider map', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(4));
    final node = _node(id: 77, state: NodeStateDto.orphan);
    layout.addNode(node, {77: node});

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
    expect(tester.takeException(), isNull);

    // Mirror main.dart's orchestrator: the same message-processing step
    // removes the NodeDto from graph_provider's map AND starts the
    // ForceLayout fade for the same id.
    container.read(graphProvider.notifier).applyDiff(
          const GraphDiff(ts: 2, add: [], update: [], remove: [77]),
        );
    layout.removeNode(77);
    await tester.pump();

    // NodeDto is gone...
    expect(
      container.read(graphProvider.notifier).nodes.containsKey(77),
      isFalse,
    );
    // ...but the SimNode is still mid-fade, with a captured last-known
    // state, and the widget must keep painting it without throwing.
    expect(layout.simNodes.containsKey(77), isTrue);
    expect(layout.simNodes[77]!.lastKnownState, equals(NodeStateDto.orphan));
    expect(tester.takeException(), isNull);

    // Advance the fade partway and pump again: still mid-fade, still must
    // render without crashing.
    layout.step(0.5);
    await tester.pump();
    expect(layout.simNodes.containsKey(77), isTrue);
    expect(layout.simNodes[77]!.fade, lessThan(1.0));
    expect(layout.simNodes[77]!.fade, greaterThan(0.0));
    expect(tester.takeException(), isNull);
  });

  testWidgets(
      'min-size filter hides a node from graph-view painting and hit-testing',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(6));
    final small = _node(id: 1, size: 8);
    final big = _node(id: 2, size: 4096);
    layout.addNode(small, {1: small, 2: big});
    layout.addNode(big, {1: small, 2: big});
    // Pin positions so we know exactly where to tap.
    layout.simNodes[1]!.position.setValues(100, 100);
    layout.simNodes[1]!.radius = 20;
    layout.simNodes[2]!.position.setValues(300, 300);
    layout.simNodes[2]!.radius = 20;

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
        .applyDiff(GraphSnapshot(ts: 1, nodes: [small, big]));
    await tester.pump();

    // Before filtering: tapping the small node selects it.
    await tester.tapAt(const Offset(100, 100));
    await tester.pump();
    expect(container.read(selectedNodeIdProvider), 1);

    container.read(selectedNodeIdProvider.notifier).state = null;
    container.read(minSizeFilterProvider.notifier).state = 100;
    await tester.pump();
    expect(tester.takeException(), isNull);

    // After filtering: the small node is hidden from hit-testing...
    await tester.tapAt(const Offset(100, 100));
    await tester.pump();
    expect(container.read(selectedNodeIdProvider), isNull);

    // ...but the big node remains tappable...
    await tester.tapAt(const Offset(300, 300));
    await tester.pump();
    expect(container.read(selectedNodeIdProvider), 2);

    // ...and the underlying node map is untouched by the filter.
    expect(container.read(graphProvider.notifier).nodes.length, 2);
  });

  testWidgets(
      'regression: painter actually draws circles when the provider is '
      'populated, and draws nothing when it is empty — the test class '
      'missing before fix/canvas-render (77 tests passed with a blank '
      'canvas, none of them asserted anything was actually painted)',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    final layout = ForceLayout(centerX: 200, centerY: 200, random: Random(7));
    final node = _node(id: 1, state: NodeStateDto.healthy);
    layout.addNode(node, {1: node});
    layout.simNodes[1]!.position.setValues(150, 150);
    layout.simNodes[1]!.radius = 20;

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

    // Empty provider: no *nodes* to paint. The painter now always draws a
    // faint background HUD grid regardless of data (see graph_canvas.dart
    // `_paintGrid` — a persistent scale reference, not conditional on
    // there being anything to show), so "paints nothing at all" is no
    // longer the right assertion; tightened to "paints no circles" per
    // this test's own original guidance above.
    await tester.pump();
    expect(find.byKey(const Key('graphCanvasPaint')), isNot(paints..circle()));

    // Populate: the painter must now actually issue a drawCircle call for
    // the live node. This is the assertion class that would have caught a
    // dead render path (revision/simNodes populated correctly upstream,
    // but the painter never invoked, or invoked with stale/empty data).
    container
        .read(graphProvider.notifier)
        .applyDiff(GraphSnapshot(ts: 1, nodes: [node]));
    await tester.pump();
    expect(find.byKey(const Key('graphCanvasPaint')), paints..circle());
  });
}
