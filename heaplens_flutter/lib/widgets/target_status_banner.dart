import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/target_diagnosis.dart';
import '../providers/target_diagnostics_provider.dart';
import '../providers/view_mode_provider.dart';
import '../theme/xynorash_theme.dart';

/// Colors for each non-[TargetStatus.capturing] state — [noEvents] reads as
/// a caution amber (target may simply be idle, not necessarily a problem),
/// [noEdges]/[unsymbolized] read as an informational blue (capturing is
/// working; the limitation is inherent to the target, not an error).
/// [noEdgesOrphaned] reuses the shared orphan coral ([kNodeStateColors]) —
/// same semantic as every other orphan indicator in the app, since this
/// state literally means "most of the graph is orphaned", not a capture
/// problem.
const Map<TargetStatus, Color> _kBannerColors = {
  TargetStatus.noEvents: Color(0xFFB08900),
  TargetStatus.noEdges: Color(0xFF2D6CA6),
  TargetStatus.unsymbolized: Color(0xFF2D6CA6),
  TargetStatus.noEdgesOrphaned: XynorashTheme.coral,
};

const Map<TargetStatus, IconData> _kBannerIcons = {
  TargetStatus.noEvents: Icons.hourglass_empty,
  TargetStatus.noEdges: Icons.hub_outlined,
  TargetStatus.unsymbolized: Icons.hub_outlined,
  TargetStatus.noEdgesOrphaned: Icons.link_off,
};

/// Honest target-diagnostic banner: explains *why* the graph looks empty or
/// edgeless instead of leaving the user staring at a blank canvas. Renders
/// nothing for [TargetStatus.capturing] (the common, healthy case).
///
/// Dual-level legibility: the plain-language [TargetDiagnosis.message] is
/// what's on screen; the raw counts it was derived from (events, symbols
/// resolved/unresolved, nodes, edges) are one hover away via [Tooltip] and
/// also mirrored into [DebugOverlay] — never lost, never required reading
/// to understand the banner's headline claim.
class TargetStatusBanner extends ConsumerWidget {
  const TargetStatusBanner({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final diagnosis = ref.watch(targetDiagnosticsProvider);
    if (diagnosis.status == TargetStatus.capturing) {
      return const SizedBox.shrink();
    }

    final color = _kBannerColors[diagnosis.status]!;
    final icon = _kBannerIcons[diagnosis.status]!;
    final showMapViewAction = diagnosis.status == TargetStatus.noEdges ||
        diagnosis.status == TargetStatus.unsymbolized ||
        diagnosis.status == TargetStatus.noEdgesOrphaned;

    return Material(
      key: const Key('targetStatusBanner'),
      color: color.withValues(alpha: 0.16),
      child: Container(
        decoration: BoxDecoration(
          border: Border(bottom: BorderSide(color: color.withValues(alpha: 0.4))),
        ),
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        child: Tooltip(
          message: 'events=${diagnosis.eventsReceived}  '
              'symbols=${diagnosis.symbolsResolved}/'
              '${diagnosis.symbolsResolved + diagnosis.hexFallback} resolved  '
              'nodes=${diagnosis.nodeCount}  edges=${diagnosis.edgeCount}',
          child: Row(
            children: [
              Icon(icon, color: color, size: 18),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  diagnosis.message!,
                  style: const TextStyle(color: Colors.white),
                ),
              ),
              if (showMapViewAction)
                TextButton(
                  key: const Key('switchToMapViewButton'),
                  onPressed: () {
                    ref.read(viewModeProvider.notifier).state = ViewMode.memoryMap;
                  },
                  child: const Text('Switch to Map view'),
                ),
            ],
          ),
        ),
      ),
    );
  }
}
