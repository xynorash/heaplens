import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/target_diagnostics_provider.dart';
import 'package:heaplens_flutter/providers/view_mode_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/target_status_banner.dart';

NodeDto _node({required int id, List<int> edges = const []}) {
  return NodeDto(
    id: id,
    ptr: id,
    size: 8,
    ts: 0,
    symbol: 'sym',
    live: true,
    state: NodeStateDto.healthy,
    edges: edges,
  );
}

Future<ProviderContainer> _pumpBanner(
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
            home: Scaffold(body: TargetStatusBanner()),
          );
        },
      ),
    ),
  );
  return container;
}

void main() {
  testWidgets('renders nothing while capturing (no message yet)', (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    await _pumpBanner(tester, controller);

    expect(find.byKey(const Key('targetStatusBanner')), findsNothing);
  });

  testWidgets('shows the no-events message once past the window, with name/pid',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpBanner(tester, controller);

    final notifier = container.read(targetDiagnosticsProvider.notifier);
    var now = DateTime(2026, 1, 1);
    notifier.now = () => now;

    controller.add(const GraphStats(
      ts: 1,
      eventsReceived: 0,
      symbolsResolved: 0,
      hexFallback: 0,
      targetPid: 777,
      targetName: 'idle_target.exe',
    ));
    // The stream -> Riverpod StreamProvider -> ref.listen chain delivers
    // across more than one microtask hop, so a single tester.pump() isn't
    // guaranteed to observe the update in the same frame it arrived on.
    // NOTE: use a second bare pump() here, not `Future.delayed` — under
    // flutter_test's fake-async zone, a real Timer (which is what
    // Future.delayed schedules, even for Duration.zero) never fires unless
    // the fake clock is explicitly advanced, so awaiting one directly
    // deadlocks the test until the runner's outer wall-clock timeout.
    await tester.pump();
    await tester.pump();
    expect(find.byKey(const Key('targetStatusBanner')), findsNothing);

    now = now.add(const Duration(seconds: 4));
    controller.add(const GraphStats(
      ts: 2,
      eventsReceived: 0,
      symbolsResolved: 0,
      hexFallback: 0,
      targetPid: 777,
      targetName: 'idle_target.exe',
    ));
    await tester.pump();
    await tester.pump();

    expect(find.byKey(const Key('targetStatusBanner')), findsOneWidget);
    expect(find.textContaining('idle_target.exe'), findsOneWidget);
    expect(find.textContaining('[777]'), findsOneWidget);
    // No-events state has no Map-view action — the Map view doesn't help
    // with "nothing is happening".
    expect(find.byKey(const Key('switchToMapViewButton')), findsNothing);
  });

  testWidgets('no-edges banner offers a Switch to Map view action that updates viewModeProvider',
      (tester) async {
    final controller = StreamController<GraphMessage>();
    addTearDown(() => controller.close());
    final container = await _pumpBanner(tester, controller);

    container.read(graphProvider.notifier).applyDiff(
      GraphSnapshot(ts: 1, nodes: List.generate(10, (i) => _node(id: i))),
    );
    controller.add(const GraphStats(ts: 1, eventsReceived: 500, symbolsResolved: 10, hexFallback: 0));
    await tester.pump();
    await tester.pump();

    expect(find.byKey(const Key('targetStatusBanner')), findsOneWidget);
    expect(find.textContaining('no ownership structure could be inferred'), findsOneWidget);
    expect(find.byKey(const Key('switchToMapViewButton')), findsOneWidget);

    expect(container.read(viewModeProvider), ViewMode.graph);
    await tester.tap(find.byKey(const Key('switchToMapViewButton')));
    await tester.pump();
    expect(container.read(viewModeProvider), ViewMode.memoryMap);
  });
}
