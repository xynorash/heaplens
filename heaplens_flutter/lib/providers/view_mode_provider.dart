import 'package:flutter_riverpod/flutter_riverpod.dart';

/// Which center-panel widget is currently shown. Read by `main.dart` (Task
/// 9) to decide between the force-directed graph canvas and the memory-map
/// treemap-style view (`memory_map.dart`, Task 7 — not yet built on this
/// branch). This task only defines and exposes the enum/provider; the
/// control bar's toggle button writes to it.
enum ViewMode { graph, memoryMap }

/// Current view mode, toggled by the control bar. Defaults to [ViewMode.graph]
/// since that's the only view implemented so far on this branch.
final viewModeProvider = StateProvider<ViewMode>((ref) => ViewMode.graph);
