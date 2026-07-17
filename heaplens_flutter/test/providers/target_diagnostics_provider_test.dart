import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/models/target_diagnosis.dart';
import 'package:heaplens_flutter/providers/graph_provider.dart';
import 'package:heaplens_flutter/providers/target_diagnostics_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';

NodeDto _node({required int id, bool live = true, List<int> edges = const []}) {
  return NodeDto(
    id: id,
    ptr: id,
    size: 8,
    ts: 0,
    symbol: 'sym',
    live: live,
    state: NodeStateDto.healthy,
    edges: edges,
  );
}

void main() {
  group('TargetDiagnosticsNotifier', () {
    late StreamController<GraphMessage> controller;
    late ProviderContainer container;
    late DateTime clock;

    setUp(() {
      controller = StreamController<GraphMessage>();
      clock = DateTime(2026, 1, 1);
      container = ProviderContainer(
        overrides: [
          graphMessageProvider.overrideWith((ref) => controller.stream),
        ],
      );
      addTearDown(container.dispose);
      addTearDown(() => controller.close());

      // Activate the notifier and pin its clock before any message arrives.
      container.read(targetDiagnosticsProvider.notifier).now = () => clock;
      container.listen(targetDiagnosticsProvider, (_, _) {});
    });

    test('initial state is capturing with no message', () {
      expect(container.read(targetDiagnosticsProvider).status, TargetStatus.capturing);
      expect(container.read(targetDiagnosticsProvider).message, isNull);
    });

    test('zero events, still within the no-events window: stays capturing', () async {
      controller.add(const GraphStats(
        ts: 1,
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
      ));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(targetDiagnosticsProvider).status, TargetStatus.capturing);
    });

    test('zero events, past the no-events window: transitions to noEvents', () async {
      controller.add(const GraphStats(
        ts: 1,
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        targetPid: 99,
        targetName: 'idle.exe',
      ));
      await Future<void>.delayed(Duration.zero);
      expect(container.read(targetDiagnosticsProvider).status, TargetStatus.capturing);

      // Advance the injected clock past the window and push a second stats
      // message (recompute only happens on message arrival).
      clock = clock.add(kNoEventsWindow + const Duration(seconds: 1));
      controller.add(const GraphStats(
        ts: 2,
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        targetPid: 99,
        targetName: 'idle.exe',
      ));
      await Future<void>.delayed(Duration.zero);

      final diag = container.read(targetDiagnosticsProvider);
      expect(diag.status, TargetStatus.noEvents);
      expect(diag.message, contains('idle.exe'));
      expect(diag.message, contains('[99]'));
    });

    test('nodes with no edges (via graphProvider) combined with events flowing: noEdges', () async {
      // Populate the node map directly through graphProvider, as the real
      // app does via its own listener on the same graphMessageProvider
      // stream.
      container.read(graphProvider.notifier).applyDiff(
        GraphSnapshot(ts: 1, nodes: [_node(id: 1), _node(id: 2), _node(id: 3)]),
      );

      controller.add(const GraphStats(
        ts: 1,
        eventsReceived: 500,
        symbolsResolved: 10,
        hexFallback: 0,
      ));
      await Future<void>.delayed(Duration.zero);

      final diag = container.read(targetDiagnosticsProvider);
      expect(diag.status, TargetStatus.noEdges);
      expect(diag.nodeCount, 3);
      expect(diag.edgeCount, 0);
    });

    test('a later Diff message (no accompanying Stats) still recomputes node/edge counts', () async {
      controller.add(const GraphStats(ts: 1, eventsReceived: 500, symbolsResolved: 10, hexFallback: 0));
      await Future<void>.delayed(Duration.zero);
      expect(container.read(targetDiagnosticsProvider).status, TargetStatus.capturing);

      // A diff arrives that adds nodes with no edges, with no new Stats
      // message accompanying it — the diagnosis must still update because
      // _handleMessage recomputes on every message, not only on Stats.
      controller.add(GraphDiff(ts: 2, add: [_node(id: 1), _node(id: 2)], update: [], remove: []));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(targetDiagnosticsProvider).status, TargetStatus.noEdges);
    });

    test('predominantly hex-fallback symbols alongside no edges: unsymbolized, folded into the noEdges message', () async {
      container.read(graphProvider.notifier).applyDiff(
        GraphSnapshot(ts: 1, nodes: List.generate(10, (i) => _node(id: i))),
      );
      controller.add(const GraphStats(
        ts: 1,
        eventsReceived: 500,
        symbolsResolved: 1,
        hexFallback: 99,
      ));
      await Future<void>.delayed(Duration.zero);

      final diag = container.read(targetDiagnosticsProvider);
      expect(diag.status, TargetStatus.unsymbolized);
      expect(diag.message, contains('Symbols unavailable for this target.'));
    });
  });
}
