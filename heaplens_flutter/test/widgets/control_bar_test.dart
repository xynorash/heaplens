import 'dart:async';
import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/control.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/providers/filter_providers.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/paused_provider.dart';
import 'package:heaplens_flutter/providers/target_provider.dart';
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
  StreamController<GraphMessage> controller, {
  List<Override> extraOverrides = const [],
}) async {
  late ProviderContainer container;
  await tester.pumpWidget(
    ProviderScope(
      overrides: [
        graphMessageProvider.overrideWith((ref) => controller.stream),
        // ControlBar unconditionally `ref.listen`s controlResponseProvider
        // (Stage 7 §4.4's target-exit banner) — which builds it even when a
        // test doesn't care about it. Left unoverridden, that would build
        // the *real* provider and open a real WebSocket connection attempt
        // during every test in this file. An empty, never-emitting stream
        // is a safe default; tests that do care override it themselves via
        // extraOverrides.
        controlResponseProvider.overrideWith((ref) => const Stream.empty()),
        ...extraOverrides,
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

  group('attach/detach control (Stage 7 §3/Step 4)', () {
    testWidgets('shows "Attach to Process…" when nothing is attached', (tester) async {
      final controller = StreamController<GraphMessage>();
      addTearDown(() => controller.close());
      await _pumpControlBar(tester, controller);

      expect(find.byKey(const Key('attachButton')), findsOneWidget);
      expect(find.byKey(const Key('detachButton')), findsNothing);
    });

    // Coverage gap flagged after the injection-safety-gate merge (2026-07-21):
    // the tests above only ever checked presence/absence of the button by
    // key, never whether it was actually *disabled*, nor what the
    // safety-explanation tooltip actually says. A future edit could silently
    // re-enable the button, or water down/drop the explanation, without
    // failing anything — this closes that gap.
    //
    // Re-scoped 2026-07-22 when kAttachEnabled flipped to true (the hook
    // defect this gate existed for was root-caused, fixed, and validated —
    // see README.txt): this now pins the *enabled*-branch behavior instead.
    // A future re-disable of the gate should update this test back, not
    // leave it silently asserting the wrong branch.
    testWidgets(
        'Attach button is genuinely enabled (onPressed is set, not just '
        'styled) and carries the exact capability/safety-boundary tooltip '
        'while kAttachEnabled is true', (tester) async {
      final controller = StreamController<GraphMessage>();
      addTearDown(() => controller.close());
      await _pumpControlBar(tester, controller);

      // Pin the real, current gate state this test's assertions depend on —
      // if this ever flips back, the assertions below intentionally stop
      // matching production behavior, which is exactly the point.
      expect(kAttachEnabled, isTrue,
          reason: 'this test asserts the *enabled* branch; it must be '
              'updated (or a companion disabled-state test added) if this '
              'flag ever flips back to false');

      final button = tester.widget<ElevatedButton>(
        find.byKey(const Key('attachButton')),
      );
      expect(
        button.onPressed,
        isNotNull,
        reason: 'must be genuinely enabled (onPressed != null), not just '
            'visually styled to look enabled',
      );

      final tooltip = tester.widget<Tooltip>(
        find.byKey(const Key('attachEnabledTooltip')),
      );
      expect(
        tooltip.message,
        kAttachEnabledInfo,
        reason: 'the capability/safety-boundary tooltip text must match '
            'exactly — a future edit that waters down or drops the '
            'automatic driver-exclusion or validated-duration disclosure '
            'must fail here, not pass silently',
      );
    });

    testWidgets('shows the attached target\'s name/pid and a Detach button once attached',
        (tester) async {
      final controller = StreamController<GraphMessage>();
      addTearDown(() => controller.close());
      final container = await _pumpControlBar(tester, controller);

      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );
      await tester.pump();

      expect(find.byKey(const Key('attachButton')), findsNothing);
      expect(find.byKey(const Key('attachedTargetLabel')), findsOneWidget);
      expect(find.text('target.exe (pid 4242)'), findsOneWidget);
      expect(find.byKey(const Key('detachButton')), findsOneWidget);
    });

    testWidgets('tapping Detach sends DetachTargetRequest and clears the attached target',
        (tester) async {
      final controller = StreamController<GraphMessage>();
      addTearDown(() => controller.close());
      final sent = <String>[];
      // A never-closing stream, not `Stream.empty()` — an empty stream
      // completes (fires `onDone`) as soon as it's listened to, which would
      // immediately trigger GraphMessageConnection's reconnect path and
      // clear the very `_sendCurrent` this test needs `sendRequest` to
      // forward through, before the tap below ever happens.
      final neverCloses = StreamController<dynamic>();
      addTearDown(neverCloses.close);
      final fakeConnection = GraphMessageConnection(
        connector: () => WsFrames(neverCloses.stream, () {}, sent.add),
        backoff: (_) => Duration.zero,
        onStatus: (_) {},
        onMessage: (_) {},
        onError: (e, st) {},
      );
      fakeConnection.start();
      addTearDown(fakeConnection.dispose);

      final container = await _pumpControlBar(
        tester,
        controller,
        extraOverrides: [
          wsConnectionProvider.overrideWithValue(fakeConnection),
        ],
      );

      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );
      await tester.pump();

      await tester.tap(find.byKey(const Key('detachButton')));
      await tester.pump();

      expect(sent, [jsonEncode(const DetachTargetRequest().toJson())]);
      // Detach is optimistic client-side (target_provider.dart's doc) — the
      // attached target clears immediately, before any daemon reply.
      expect(container.read(attachedTargetProvider), isNull);
      expect(find.byKey(const Key('attachButton')), findsOneWidget);
    });
  });
}
