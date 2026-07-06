import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../simulation/force_layout.dart';

/// Single shared [ForceLayout] instance for the app's lifetime.
///
/// Fed by `main.dart`'s dedicated `ref.listen(graphMessageProvider, ...)`
/// (independent of `graph_provider.dart`'s own internal listener — see the
/// doc on `_GraphOrchestrator` in main.dart for why there are two separate
/// listeners on the same message stream) and read by `graph_canvas.dart` for
/// both physics-driven positions and hit-testing.
///
/// Deliberately a plain (non-autoDispose) [Provider]: the layout must persist
/// for the whole app lifetime, not be torn down when nothing is watching it.
final forceLayoutProvider = Provider<ForceLayout>((ref) => ForceLayout());
