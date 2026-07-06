import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/filter_providers.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/paused_provider.dart';
import 'package:heaplens_flutter/providers/view_mode_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/control_bar.dart';

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

Future<ProviderContainer> _pumpControlBar(
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
            home: Scaffold(body: ControlBar()),
          );
        },
      ),
    ),
  );
  return container;
}

void main() {
  testWidgets('connection status displays correctly for each state',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    container.read(connectionStatusProvider.notifier).state =
        ConnectionStatus.connecting;
    await tester.pump();
    expect(find.text('Connecting…'), findsOneWidget);

    container.read(connectionStatusProvider.notifier).state =
        ConnectionStatus.connected;
    await tester.pump();
    expect(find.text('Connected'), findsOneWidget);

    container.read(connectionStatusProvider.notifier).state =
        ConnectionStatus.disconnected;
    await tester.pump();
    expect(find.text('Disconnected'), findsOneWidget);
  });

  testWidgets('live counters reflect the graph provider derived getters',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    final healthy = _node(id: 1, size: 100);
    final orphan = _node(id: 2, size: 50, state: NodeStateDto.orphan);
    container.read(graphProvider.notifier).applyDiff(
          GraphSnapshot(ts: 1, nodes: [healthy, orphan]),
        );
    await tester.pump();

    expect(find.text('Nodes: '), findsOneWidget);
    expect(find.text('2'), findsOneWidget); // liveNodeCount
    expect(find.text('1'), findsOneWidget); // orphanCount
    expect(find.text('150'), findsOneWidget); // totalLiveBytes
  });

  testWidgets(
      'pause gates applyDiff: a message arriving while paused does not '
      'change graph state, one arriving after resume does', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    expect(container.read(pausedProvider), isFalse);

    // Pause via the button.
    await tester.tap(find.byKey(const Key('pauseResumeButton')));
    await tester.pump();
    expect(container.read(pausedProvider), isTrue);

    final revisionBefore = container.read(graphProvider);
    controller.add(GraphSnapshot(ts: 1, nodes: [_node(id: 1)]));
    await tester.pump();
    await tester.pump();

    // Paused: the message must be dropped, not applied.
    expect(container.read(graphProvider), revisionBefore);
    expect(container.read(graphProvider.notifier).nodes, isEmpty);

    // Resume via the button.
    await tester.tap(find.byKey(const Key('pauseResumeButton')));
    await tester.pump();
    expect(container.read(pausedProvider), isFalse);

    controller.add(GraphSnapshot(ts: 2, nodes: [_node(id: 1)]));
    await tester.pump();
    await tester.pump();

    // Resumed: the next message must be applied.
    expect(container.read(graphProvider), revisionBefore + 1);
    expect(container.read(graphProvider.notifier).nodes.containsKey(1), isTrue);
  });

  testWidgets('view mode toggle updates viewModeProvider', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    expect(container.read(viewModeProvider), ViewMode.graph);

    await tester.tap(find.text('Memory Map'));
    await tester.pump();

    expect(container.read(viewModeProvider), ViewMode.memoryMap);
  });

  testWidgets('min-size slider updates minSizeFilterProvider', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    expect(container.read(minSizeFilterProvider), 0);

    final sliderFinder = find.byKey(const Key('minSizeSlider'));
    await tester.drag(sliderFinder, const Offset(50, 0));
    await tester.pump();

    expect(container.read(minSizeFilterProvider), greaterThan(0));
  });

  testWidgets('orphan-only switch updates orphanOnlyFilterProvider',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    expect(container.read(orphanOnlyFilterProvider), isFalse);

    await tester.tap(find.byKey(const Key('orphanOnlySwitch')));
    await tester.pump();

    expect(container.read(orphanOnlyFilterProvider), isTrue);
  });

  testWidgets('symbol search field updates symbolSearchFilterProvider',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpControlBar(tester, controller);

    expect(container.read(symbolSearchFilterProvider), '');

    await tester.enterText(
      find.byKey(const Key('symbolSearchField')),
      'alloc::vec',
    );
    await tester.pump();

    expect(container.read(symbolSearchFilterProvider), 'alloc::vec');
  });
}
