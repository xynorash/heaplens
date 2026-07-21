import 'dart:async';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/control.dart';
import 'package:heaplens_flutter/providers/target_provider.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';

void main() {
  group('AttachedTargetNotifier', () {
    late StreamController<ControlResponse> controller;
    late ProviderContainer container;

    setUp(() {
      controller = StreamController<ControlResponse>();
      container = ProviderContainer(
        overrides: [
          controlResponseProvider.overrideWith((ref) => controller.stream),
        ],
      );
      addTearDown(container.dispose);
      addTearDown(controller.close);
      // Activate the notifier (and its ref.listen to controlResponseProvider).
      container.read(attachedTargetProvider);
    });

    test('starts as null (nothing attached)', () {
      expect(container.read(attachedTargetProvider), isNull);
    });

    test('setAttached records the target explicitly (not derived from a response alone)', () {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );
      final state = container.read(attachedTargetProvider);
      expect(state?.pid, 4242);
      expect(state?.name, 'target.exe');
    });

    test('a successful DetachResult clears the attached target', () async {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );
      expect(container.read(attachedTargetProvider), isNotNull);

      controller.add(const DetachResultResponse(ok: true, message: 'detached'));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(attachedTargetProvider), isNull);
    });

    test('a failed DetachResult does NOT clear the attached target', () async {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );

      controller.add(const DetachResultResponse(ok: false, message: 'writer thread did not stop'));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(attachedTargetProvider)?.pid, 4242);
    });

    test('TargetExited for the currently-tracked pid clears it', () async {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );

      controller.add(const TargetExitedResponse(4242));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(attachedTargetProvider), isNull);
    });

    test('TargetExited for a DIFFERENT pid than currently tracked is ignored', () async {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );

      // A stale push for some other/prior pid must not clobber current state.
      controller.add(const TargetExitedResponse(9999));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(attachedTargetProvider)?.pid, 4242);
    });

    test('clear() optimistically clears without waiting for a DetachResult', () {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );
      container.read(attachedTargetProvider.notifier).clear();
      expect(container.read(attachedTargetProvider), isNull);
    });

    test('ProcessListResponse/AttachResultResponse do not affect attached state', () async {
      container.read(attachedTargetProvider.notifier).setAttached(
            const AttachedTarget(pid: 4242, name: 'target.exe'),
          );

      controller.add(const ProcessListResponse([]));
      controller.add(const AttachResultResponse(ok: true, message: 'attached to pid 1'));
      await Future<void>.delayed(Duration.zero);

      expect(container.read(attachedTargetProvider)?.pid, 4242);
    });
  });
}
