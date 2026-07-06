import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../providers/filter_providers.dart';
import '../providers/graph_provider.dart';
import '../providers/paused_provider.dart';
import '../providers/view_mode_provider.dart';
import '../providers/ws_provider.dart';

/// Colors for each [ConnectionStatus], used for the small status dot.
const Map<ConnectionStatus, Color> kConnectionStatusColors = {
  ConnectionStatus.connected: Color(0xFF2DD4BF), // teal
  ConnectionStatus.connecting: Color(0xFFFFC107), // amber
  ConnectionStatus.disconnected: Color(0xFFFF7F50), // coral
};

/// Human-readable label for each [ConnectionStatus], surfaced as plain text
/// (in addition to the colored dot) so status is not conveyed by color alone.
const Map<ConnectionStatus, String> kConnectionStatusLabels = {
  ConnectionStatus.connected: 'Connected',
  ConnectionStatus.connecting: 'Connecting…',
  ConnectionStatus.disconnected: 'Disconnected',
};

/// Top control bar: connection status, live counters, pause/resume, view
/// toggle, and rendering-only filters (min-size slider, orphan-only toggle,
/// symbol search).
///
/// Deliberately simple — a horizontal row of controls, not a polished design
/// pass. Filters defined here (`filter_providers.dart`) are consumed by a
/// later task's rendering code (`graph_canvas.dart`/`memory_map.dart`); this
/// widget only writes to them.
class ControlBar extends ConsumerWidget {
  const ControlBar({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    final status = ref.watch(connectionStatusProvider);
    // Watched purely to know *when* to rebuild for updated counters; the
    // actual counts are read fresh from the notifier's derived getters,
    // per the documented graph_provider.dart contract.
    ref.watch(graphProvider);
    final notifier = ref.read(graphProvider.notifier);
    final paused = ref.watch(pausedProvider);
    final viewMode = ref.watch(viewModeProvider);
    final minSize = ref.watch(minSizeFilterProvider);
    final orphanOnly = ref.watch(orphanOnlyFilterProvider);

    return Material(
      color: const Color(0xFF1E1E1E),
      child: Padding(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
        child: Wrap(
          spacing: 16,
          runSpacing: 8,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            _ConnectionIndicator(status: status),
            _Counter(label: 'Nodes', value: '${notifier.liveNodeCount}'),
            _Counter(label: 'Orphans', value: '${notifier.orphanCount}'),
            _Counter(label: 'Bytes', value: '${notifier.totalLiveBytes}'),
            IconButton(
              key: const Key('pauseResumeButton'),
              tooltip: paused ? 'Resume' : 'Pause',
              icon: Icon(paused ? Icons.play_arrow : Icons.pause),
              onPressed: () {
                ref.read(pausedProvider.notifier).state = !paused;
              },
            ),
            SegmentedButton<ViewMode>(
              key: const Key('viewModeToggle'),
              segments: const [
                ButtonSegment(
                  value: ViewMode.graph,
                  label: Text('Graph'),
                  icon: Icon(Icons.hub_outlined),
                ),
                ButtonSegment(
                  value: ViewMode.memoryMap,
                  label: Text('Memory Map'),
                  icon: Icon(Icons.grid_view),
                ),
              ],
              selected: {viewMode},
              onSelectionChanged: (selection) {
                ref.read(viewModeProvider.notifier).state = selection.first;
              },
            ),
            SizedBox(
              width: 200,
              child: Row(
                mainAxisSize: MainAxisSize.min,
                children: [
                  const Text('Min size', style: TextStyle(color: Colors.white70)),
                  Expanded(
                    child: Slider(
                      key: const Key('minSizeSlider'),
                      value: minSize,
                      min: 0,
                      max: 4096,
                      onChanged: (v) {
                        ref.read(minSizeFilterProvider.notifier).state = v;
                      },
                    ),
                  ),
                ],
              ),
            ),
            Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Text('Orphans only', style: TextStyle(color: Colors.white70)),
                Switch(
                  key: const Key('orphanOnlySwitch'),
                  value: orphanOnly,
                  onChanged: (v) {
                    ref.read(orphanOnlyFilterProvider.notifier).state = v;
                  },
                ),
              ],
            ),
            SizedBox(
              width: 160,
              child: TextField(
                key: const Key('symbolSearchField'),
                decoration: const InputDecoration(
                  hintText: 'Search symbol…',
                  isDense: true,
                ),
                onChanged: (v) {
                  ref.read(symbolSearchFilterProvider.notifier).state = v;
                },
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class _ConnectionIndicator extends StatelessWidget {
  const _ConnectionIndicator({required this.status});

  final ConnectionStatus status;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Container(
          key: const Key('connectionStatusDot'),
          width: 10,
          height: 10,
          decoration: BoxDecoration(
            color: kConnectionStatusColors[status],
            shape: BoxShape.circle,
          ),
        ),
        const SizedBox(width: 6),
        Text(
          kConnectionStatusLabels[status]!,
          style: const TextStyle(color: Colors.white70),
        ),
      ],
    );
  }
}

class _Counter extends StatelessWidget {
  const _Counter({required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text('$label: ', style: const TextStyle(color: Colors.white70)),
        Text(
          value,
          style: const TextStyle(color: Colors.white, fontWeight: FontWeight.bold),
        ),
      ],
    );
  }
}
