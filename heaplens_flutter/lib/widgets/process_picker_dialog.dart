import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import '../models/control.dart';
import '../providers/target_provider.dart';
import '../providers/ws_provider.dart';

/// Stage 7 Step 4: the attach-target picker. Requests the process list on
/// open, lets the user pick a process, sends `AttachTarget`, and surfaces
/// the result — success closes the dialog and records the attached target;
/// failure (nonexistent/access-denied pid, per §3.3) stays open and shows
/// the daemon's own message text verbatim, not a generic "failed."
///
/// A 32-bit (`arch != "x64"`) process is shown but disabled with an
/// explanatory subtitle rather than hidden — matching §3.3's "this build of
/// HeapLens is 64-bit and cannot attach" message, surfaced *before* the
/// user wastes a click on a target that would only fail at attach time.
Future<void> showProcessPickerDialog(BuildContext context) {
  return showDialog<void>(
    context: context,
    builder: (context) => const _ProcessPickerDialog(),
  );
}

class _ProcessPickerDialog extends ConsumerStatefulWidget {
  const _ProcessPickerDialog();

  @override
  ConsumerState<_ProcessPickerDialog> createState() => _ProcessPickerDialogState();
}

class _ProcessPickerDialogState extends ConsumerState<_ProcessPickerDialog> {
  List<ProcessInfo>? _processes;
  String? _error;
  bool _attaching = false;
  String _filter = '';

  @override
  void initState() {
    super.initState();
    _requestList();
  }

  void _requestList() {
    setState(() {
      _processes = null;
      _error = null;
    });
    ref.read(wsConnectionProvider).sendRequest(const ListProcessesRequest());
  }

  Future<void> _attach(ProcessInfo process) async {
    if (process.arch != 'x64' || _attaching) return;
    setState(() {
      _attaching = true;
      _error = null;
    });

    // Start listening for the reply *before* sending, so a very fast daemon
    // reply can't arrive and be missed between the send and the listen.
    final replyFuture = _listenOnce<AttachResultResponse>();
    ref.read(wsConnectionProvider).sendRequest(AttachTargetRequest(process.pid));
    final resp = await replyFuture;
    if (!mounted) return;

    if (resp == null) {
      setState(() {
        _attaching = false;
        _error = 'No response from daemon (timed out).';
      });
      return;
    }
    if (resp.ok) {
      ref.read(attachedTargetProvider.notifier).setAttached(
            AttachedTarget(pid: process.pid, name: process.name),
          );
      if (mounted) Navigator.of(context).pop();
      return;
    }
    setState(() {
      _attaching = false;
      _error = resp.message;
    });
  }

  /// Sends nothing itself — waits for the next matching response already in
  /// flight from a request the caller already sent. Bounded so a daemon
  /// that never replies (crashed mid-attach, connection dropped) doesn't
  /// leave the dialog stuck showing a spinner forever.
  ///
  /// Uses one `Completer` driven by both the listener and an explicit,
  /// cancelable `Timer` — not `Future.any([completer.future, Future.delayed(...)])`,
  /// which was tried first: `Future.any` doesn't cancel the losing branch's
  /// own `Timer` once the other one wins, so a `Future.delayed` for the
  /// timeout kept running for the full 10s after a normal, fast reply
  /// already resolved things — harmless in the sense that its callback was
  /// a no-op by then, but it leaked a live platform timer for 10s past
  /// every successful attach, caught by `flutter test`'s
  /// "Timer still pending after widget tree disposed" check.
  Future<T?> _listenOnce<T extends ControlResponse>() async {
    final completer = Completer<T?>();
    late final ProviderSubscription<AsyncValue<ControlResponse>> sub;
    sub = ref.listenManual<AsyncValue<ControlResponse>>(controlResponseProvider, (previous, next) {
      next.whenData((resp) {
        if (resp is T && !completer.isCompleted) {
          completer.complete(resp);
        }
      });
    });
    final timer = Timer(const Duration(seconds: 10), () {
      if (!completer.isCompleted) completer.complete(null);
    });
    final result = await completer.future;
    timer.cancel();
    sub.close();
    return result;
  }

  @override
  Widget build(BuildContext context) {
    ref.listen<AsyncValue<ControlResponse>>(controlResponseProvider, (previous, next) {
      next.whenData((resp) {
        if (resp is ProcessListResponse && mounted) {
          setState(() => _processes = resp.processes);
        }
      });
    });

    final processes = _processes;
    final List<ProcessInfo> filtered = processes == null
        ? <ProcessInfo>[]
        : processes.where((p) {
            if (_filter.isEmpty) return true;
            final f = _filter.toLowerCase();
            return p.name.toLowerCase().contains(f) || p.pid.toString().contains(f);
          }).toList();
    filtered.sort((a, b) => a.name.toLowerCase().compareTo(b.name.toLowerCase()));

    return Dialog(
      child: SizedBox(
        width: 480,
        height: 520,
        child: Column(
          children: [
            Padding(
              padding: const EdgeInsets.all(16),
              child: Row(
                children: [
                  const Expanded(
                    child: Text('Attach to process', style: TextStyle(fontSize: 18, fontWeight: FontWeight.bold)),
                  ),
                  IconButton(
                    key: const Key('processPickerRefresh'),
                    tooltip: 'Refresh',
                    icon: const Icon(Icons.refresh),
                    onPressed: _requestList,
                  ),
                ],
              ),
            ),
            Padding(
              padding: const EdgeInsets.symmetric(horizontal: 16),
              child: TextField(
                key: const Key('processPickerSearchField'),
                decoration: const InputDecoration(hintText: 'Filter by name or pid…', isDense: true),
                onChanged: (v) => setState(() => _filter = v),
              ),
            ),
            if (_error != null)
              Padding(
                padding: const EdgeInsets.all(12),
                child: Text(
                  _error!,
                  key: const Key('processPickerError'),
                  style: const TextStyle(color: Colors.redAccent),
                ),
              ),
            Expanded(
              child: processes == null
                  ? const Center(child: CircularProgressIndicator())
                  : filtered.isEmpty
                      ? const Center(child: Text('No matching processes'))
                      : ListView.builder(
                          key: const Key('processPickerList'),
                          itemCount: filtered.length,
                          itemBuilder: (context, i) {
                            final p = filtered[i];
                            final attachable = p.arch == 'x64';
                            return ListTile(
                              key: Key('processPickerItem_${p.pid}'),
                              enabled: attachable && !_attaching,
                              title: Text(p.name),
                              subtitle: Text(
                                attachable
                                    ? 'pid ${p.pid} · ${p.arch}'
                                    : 'pid ${p.pid} · ${p.arch} — this build of HeapLens is 64-bit and cannot attach',
                              ),
                              onTap: attachable ? () => _attach(p) : null,
                            );
                          },
                        ),
            ),
            if (_attaching) const LinearProgressIndicator(key: Key('processPickerAttaching')),
            OverflowBar(
              children: [
                TextButton(
                  onPressed: () => Navigator.of(context).pop(),
                  child: const Text('Cancel'),
                ),
              ],
            ),
          ],
        ),
      ),
    );
  }
}
