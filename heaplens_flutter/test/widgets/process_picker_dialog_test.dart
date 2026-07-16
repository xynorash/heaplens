import 'dart:async';

import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/control.dart';
import 'package:heaplens_flutter/providers/target_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';
import 'package:heaplens_flutter/widgets/process_picker_dialog.dart';

/// Stage 7 Step 4 acceptance gate (partial — the rest is a manual run
/// against a real daemon+target per docs/stage7-injection-design.md §8):
/// this covers the picker's own UI contract in isolation — it requests the
/// process list on open, sends `AttachTarget` on selection, and surfaces
/// both success (records the attached target, closes) and failure (daemon's
/// own message text, verbatim, dialog stays open) — driven entirely through
/// fakes, no real daemon or WebSocket involved.
void main() {
  Future<(ProviderContainer, StreamController<ControlResponse>, List<String>)> pumpPicker(
    WidgetTester tester,
  ) async {
    final controlController = StreamController<ControlResponse>.broadcast();
    addTearDown(controlController.close);
    final sent = <String>[];
    final neverCloses = StreamController<dynamic>();
    addTearDown(neverCloses.close);
    final fakeConnection = GraphMessageConnection(
      connector: () => WsFrames(neverCloses.stream, () {}, sent.add),
      backoff: (_) => Duration.zero,
      onStatus: (_) {},
      onMessage: (_) {},
      onError: (e, st) {},
    );
    fakeConnection.start();
    addTearDown(fakeConnection.dispose);

    late ProviderContainer container;
    await tester.pumpWidget(
      ProviderScope(
        overrides: [
          controlResponseProvider.overrideWith((ref) => controlController.stream),
          wsConnectionProvider.overrideWithValue(fakeConnection),
        ],
        child: Consumer(
          builder: (context, ref, _) {
            container = ProviderScope.containerOf(context);
            return MaterialApp(
              home: Scaffold(
                body: Builder(
                  builder: (context) => ElevatedButton(
                    onPressed: () => showProcessPickerDialog(context),
                    child: const Text('open'),
                  ),
                ),
              ),
            );
          },
        ),
      ),
    );

    await tester.tap(find.text('open'));
    // Not pumpAndSettle: the dialog shows an indeterminate
    // CircularProgressIndicator while `_processes` is still null, which
    // animates forever and would make pumpAndSettle time out. A couple of
    // bounded pumps is enough to finish the dialog's own open transition.
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 300));

    return (container, controlController, sent);
  }

  testWidgets('opening the picker requests the process list and shows returned processes', (tester) async {
    final (_, controlController, sent) = await pumpPicker(tester);

    expect(sent, [
      '{"type":"list_processes"}',
    ]);
    expect(find.byKey(const Key('processPickerList')), findsNothing); // still loading

    controlController.add(const ProcessListResponse([
      ProcessInfo(pid: 111, name: 'target.exe', arch: 'x64'),
      ProcessInfo(pid: 222, name: 'legacy32.exe', arch: 'x86'),
    ]));
    await tester.pumpAndSettle();

    expect(find.byKey(const Key('processPickerList')), findsOneWidget);
    expect(find.text('target.exe'), findsOneWidget);
    expect(find.text('legacy32.exe'), findsOneWidget);
  });

  testWidgets('an x86 process is shown disabled, with an explanatory subtitle, not hidden', (tester) async {
    final (_, controlController, _) = await pumpPicker(tester);

    controlController.add(const ProcessListResponse([
      ProcessInfo(pid: 222, name: 'legacy32.exe', arch: 'x86'),
    ]));
    await tester.pumpAndSettle();

    final tile = tester.widget<ListTile>(find.byKey(const Key('processPickerItem_222')));
    expect(tile.enabled, isFalse);
    expect(find.textContaining('64-bit and cannot attach'), findsOneWidget);
  });

  testWidgets('selecting a process sends AttachTarget and a successful reply records + closes', (tester) async {
    final (container, controlController, sent) = await pumpPicker(tester);

    controlController.add(const ProcessListResponse([
      ProcessInfo(pid: 111, name: 'target.exe', arch: 'x64'),
    ]));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('processPickerItem_111')));
    await tester.pump();

    expect(sent, [
      '{"type":"list_processes"}',
      '{"type":"attach_target","pid":111}',
    ]);

    controlController.add(const AttachResultResponse(ok: true, message: 'attached to pid 111'));
    await tester.pumpAndSettle();

    // Dialog closed.
    expect(find.byKey(const Key('processPickerList')), findsNothing);
    final attached = container.read(attachedTargetProvider);
    expect(attached?.pid, 111);
    expect(attached?.name, 'target.exe');
  });

  testWidgets('an attach failure surfaces the daemon\'s message verbatim; dialog stays open', (tester) async {
    final (container, controlController, _) = await pumpPicker(tester);

    controlController.add(const ProcessListResponse([
      ProcessInfo(pid: 999999999, name: 'ghost.exe', arch: 'x64'),
    ]));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const Key('processPickerItem_999999999')));
    await tester.pump();

    controlController.add(const AttachResultResponse(
      ok: false,
      message: 'cannot open process 999999999 — access denied, or the process does not exist.',
    ));
    await tester.pumpAndSettle();

    // Dialog stays open — not a silent no-op.
    expect(find.byKey(const Key('processPickerList')), findsOneWidget);
    expect(
      find.byKey(const Key('processPickerError')),
      findsOneWidget,
    );
    expect(find.textContaining('access denied'), findsOneWidget);
    expect(container.read(attachedTargetProvider), isNull);
  });
}
