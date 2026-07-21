import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/control.dart';
import '../models/node.dart';
import '../providers/filter_providers.dart';
import '../providers/graph_provider.dart';
import '../providers/paused_provider.dart';
import '../providers/target_provider.dart';
import '../providers/view_mode_provider.dart';
import '../providers/ws_provider.dart';
import '../theme/xynorash_theme.dart';
import 'node_colors.dart';
import 'process_picker_dialog.dart';
import 'ui_common.dart';

/// Colors for each [ConnectionStatus], used for the small status dot.
/// Same [XynorashTheme] hues as [kNodeStateColors] — connected/connecting/
/// disconnected map onto the same teal/amber/coral roles as
/// healthy/hot/orphan, so the color language stays consistent between
/// "is a node okay" and "is the connection okay".
const Map<ConnectionStatus, Color> kConnectionStatusColors = {
  ConnectionStatus.connected: XynorashTheme.teal,
  ConnectionStatus.connecting: XynorashTheme.orange,
  ConnectionStatus.disconnected: XynorashTheme.coral,
};

/// Human-readable label for each [ConnectionStatus], surfaced as plain text
/// (in addition to the colored dot) so status is not conveyed by color alone.
const Map<ConnectionStatus, String> kConnectionStatusLabels = {
  ConnectionStatus.connected: 'Connected',
  ConnectionStatus.connecting: 'Connecting…',
  ConnectionStatus.disconnected: 'Disconnected',
};

