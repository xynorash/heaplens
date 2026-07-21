import 'package:flutter/material.dart';

/// XYNORASH color theme — lifted directly from the real fastfetch config
/// at `xynorash-pwsh/fastfetch/config.jsonc` (RGB values below are copied
/// verbatim from that file's `38;2;R;G;B` ANSI true-color codes), so the
/// app's chrome matches the same palette used everywhere else in this
/// setup. Single source of truth: every widget that used to hardcode its
/// own copy of the healthy/orphan/hot/connection-status/severity colors
/// now reads from here instead.
class XynorashTheme {
  XynorashTheme._();

  /// `"keys": "38;2;170;0;255"` — the config's primary accent.
  static const Color purple = Color(0xFFAA00FF);

  /// `"title": "38;2;0;229;255"` — the config's secondary accent.
  static const Color cyan = Color(0xFF00E5FF);

  /// Shell/Term/Font/Vol color (`38;2;0;229;170`) — already almost
  /// exactly the app's pre-existing "healthy" teal (0xFF2DD4BF); this is
  /// the theme's precise hue.
  static const Color teal = Color(0xFF00E5AA);

  /// CPU/DDR color (`38;2;255;170;0`) — replaces the generic Material
  /// amber previously used for "hot".
  static const Color orange = Color(0xFFFFAA00);

  /// GPU/Display color (`38;2;255;100;50`) — a coral-toned red-orange,
  /// close to the app's pre-existing orphan coral; this is the theme's
  /// precise hue.
  static const Color coral = Color(0xFFFF6432);

  /// Disk color (`38;2;238;0;238`).
  static const Color magenta = Color(0xFFEE00EE);

  /// Media/Now color (`38;2;255;200;50`).
  static const Color yellow = Color(0xFFFFC832);

  // --- Surface depth ---------------------------------------------------
  //
  // A vitals-HUD reads as layers of a cockpit display, not one flat gray
  // with lighter rectangles floating on it — each surface below is one
  // genuine step darker/lighter than the next, not just an opacity trick.

  /// True window background — the deepest layer.
  static const Color bgBase = Color(0xFF0A0B0F);

  /// Panels resting directly on the base (right rail, insights panel).
  static const Color bgPanel = Color(0xFF12141B);

  /// The ribbon — one step up again, it's the app's "always on top" HUD bar.
  static const Color bgRibbon = Color(0xFF161822);

  /// Hairline / bracket-frame color at rest (very low alpha, present but
  /// not shouting).
  static Color hudLine({double alpha = 0.35}) => cyan.withValues(alpha: alpha);

  // --- Typography --------------------------------------------------------

  /// Monospace family for every technical/numeric value (ptr, size, ts,
  /// byte counts, pid, revision, log lines) — Consolas is present on
  /// every Windows install this app targets; 'monospace' is the
  /// cross-platform fallback. Deliberately not a bundled custom font: no
  /// asset fetch available, and a well-chosen system font applied
  /// *consistently* reads more intentional than an inconsistently-applied
  /// downloaded one.
  static const String monoFamily = 'Consolas';
  static const List<String> monoFallback = ['Cascadia Code', 'monospace'];

  static TextStyle mono({
    double fontSize = 12,
    Color color = Colors.white,
    FontWeight fontWeight = FontWeight.normal,
  }) => TextStyle(
    fontFamily: monoFamily,
    fontFamilyFallback: monoFallback,
    fontSize: fontSize,
    color: color,
    fontWeight: fontWeight,
  );

  /// Section-header treatment: uppercase + letter-spacing — the
  /// terminal-label convention ("NODE DETAIL", not "Node detail") used
  /// for every panel/section title, achievable with the system sans
  /// alone rather than needing a distinct display face.
  static TextStyle sectionHeader({Color? color, double fontSize = 12}) => TextStyle(
    color: color ?? cyan,
    fontSize: fontSize,
    fontWeight: FontWeight.w700,
    letterSpacing: 1.6,
  );

  /// Wraps [title] in the `[ TITLE ]` bracket convention and applies
  /// [sectionHeader]'s uppercase/letter-spacing treatment in one call —
  /// the single place this exact bracket format is defined, so every
  /// panel header stays identical if it ever changes.
  static String bracketLabel(String title) => '[ ${title.toUpperCase()} ]';

  static ThemeData darkTheme() {
    final scheme = ColorScheme.fromSeed(
      seedColor: cyan,
      brightness: Brightness.dark,
      primary: cyan,
      secondary: purple,
    );
    return ThemeData.dark(useMaterial3: true).copyWith(
      colorScheme: scheme,
      scaffoldBackgroundColor: bgBase,
      switchTheme: SwitchThemeData(
        thumbColor: WidgetStateProperty.resolveWith(
          (states) => states.contains(WidgetState.selected) ? cyan : null,
        ),
        trackColor: WidgetStateProperty.resolveWith(
          (states) =>
              states.contains(WidgetState.selected) ? cyan.withValues(alpha: 0.5) : null,
        ),
      ),
      sliderTheme: const SliderThemeData(activeTrackColor: cyan, thumbColor: cyan),
      segmentedButtonTheme: SegmentedButtonThemeData(
        style: ButtonStyle(
          backgroundColor: WidgetStateProperty.resolveWith(
            (states) =>
                states.contains(WidgetState.selected) ? cyan.withValues(alpha: 0.25) : null,
          ),
          foregroundColor: WidgetStateProperty.resolveWith(
            (states) => states.contains(WidgetState.selected) ? cyan : null,
          ),
        ),
      ),
    );
  }
}
