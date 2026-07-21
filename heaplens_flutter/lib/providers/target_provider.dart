import 'package:flutter/foundation.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/control.dart';
import 'ws_provider.dart';

/// The currently-attached target, as far as this Flutter client knows.
///
/// `name` is carried along from the [ProcessInfo] the picker showed at
/// selection time, purely for display — the daemon's `AttachResult` reply
/// doesn't echo it back (only `ok`/`message`, see
/// `heaplens_protocol::control::ControlResponse`), so it has nowhere else
/// to come from.
@immutable
class AttachedTarget {
  final int pid;
  final String name;

  const AttachedTarget({required this.pid, required this.name});
}

/// Tracks the attached target across attach/detach/target-exit, Stage 7
/// §3/§4.4.
///
/// Attach is set explicitly by the picker (`setAttached`) once it observes
/// a successful `AttachResult` — the notifier itself only *listens* for the
/// ways a session ends (`DetachResult{ok:true}`, the unprompted
/// `TargetExited` push) and clears itself, following the same
/// automatic-wiring-in-`build()` pattern `GraphNotifier` uses for
/// `graphMessageProvider`.
class AttachedTargetNotifier extends Notifier<AttachedTarget?> {
  @override
  AttachedTarget? build() {
    ref.listen<AsyncValue<ControlResponse>>(controlResponseProvider, (previous, next) {
      next.whenData((resp) {
        switch (resp) {
          case DetachResultResponse(ok: true):
            state = null;
          case TargetExitedResponse(:final pid):
            // Only clear if it's *our* tracked target — a stale push for a
            // pid we're not currently tracking (e.g. arriving just after we
            // already switched away from it) must not clobber newer state.
            if (state?.pid == pid) state = null;
          case ProcessListResponse():
          case AttachResultResponse():
          case DetachResultResponse():
            break;
        }
      });
    });
    return null;
  }

  /// Called by the picker after it receives `AttachResult{ok: true}` for
  /// this pid — see the class doc for why this can't be derived from the
  /// response alone.
  void setAttached(AttachedTarget target) {
    state = target;
  }

  /// Optimistically clears local state right after sending `DetachTarget`,
  /// without waiting for the daemon's reply — mirrors how `paused_provider`
  /// and other UI-only toggles in this app favor immediate feedback; the
  /// `DetachResult{ok:true}` listener above is what actually confirms it
  /// and would be a no-op if this already cleared it.
  void clear() {
    state = null;
  }
}

final attachedTargetProvider = NotifierProvider<AttachedTargetNotifier, AttachedTarget?>(
  AttachedTargetNotifier.new,
);
