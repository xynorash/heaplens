import 'package:flutter/material.dart';

import '../theme/xynorash_theme.dart';

/// A toggle sized to actually match the height of the label text it sits
/// next to, rather than the stock Material [Switch]'s fixed proportions.
/// Scaling a real `Switch` down (`Transform.scale`) still left it visibly
/// taller than a 12px label, because Material's switch track has its own
/// fixed minimum size that a naive scale factor doesn't reliably land on
/// — this widget's dimensions are set directly instead, so "match the
/// text" is exact rather than approximated.
class CompactSwitch extends StatelessWidget {
  const CompactSwitch({
    super.key,
    required this.value,
    required this.onChanged,
    this.height = 14,
  });

  final bool value;
  final ValueChanged<bool> onChanged;

  /// Track height (px) — pass the label text's line height so the two
  /// visually align. Width and thumb size are derived from this.
  final double height;

  @override
  Widget build(BuildContext context) {
    final width = height * 2;
    final thumbSize = height - 4;
    return GestureDetector(
      behavior: HitTestBehavior.opaque,
      onTap: () => onChanged(!value),
      child: AnimatedContainer(
        duration: const Duration(milliseconds: 150),
        width: width,
        height: height,
        padding: const EdgeInsets.all(2),
        decoration: BoxDecoration(
          color: value ? XynorashTheme.cyan.withValues(alpha: 0.55) : Colors.white24,
          borderRadius: BorderRadius.circular(height / 2),
        ),
        child: AnimatedAlign(
          duration: const Duration(milliseconds: 150),
          curve: Curves.easeOut,
          alignment: value ? Alignment.centerRight : Alignment.centerLeft,
          child: Container(
            width: thumbSize,
            height: thumbSize,
            decoration: BoxDecoration(
              color: value ? XynorashTheme.cyan : Colors.white70,
              shape: BoxShape.circle,
            ),
          ),
        ),
      ),
    );
  }
}

/// One option in a [CompactSegmented] toggle.
class CompactSegmentOption<T> {
  const CompactSegmentOption({required this.value, required this.label});

  final T value;
  final String label;
}

/// A small pill-shaped segmented toggle sized to match its label text's
/// height — the same "match the text, not a stock widget's fixed
/// proportions" fix as [CompactSwitch], applied to render-mode-style
/// choices (a stock Material `SegmentedButton` renders as a noticeably
/// larger pill than the 12px label next to it, same issue the switch had).
class CompactSegmented<T> extends StatelessWidget {
  const CompactSegmented({
    super.key,
    required this.options,
    required this.selected,
    required this.onChanged,
    this.height = 20,
  });

  final List<CompactSegmentOption<T>> options;
  final T selected;
  final ValueChanged<T> onChanged;
  final double height;

  @override
  Widget build(BuildContext context) {
    return Container(
      height: height,
      padding: const EdgeInsets.all(2),
      decoration: BoxDecoration(
        color: Colors.white10,
        borderRadius: BorderRadius.circular(height / 2),
      ),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          for (final option in options)
            GestureDetector(
              behavior: HitTestBehavior.opaque,
              onTap: () => onChanged(option.value),
              child: AnimatedContainer(
                duration: const Duration(milliseconds: 150),
                padding: const EdgeInsets.symmetric(horizontal: 10),
                alignment: Alignment.center,
                decoration: BoxDecoration(
                  color: option.value == selected
                      ? XynorashTheme.cyan.withValues(alpha: 0.25)
                      : null,
                  borderRadius: BorderRadius.circular((height - 4) / 2),
                ),
                child: Text(
                  option.label,
                  style: TextStyle(
                    fontSize: 11,
                    fontWeight: option.value == selected ? FontWeight.w600 : FontWeight.normal,
                    color: option.value == selected ? XynorashTheme.cyan : Colors.white54,
                  ),
                ),
              ),
            ),
        ],
      ),
    );
  }
}

/// The app's one signature visual device: a thin sci-fi-HUD corner
/// bracket at each corner of [child], instead of a Material rounded-card
/// border. Applied consistently to every major panel (ribbon, right
/// rail, insights panel, node detail) — restrained (thin stroke, low
/// alpha by default) rather than decorative, the way a real cockpit
/// display's panel edges are functional framing, not ornament.
class HudFrame extends StatelessWidget {
  const HudFrame({
    super.key,
    required this.child,
    this.color,
    this.cornerLength = 12,
    this.strokeWidth = 1.4,
  });

  final Widget child;
  final Color? color;
  final double cornerLength;
  final double strokeWidth;

  @override
  Widget build(BuildContext context) {
    return CustomPaint(
      foregroundPainter: _HudCornerPainter(
        color: color ?? XynorashTheme.hudLine(),
        cornerLength: cornerLength,
        strokeWidth: strokeWidth,
      ),
      child: child,
    );
  }
}

class _HudCornerPainter extends CustomPainter {
  _HudCornerPainter({required this.color, required this.cornerLength, required this.strokeWidth});

  final Color color;
  final double cornerLength;
  final double strokeWidth;

  @override
  void paint(Canvas canvas, Size size) {
    final paint = Paint()
      ..color = color
      ..strokeWidth = strokeWidth
      ..style = PaintingStyle.stroke
      ..strokeCap = StrokeCap.round;
    final len = cornerLength.clamp(0.0, size.shortestSide / 2);

    // Top-left
    canvas.drawLine(Offset(0, len), Offset.zero, paint);
    canvas.drawLine(Offset.zero, Offset(len, 0), paint);
    // Top-right
    canvas.drawLine(Offset(size.width - len, 0), Offset(size.width, 0), paint);
    canvas.drawLine(Offset(size.width, 0), Offset(size.width, len), paint);
    // Bottom-left
    canvas.drawLine(Offset(0, size.height - len), Offset(0, size.height), paint);
    canvas.drawLine(Offset(0, size.height), Offset(len, size.height), paint);
    // Bottom-right
    canvas.drawLine(Offset(size.width - len, size.height), Offset(size.width, size.height), paint);
    canvas.drawLine(Offset(size.width, size.height), Offset(size.width, size.height - len), paint);
  }

