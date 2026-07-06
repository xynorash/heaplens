// App-level integration smoke tests for `main.dart`'s wiring: a fake
// `graphMessageProvider` feeds a small static graph through the widget tree
// and we assert the app builds/renders without throwing across
// snapshot -> diff(update) -> diff(remove), and that the view-mode toggle
// swaps which center widget is shown.

import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/main.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/force_layout_provider.dart';
import 'package:heaplens_flutter/providers/view_mode_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/graph_canvas.dart';
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

void main() {
  testWidgets(
      'app builds, renders a static graph through the full '
      'snapshot -> diff(update) -> diff(remove) lifecycle, and drives the '
      'shared ForceLayout accordingly', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    late ProviderContainer container;

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
        child: Consumer(
          builder: (context, ref, _) {
            container = ProviderScope.containerOf(context);
            return const HeapLensApp();
          },
        ),
      ),
    );
    await tester.pump();

    // Snapshot: two nodes appear.
    controller.add(
      GraphSnapshot(
        ts: 1,
        nodes: [
          _node(id: 1, edges: [2]),
          _node(id: 2, state: NodeStateDto.orphan),
        ],
      ),
    );
    await tester.pump();
    await tester.pump();

    final layout = container.read(forceLayoutProvider);
    expect(layout.simNodes.keys.toSet(), {1, 2});
    expect(tester.takeException(), isNull);

    // Diff: update node 1's size — same id, no new SimNode, radius changes.
    controller.add(
      GraphDiff(
        ts: 2,
        add: [],
        update: [_node(id: 1, size: 4096, edges: [2])],
        remove: [],
      ),
    );
    await tester.pump();
    await tester.pump();

    expect(layout.simNodes.keys.toSet(), {1, 2});
    expect(tester.takeException(), isNull);

    // Diff: remove node 2 — starts a fade, still present immediately after.
    controller.add(GraphDiff(ts: 3, add: [], update: [], remove: [2]));
    await tester.pump();
    await tester.pump();

    expect(layout.simNodes.containsKey(2), isTrue);
    expect(tester.takeException(), isNull);

    // Advance enough physics ticks (30Hz, driven by the orchestrator's own
    // Timer.periodic — advance real+fake time together) for the ~1s fade to
    // complete and the SimNode to be deleted.
    await tester.pump(const Duration(seconds: 2));

    expect(layout.simNodes.containsKey(2), isFalse);
    expect(layout.simNodes.containsKey(1), isTrue);
    expect(tester.takeException(), isNull);

    // Unmount so `_GraphOrchestratorState.dispose()` cancels its physics
    // `Timer.periodic` — otherwise flutter_test's fake-async zone flags it
    // as a leaked pending timer at test teardown.
    await tester.pumpWidget(const SizedBox());
  });

  testWidgets('view-mode toggle swaps the center widget between GraphCanvas '
      'and MemoryMap', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());

    late ProviderContainer container;

    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
        child: Consumer(
          builder: (context, ref, _) {
            container = ProviderScope.containerOf(context);
            return const HeapLensApp();
          },
        ),
      ),
    );
    await tester.pump();

    // Defaults to graph view.
    expect(find.byType(GraphCanvas), findsOneWidget);
    expect(find.byType(MemoryMap), findsNothing);

    container.read(viewModeProvider.notifier).state = ViewMode.memoryMap;
    await tester.pump();

    expect(find.byType(GraphCanvas), findsNothing);
    expect(find.byType(MemoryMap), findsOneWidget);

    container.read(viewModeProvider.notifier).state = ViewMode.graph;
    await tester.pump();

    expect(find.byType(GraphCanvas), findsOneWidget);
    expect(find.byType(MemoryMap), findsNothing);

    // Unmount so the physics `Timer.periodic` gets cancelled (see note in
    // the previous test).
    await tester.pumpWidget(const SizedBox());
  });
}
