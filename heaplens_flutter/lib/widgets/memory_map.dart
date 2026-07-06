import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/node.dart';
import '../providers/filter_providers.dart';
import '../providers/graph_provider.dart';
import '../providers/selection_provider.dart';
import 'node_colors.dart';

/// Fixed cell size (width/height, in logical pixels) for each grid cell.
const double _kCellSize = 28.0;
const double _kCellSpacing = 2.0;

/// Default column count when parent provides unbounded (infinite) width.
const int _kDefaultColumnsWhenUnbounded = 20;

/// Address-ordered "memory map" view: every live node gets exactly one cell,
/// sorted by [NodeDto.ptr] ascending and laid into a grid whose column count
/// is derived from the available width. Cell area is NOT proportional to
/// node size for M5 — "one cell per node, ordered by ptr" is sufficient per
/// the task brief.
///
/// This widget applies the Task 6 rendering-only filters
/// ([minSizeFilterProvider], [orphanOnlyFilterProvider],
/// [symbolSearchFilterProvider]) directly to what it renders: filtered-out
/// nodes are simply omitted from the grid. This never mutates or touches the
/// underlying node map owned by [graphProvider] — the filters are purely
/// about what this view chooses to draw.
class MemoryMap extends ConsumerWidget {
  const MemoryMap({super.key});

  @override
  Widget build(BuildContext context, WidgetRef ref) {
    // Watched purely to know *when* to rebuild; the fresh node map is read
    // below rather than cached across revisions (same contract as
    // graph_canvas.dart).
    ref.watch(graphProvider);
    final nodes = ref.read(graphProvider.notifier).nodes;
    final selectedId = ref.watch(selectedNodeIdProvider);

    final minSize = ref.watch(minSizeFilterProvider);
    final orphanOnly = ref.watch(orphanOnlyFilterProvider);
    final symbolSearch = ref.watch(symbolSearchFilterProvider).toLowerCase();

    final visible = nodes.values.where((n) {
      if (!n.live) return false;
      if (n.size < minSize) return false;
      if (orphanOnly && n.state != NodeStateDto.orphan) return false;
      if (symbolSearch.isNotEmpty &&
          !n.symbol.toLowerCase().contains(symbolSearch)) {
        return false;
      }
      return true;
    }).toList()
      ..sort((a, b) => a.ptr.compareTo(b.ptr));

    return LayoutBuilder(
      builder: (context, constraints) {
        final columns = constraints.maxWidth.isFinite
            ? ((constraints.maxWidth) / (_kCellSize + _kCellSpacing))
                .floor()
                .clamp(1, 1 << 30)
            : _kDefaultColumnsWhenUnbounded;

        return GridView.builder(
          padding: const EdgeInsets.all(_kCellSpacing),
          gridDelegate: SliverGridDelegateWithFixedCrossAxisCount(
            crossAxisCount: columns,
            crossAxisSpacing: _kCellSpacing,
            mainAxisSpacing: _kCellSpacing,
          ),
          itemCount: visible.length,
          itemBuilder: (context, index) {
            final node = visible[index];
            return _MemoryMapCell(
              key: ValueKey(node.id),
              node: node,
              selected: node.id == selectedId,
              onTap: () {
                ref.read(selectedNodeIdProvider.notifier).state = node.id;
              },
            );
          },
        );
      },
    );
  }
}

class _MemoryMapCell extends StatelessWidget {
  const _MemoryMapCell({
    super.key,
    required this.node,
    required this.selected,
    required this.onTap,
  });

  final NodeDto node;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    return GestureDetector(
      onTap: onTap,
      child: Tooltip(
        message: '${node.symbol}\n'
            'ptr: 0x${node.ptr.toRadixString(16)}\n'
            'size: ${node.size}',
        child: Container(
          decoration: BoxDecoration(
            color: colorForState(node.state),
            border: selected
                ? Border.all(color: Colors.white, width: 2)
                : null,
          ),
        ),
      ),
    );
  }
}
