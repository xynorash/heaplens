import 'package:flutter_riverpod/flutter_riverpod.dart';

/// Rendering-only filters written to by the control bar and read by
/// `graph_canvas.dart` / `memory_map.dart` (a future task/follow-up wires
/// the actual read side — this task only defines the providers and the UI
/// controls that write to them).
///
/// Important: none of these filter the underlying node map owned by
/// `graph_provider.dart` — they only affect what a given view chooses to
/// draw. The full node map (and its derived getters like `orphanCount`)
/// always reflects true daemon state regardless of these filters.

/// Minimum node size (bytes) to render; nodes below this threshold are
/// hidden from the view, not removed from state. Default 0 (show all).
final minSizeFilterProvider = StateProvider<double>((ref) => 0);

/// When true, a view should render only nodes in the `orphan` state.
/// Default false (show all states).
final orphanOnlyFilterProvider = StateProvider<bool>((ref) => false);

/// Case-insensitive substring filter against `NodeDto.symbol`. Empty string
/// means "no filter, show all".
final symbolSearchFilterProvider = StateProvider<String>((ref) => '');
