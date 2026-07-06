import 'package:flutter_riverpod/flutter_riverpod.dart';

/// Whether incoming [GraphMessage]s should be dropped instead of applied.
///
/// Toggled by the control bar's pause/resume button. `graph_provider.dart`
/// checks this before calling `applyDiff` on each incoming message (see the
/// `ref.listen` wiring in `GraphNotifier.build`) — this is the "drop-while-
/// paused" strategy locked in the M5 task-3/6 briefs: pausing simply stops
/// applying messages as they arrive (they are not queued), and resuming
/// picks back up with whatever the next message happens to be. Because the
/// daemon's WS connection stays open and keeps broadcasting while paused,
/// the client may miss diffs while paused; the next `snapshot` (e.g. after a
/// reconnect) or subsequent diffs will still bring it back in sync — this is
/// an accepted tradeoff for the simplicity of not buffering messages.
final pausedProvider = StateProvider<bool>((ref) => false);