/// The ribbon: five bordered, two-line cells, matching the wireframe
/// template exactly —
///   1. Attach (line 1) / connection state (line 2).
///   2. Owners + Nodes (line 1) / Orphans + Bytes (line 2).
///   3. Min size + slider (line 1) / Render mode + Graph/Map toggle (line 2).
///   4. Process Name (line 1) / PID (line 2), or "No target attached".
///   5. Orphans only (line 1) / Search symbol (line 2).
///
/// Every raw/technical value from the flat single-line layout is retained
/// — this changes grouping/labeling/legibility only. All existing `Key`s
/// are preserved.
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
    final attached = ref.watch(attachedTargetProvider);
    final orphanCount = notifier.orphanCount;

    // Stage 7 §4.4: surface target-exit as a legible, one-time banner
    // rather than a silent state change — the graph itself doesn't visibly
    // announce "the target that was producing these nodes is gone."
    ref.listen<AsyncValue<ControlResponse>>(controlResponseProvider, (previous, next) {
      next.whenData((resp) {
        if (resp is TargetExitedResponse) {
          ScaffoldMessenger.of(context).showSnackBar(
            SnackBar(content: Text('Target process (pid ${resp.pid}) exited')),
          );
        }
      });
    });

    final cells = <Widget>[
            // --- Cell 1: Attach / connection state ---
            TopBarCell(
              children: [
                Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    _AttachControl(attached: attached, ref: ref),
                    const SizedBox(height: 10),
                    _ConnectionIndicator(status: status),
                  ],
                ),
              ],
            ),

            // --- Cell 2: Summary metrics, two lines ---
            TopBarCell(
              children: [
                Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        MetricTile(
                          label: 'Owners',
                          value: '${notifier.ownerCount}',
                          tooltip: 'Live allocations that own at least one other allocation.',
                        ),
                        const SizedBox(width: 24),
                        MetricTile(
                          label: 'Nodes',
                          value: '${notifier.liveNodeCount}',
                          tooltip: 'All currently live allocations being tracked.',
                        ),
                      ],
                    ),
                    const SizedBox(height: 10),
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        MetricTile(
                          label: 'Orphans',
                          value: '$orphanCount',
                          tooltip: kNodeStateDescriptions[NodeStateDto.orphan]!,
                          // Consistent color language: orphan = coral
                          // everywhere (graph nodes, the state chip, and
                          // here).
                          valueColor:
                              orphanCount > 0 ? colorForState(NodeStateDto.orphan) : null,
                        ),
                        const SizedBox(width: 24),
                        MetricTile(
                          label: 'Bytes',
                          value: '${notifier.totalLiveBytes}',
                          tooltip: 'Total size of all live allocations, summed.',
                        ),
                      ],
                    ),
                  ],
                ),
              ],
            ),

            // --- Cell 3: View controls, two lines ---
            TopBarCell(
              children: [
                Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        IconButton(
                          key: const Key('pauseResumeButton'),
                          tooltip: paused ? 'Resume' : 'Pause',
                          icon: Icon(paused ? Icons.play_arrow : Icons.pause, size: 18),
                          onPressed: () {
                            ref.read(pausedProvider.notifier).state = !paused;
                          },
                          padding: EdgeInsets.zero,
                          constraints: const BoxConstraints(),
                        ),
                        const SizedBox(width: 14),
                        Tooltip(
                          message: 'Hide allocations smaller than this many bytes.',
                          child: SizedBox(
                            width: 180,
                            child: Row(
                              mainAxisSize: MainAxisSize.min,
                              children: [
                                Text(
                                  'Min size: ${minSize.toStringAsFixed(0)}',
                                  style: const TextStyle(color: Colors.white70, fontSize: 12),
                                ),
                                Expanded(
                                  child: SliderTheme(
                                    // Same "match the text, not the stock
                                    // widget's fixed proportions" fix as
                                    // CompactSwitch/CompactSegmented — a
                                    // default Slider's thumb and touch
                                    // target are visibly larger than the
                                    // 12px label next to it. Stays a real
                                    // Slider (not a custom widget) since
                                    // the existing test drags it, and a
                                    // continuous-drag control is worth
                                    // keeping Material's real gesture
                                    // handling for.
                                    data: SliderThemeData(
                                      trackHeight: 2,
                                      thumbShape: const RoundSliderThumbShape(
                                        enabledThumbRadius: 5,
                                      ),
                                      overlayShape: const RoundSliderOverlayShape(
                                        overlayRadius: 10,
                                      ),
                                    ),
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
                                ),
                              ],
                            ),
                          ),
                        ),
                      ],
                    ),
                    const SizedBox(height: 10),
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Text(
                          'Render mode: ',
                          style: TextStyle(color: Colors.white70, fontSize: 12),
                        ),
                        CompactSegmented<ViewMode>(
                          key: const Key('viewModeToggle'),
                          // Matches the 12px "Render mode:" label's line
                          // height directly, same fix as CompactSwitch —
                          // the stock SegmentedButton this replaced
                          // rendered as a noticeably taller pill.
                          height: 20,
                          options: const [
                            CompactSegmentOption(value: ViewMode.graph, label: 'Graph'),
                            CompactSegmentOption(
                              value: ViewMode.memoryMap,
                              label: 'Memory Map',
                            ),
                          ],
                          selected: viewMode,
                          onChanged: (mode) {
                            ref.read(viewModeProvider.notifier).state = mode;
                          },
                        ),
                      ],
                    ),
                  ],
                ),
              ],
            ),

            // --- Cell 4: Target identity, two lines ---
            TopBarCell(
              children: [_TargetIdentity(attached: attached)],
            ),

            // --- Cell 5: Filters, two lines (far right, no trailing divider) ---
            TopBarCell(
              trailingDivider: false,
              children: [
                Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Text(
                          'Orphans only',
                          style: TextStyle(color: Colors.white70, fontSize: 12),
                        ),
                        const SizedBox(width: 6),
                        CompactSwitch(
                          key: const Key('orphanOnlySwitch'),
                          value: orphanOnly,
                          // Matches the 12px label's line height directly
                          // (see CompactSwitch's doc — this is an exact
                          // dimension, not a scale-factor approximation).
                          height: 14,
                          onChanged: (v) {
                            ref.read(orphanOnlyFilterProvider.notifier).state = v;
                          },
                        ),
                      ],
                    ),
                    const SizedBox(height: 10),
                    Row(
                      mainAxisSize: MainAxisSize.min,
                      children: [
                        const Text(
                          'Search symbol: ',
                          style: TextStyle(color: Colors.white70, fontSize: 12),
                        ),
                        SizedBox(
                          width: 140,
                          child: TextField(
                            key: const Key('symbolSearchField'),
                            decoration: const InputDecoration(isDense: true),
                            onChanged: (v) {
                              ref.read(symbolSearchFilterProvider.notifier).state = v;
                            },
                          ),
                        ),
                      ],
                    ),
                  ],
                ),
              ],
            ),
    ];

    return Container(
      decoration: BoxDecoration(
        color: XynorashTheme.bgRibbon,
        border: Border(bottom: BorderSide(color: XynorashTheme.hudLine(alpha: 0.5), width: 1)),
      ),
      child: Material(
        color: Colors.transparent,
        child: Padding(
          padding: const EdgeInsets.symmetric(vertical: 10),
          // The five cells must actually spread across the full ribbon
          // width, not just sit left-aligned with the background
          // stretched behind them (that was the previous bug — the
          // background reached the edge, the content didn't). A plain Row
          // with spaceBetween does that, but hard-overflows if the window
          // is ever narrower than the cells' combined natural width — so
          // this only switches to Row once there's genuinely enough room
          // (matches what a real app window has); below that it falls
          // back to the same wrapping Wrap as before, which degrades to
          // multiple lines instead of overflowing.
          child: LayoutBuilder(
            builder: (context, constraints) {
              final naturalWidth = _measureNaturalWidth(cells);
              if (constraints.maxWidth >= naturalWidth) {
                return Row(
                  mainAxisAlignment: MainAxisAlignment.spaceBetween,
                  crossAxisAlignment: CrossAxisAlignment.center,
                  children: cells,
                );
              }
              return Wrap(
                crossAxisAlignment: WrapCrossAlignment.center,
                spacing: 8,
                runSpacing: 8,
                children: cells,
              );
            },
          ),
        ),
      ),
    );
  }

  /// Conservative threshold deciding `Row` (spread across full width) vs.
  /// `Wrap` (safe reflow) — deliberately overestimates each cell's real
  /// width rather than trying to measure it exactly. Getting this wrong
  /// on the *low* side would pick `Row` for a window that's actually too
  /// narrow for it and hard-overflow (`Row` cannot shrink below its
  /// children's fixed-width contents, e.g. the 180px min-size slider or
  /// the 140px search field); getting it wrong on the *high* side only
  /// means a genuinely-wide-enough window still gets the safe `Wrap`
  /// fallback instead of the fully-spread `Row` — a visual miss, not a
  /// crash. 320px/cell comfortably covers every cell's real content
  /// (measured against the actual rendered app) while staying well below
  /// a typical desktop window's width, and well above the 800px default
  /// test viewport (so tests keep exercising the proven-safe `Wrap` path).
  static double _measureNaturalWidth(List<Widget> cells) {
    return cells.length * 320.0;
  }
}