  @override
  bool shouldRepaint(covariant _HudCornerPainter oldDelegate) =>
      oldDelegate.color != color ||
      oldDelegate.cornerLength != cornerLength ||
      oldDelegate.strokeWidth != strokeWidth;
}

/// One grouped, visually-separated unit in the top bar (e.g. "attach
/// control", "summary metrics"). A thin vertical divider marks the
/// boundary between cells so related controls read as one group without
/// needing a heavier border around every cell.
class TopBarCell extends StatelessWidget {
  const TopBarCell({
    super.key,
    required this.children,
    this.trailingDivider = true,
  });

  final List<Widget> children;
  final bool trailingDivider;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Padding(
          // More generous than the original 12/6 — the ribbon now spans
          // the full window width, so cramming cells tight to the left
          // reads as unfinished rather than deliberate. This is purely
          // breathing room; no functional change.
          padding: const EdgeInsets.symmetric(horizontal: 22, vertical: 14),
          child: Row(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.center,
            children: children,
          ),
        ),
        if (trailingDivider)
          Container(width: 1, height: 40, color: XynorashTheme.purple.withValues(alpha: 0.35)),
      ],
    );
  }
}

/// A plain-language label paired with its technical value — the dual-level
/// legibility pattern used throughout the top bar and right rail: the
/// label is always the human-readable name of the thing, the value is
/// always the real, precise data next to it. An optional [tooltip] gives
/// the plain-sentence definition on hover; the label/value themselves are
/// never replaced by the tooltip, only supplemented.
class MetricTile extends StatelessWidget {
  const MetricTile({
    super.key,
    required this.label,
    required this.value,
    this.tooltip,
    this.valueColor,
  });

  final String label;
  final String value;
  final String? tooltip;
  final Color? valueColor;

  @override
  Widget build(BuildContext context) {
    final content = Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Text('$label: ', style: const TextStyle(color: Colors.white60, fontSize: 12)),
        // Values are data (counts, byte totals, pids) — monospace marks
        // them as such at a glance, distinct from the plain-language
        // label next to them.
        Text(value, style: XynorashTheme.mono(fontSize: 13, color: valueColor ?? Colors.white, fontWeight: FontWeight.bold)),
      ],
    );
    if (tooltip == null) return content;
    return Tooltip(message: tooltip!, child: content);
  }
}

/// An explicit empty/waiting state — used anywhere a panel would otherwise
/// render blank (no target attached, no node selected, daemon not yet
/// connected). Always says what's missing and, where relevant, what to do
/// about it, per the legibility rule that empty panels are never bare.
/// Styled as a terminal prompt line (`> message`) rather than a plain
/// italic sentence — an empty panel in this app reads as "awaiting input",
/// which a prompt communicates more precisely than a caption would.
class EmptyState extends StatelessWidget {
  const EmptyState({super.key, required this.message, this.icon});

  final String message;
  final IconData? icon;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 16),
      child: Row(
        mainAxisSize: MainAxisSize.min,
        children: [
          if (icon != null) ...[
            Icon(icon, size: 14, color: Colors.white30),
            const SizedBox(width: 8),
          ],
          Text(
            '> $message',
            style: XynorashTheme.mono(fontSize: 12, color: Colors.white38),
          ),
        ],
      ),
    );
  }
}

/// One always-on, non-collapsible section of the right rail — no chevron,
/// no tap-to-collapse, always showing [child] in full. [muted] demotes a
/// section's visual weight (smaller, dimmer text) for developer-diagnostic
/// content that isn't the primary audience's concern, without hiding it.
///
/// Pass [expand]: true (default false) to have this section grow to fill
/// any remaining vertical space in its parent `Column` — used for the last
/// section (verbose logs) so the rail's total content stretches to fill
/// the window instead of stopping partway down and leaving dead space.
/// Requires the immediate parent to be a `Column`/`Flex`, per `Expanded`'s
/// own contract.
class RailSection extends StatelessWidget {
  const RailSection({
    super.key,
    required this.title,
    required this.child,
    this.muted = false,
    this.expand = false,
  });

  final String title;
  final Widget child;
  final bool muted;
  final bool expand;

  @override
  Widget build(BuildContext context) {
    final titleColor = muted ? Colors.white38 : XynorashTheme.cyan;
    final header = Padding(
      padding: const EdgeInsets.fromLTRB(12, 10, 12, 6),
      child: Text(
        XynorashTheme.bracketLabel(title),
        overflow: TextOverflow.ellipsis,
        style: XynorashTheme.sectionHeader(color: titleColor, fontSize: muted ? 10 : 12),
      ),
    );
    final body = Padding(
      padding: const EdgeInsets.fromLTRB(12, 0, 12, 12),
      child: child,
    );

    final content = Column(
      crossAxisAlignment: CrossAxisAlignment.stretch,
      mainAxisSize: expand ? MainAxisSize.max : MainAxisSize.min,
      children: [
        header,
        if (expand) Expanded(child: body) else body,
        if (!expand) const Divider(height: 1, color: Colors.white12),
      ],
    );

    if (!expand) return content;
    return Expanded(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          const Divider(height: 1, color: Colors.white12),
          Expanded(child: content),
        ],
      ),
    );
  }
}
