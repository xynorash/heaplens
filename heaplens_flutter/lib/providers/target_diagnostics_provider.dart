import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/graph_diff.dart';
import '../models/target_diagnosis.dart';
import 'graph_provider.dart';
import 'ws_provider.dart';

/// Derives [TargetDiagnosis] from two independent inputs that arrive on
/// different cadences: [GraphStats] messages (roughly once a second, see
/// heaplens-daemon's main.rs) for events/symbol counters, and the live node
/// map (via [graphProvider]) for node/edge counts, which can change on
/// every [GraphDiff]. Recomputes on every message of either kind so the
/// diagnosis never lags more than one message behind either input.
class TargetDiagnosticsNotifier extends Notifier<TargetDiagnosis> {
  /// Injectable clock so tests can control "how long since we started
  /// observing this session" without real `Duration(seconds: 3)` sleeps.
  /// Mutable (not constructor-injected) because Riverpod constructs
  /// `Notifier`s itself via the bare `.new` factory — tests instead grab
  /// `container.read(targetDiagnosticsProvider.notifier)` and override this
  /// field before pushing messages.
  DateTime Function() now = DateTime.now;

  DateTime? _firstObservedAt;
  int _eventsReceived = 0;
  int _symbolsResolved = 0;
  int _hexFallback = 0;
  int? _targetPid;
  String? _targetName;

  @override
  TargetDiagnosis build() {
    // Force GraphNotifier.build() to run first, registering its own
    // ref.listen(graphMessageProvider, ...) ahead of ours — mirrors the
    // ordering hazard documented on main.dart's _GraphOrchestrator class:
    // without this, a Diff/Snapshot message could reach our listener before
    // graph_provider.dart has applied it, and _recompute() would read stale
    // node/edge counts for that one message.
    ref.read(graphProvider);

    ref.listen<AsyncValue<GraphMessage>>(graphMessageProvider, (previous, next) {
      next.whenData(_handleMessage);
    });

    return TargetDiagnosis.initial();
  }

  void _handleMessage(GraphMessage message) {
    if (message is GraphStats) {
      _firstObservedAt ??= now();
      _eventsReceived = message.eventsReceived;
      _symbolsResolved = message.symbolsResolved;
      _hexFallback = message.hexFallback;
      _targetPid = message.targetPid;
      _targetName = message.targetName;
    }
    _recompute();
  }

  void _recompute() {
    final notifier = ref.read(graphProvider.notifier);
    final liveNodes = notifier.nodes.values.where((n) => n.live);
    final nodeCount = liveNodes.length;
    final edgeCount = liveNodes.fold<int>(0, (sum, n) => sum + n.edges.length);

    final pastWindow = _firstObservedAt != null &&
        now().difference(_firstObservedAt!) >= kNoEventsWindow;

    state = TargetDiagnosis.classify(
      eventsReceived: _eventsReceived,
      symbolsResolved: _symbolsResolved,
      hexFallback: _hexFallback,
      nodeCount: nodeCount,
      edgeCount: edgeCount,
      pastNoEventsWindow: pastWindow,
      targetPid: _targetPid,
      targetName: _targetName,
    );
  }
}

/// Public entry point: `ref.watch(targetDiagnosticsProvider)` for the
/// current diagnosis, re-derived automatically as Stats/Diff/Snapshot
/// messages arrive.
final targetDiagnosticsProvider =
    NotifierProvider<TargetDiagnosticsNotifier, TargetDiagnosis>(
  TargetDiagnosticsNotifier.new,
);