class _ConnectionIndicator extends StatelessWidget {
  const _ConnectionIndicator({required this.status});

  final ConnectionStatus status;

  @override
  Widget build(BuildContext context) {
    return Tooltip(
      message: 'Connection to the HeapLens daemon.',
      child: Row(
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
          const SizedBox(width: 10),
          Text(
            kConnectionStatusLabels[status]!,
            style: const TextStyle(color: Colors.white70, fontSize: 12),
          ),
        ],
      ),
    );
  }
}

/// "Attach to Process…" button when nothing is attached, or a compact
/// "attached" indicator plus a Detach button when one is. The full
/// name/pid readout lives in the Target identity cell (Cell 4) — this
/// control's job is only the primary attach/detach action, kept far left
/// and prominent per the legibility brief.
class _AttachControl extends StatelessWidget {
  const _AttachControl({required this.attached, required this.ref});

  final AttachedTarget? attached;
  final WidgetRef ref;

  @override
  Widget build(BuildContext context) {
    final attached = this.attached;
    if (attached == null) {
      return ElevatedButton.icon(
        key: const Key('attachButton'),
        icon: const Icon(Icons.link, size: 16),
        label: const Text('Attach'),
        onPressed: () => showProcessPickerDialog(context),
      );
    }
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text(
          key: const Key('attachedTargetLabel'),
          '${attached.name} (pid ${attached.pid})',
          style: XynorashTheme.mono(fontSize: 12, color: Colors.white70),
        ),
        const SizedBox(width: 10),
        IconButton(
          key: const Key('detachButton'),
          tooltip: 'Detach',
          icon: const Icon(Icons.link_off, size: 16),
          padding: EdgeInsets.zero,
          constraints: const BoxConstraints(),
          onPressed: () {
            ref.read(wsConnectionProvider).sendRequest(const DetachTargetRequest());
            ref.read(attachedTargetProvider.notifier).clear();
          },
        ),
      ],
    );
  }
}

/// Target identity cell: the attached process's name and PID, each on
/// their own line — or an explicit "No target attached" state rather
/// than a blank cell, per the legibility brief's empty-state rule.
class _TargetIdentity extends StatelessWidget {
  const _TargetIdentity({required this.attached});

  final AttachedTarget? attached;

  @override
  Widget build(BuildContext context) {
    final attached = this.attached;
    if (attached == null) {
      return const EmptyState(
        key: Key('noTargetAttached'),
        message: 'No target attached',
        icon: Icons.link_off,
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      mainAxisSize: MainAxisSize.min,
      children: [
        MetricTile(label: 'Process Name', value: attached.name),
        const SizedBox(height: 10),
        MetricTile(label: 'PID', value: '${attached.pid}'),
      ],
    );
  }
}
