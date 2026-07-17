import 'package:flutter/foundation.dart';

/// Which of the four target-health states currently applies. See
/// [TargetDiagnosis.classify] for the exact trigger conditions.
enum TargetStatus {
  /// Events are arriving and/or the graph looks healthy — no diagnosis
  /// needed. The banner renders nothing (or a subtle indicator) for this
  /// status.
  capturing,

  /// Attached, but zero allocation events observed over [kNoEventsWindow]
  /// after attach — the target may be idle, or use a heap path HeapLens
  /// doesn't hook.
  noEvents,

  /// Allocations are arriving and nodes exist, but almost no ownership
  /// edges formed — phi could not build topology for this target.
  noEdges,

  /// Same root cause as [noEdges], but confirmed rather than merely
  /// inferred: effective-site names are predominantly hex/`0x…` fallbacks,
  /// i.e. the target itself is unsymbolized.
  unsymbolized,
}

/// Minimum wall-clock time after we start observing a session before "zero
/// events" is treated as a real diagnosis rather than "the daemon just
/// hasn't had a chance to report anything yet". Attach time itself isn't
/// known client-side (the daemon doesn't export a wall-clock attach
/// timestamp — see target_diagnostics_provider.dart), so this is measured
/// from when this client first observed the session, which is an
/// acceptable approximation for a diagnostic banner, not a precision
/// measurement.
const Duration kNoEventsWindow = Duration(seconds: 3);

/// Below this edge-to-node ratio, a nonempty graph is considered to have
/// "no ownership structure" for diagnostic purposes. Not exactly zero, so a
/// handful of incidental phi-ambiguity edges (see graph.rs's documented,
/// accepted misattribution case) don't prevent the diagnosis from firing.
const double kNoEdgesRatioThreshold = 0.05;

/// Above this hex-fallback/total-symbol ratio, effective-site names are
/// considered "predominantly" unresolved.
const double kUnsymbolizedRatioThreshold = 0.8;

/// The current target-health diagnosis, plus the raw counts it was derived
/// from (for the banner's hover tooltip / the debug overlay — dual-level
/// legibility: the plain message is what most users read, the raw numbers
/// stay one hover away for anyone who wants them).
@immutable
class TargetDiagnosis {
  const TargetDiagnosis({
    required this.status,
    required this.message,
    required this.eventsReceived,
    required this.symbolsResolved,
    required this.hexFallback,
    required this.nodeCount,
    required this.edgeCount,
  });

  final TargetStatus status;

  /// Plain-language banner text, or null when [status] is
  /// [TargetStatus.capturing] (no banner needed).
  final String? message;

  final int eventsReceived;
  final int symbolsResolved;
  final int hexFallback;
  final int nodeCount;
  final int edgeCount;

  /// Initial value before any Stats message has arrived — deliberately
  /// [TargetStatus.capturing] (no banner) rather than [TargetStatus.noEvents],
  /// since we have no evidence yet either way.
  factory TargetDiagnosis.initial() => const TargetDiagnosis(
        status: TargetStatus.capturing,
        message: null,
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        nodeCount: 0,
        edgeCount: 0,
      );

  static TargetDiagnosis classify({
    required int eventsReceived,
    required int symbolsResolved,
    required int hexFallback,
    required int nodeCount,
    required int edgeCount,
    required bool pastNoEventsWindow,
    required int? targetPid,
    required String? targetName,
  }) {
    if (eventsReceived == 0) {
      if (!pastNoEventsWindow) {
        return TargetDiagnosis(
          status: TargetStatus.capturing,
          message: null,
          eventsReceived: eventsReceived,
          symbolsResolved: symbolsResolved,
          hexFallback: hexFallback,
          nodeCount: nodeCount,
          edgeCount: edgeCount,
        );
      }
      final label = (targetName != null && targetPid != null)
          ? 'Attached to `$targetName` [$targetPid]'
          : 'Attached';
      return TargetDiagnosis(
        status: TargetStatus.noEvents,
        message: '$label — no heap activity observed. The target may be '
            'idle, or may use a heap path HeapLens does not hook (e.g. the '
            'Windows segment heap).',
        eventsReceived: eventsReceived,
        symbolsResolved: symbolsResolved,
        hexFallback: hexFallback,
        nodeCount: nodeCount,
        edgeCount: edgeCount,
      );
    }

    final edgeRatio = nodeCount == 0 ? 0.0 : edgeCount / nodeCount;
    final totalSymbols = symbolsResolved + hexFallback;
    final hexRatio = totalSymbols == 0 ? 0.0 : hexFallback / totalSymbols;
    final unsymbolized = totalSymbols > 0 && hexRatio >= kUnsymbolizedRatioThreshold;

    if (nodeCount > 0 && edgeRatio < kNoEdgesRatioThreshold) {
      const base = 'Capturing allocations, but no ownership structure could '
          'be inferred — this target lacks the debug symbols HeapLens needs '
          'to build topology. Allocation sizes and growth are still '
          'available in the Map view.';
      return TargetDiagnosis(
        status: unsymbolized ? TargetStatus.unsymbolized : TargetStatus.noEdges,
        message: unsymbolized ? '$base Symbols unavailable for this target.' : base,
        eventsReceived: eventsReceived,
        symbolsResolved: symbolsResolved,
        hexFallback: hexFallback,
        nodeCount: nodeCount,
        edgeCount: edgeCount,
      );
    }

    return TargetDiagnosis(
      status: TargetStatus.capturing,
      message: null,
      eventsReceived: eventsReceived,
      symbolsResolved: symbolsResolved,
      hexFallback: hexFallback,
      nodeCount: nodeCount,
      edgeCount: edgeCount,
    );
  }
}
