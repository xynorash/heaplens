import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/target_diagnosis.dart';

void main() {
  group('TargetDiagnosis.classify', () {
    test('zero events, before the no-events window: capturing, no message', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        nodeCount: 0,
        edgeCount: 0,
        pastNoEventsWindow: false,
        targetPid: 4242,
        targetName: 'target.exe',
      );
      expect(d.status, TargetStatus.capturing);
      expect(d.message, isNull);
    });

    test('zero events, past the no-events window: noEvents with name/pid label', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        nodeCount: 0,
        edgeCount: 0,
        pastNoEventsWindow: true,
        targetPid: 4242,
        targetName: 'target.exe',
      );
      expect(d.status, TargetStatus.noEvents);
      expect(d.message, contains('Attached to `target.exe` [4242]'));
      expect(d.message, contains('no heap activity observed'));
      expect(d.message, contains('segment heap'));
    });

    test('zero events, past window, no handshake yet: generic "Attached" label', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 0,
        symbolsResolved: 0,
        hexFallback: 0,
        nodeCount: 0,
        edgeCount: 0,
        pastNoEventsWindow: true,
        targetPid: null,
        targetName: null,
      );
      expect(d.status, TargetStatus.noEvents);
      expect(d.message, startsWith('Attached —'));
    });

    test('events flowing, healthy edge ratio: capturing, no message', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 40,
        hexFallback: 2,
        nodeCount: 100,
        edgeCount: 60,
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'ok.exe',
      );
      expect(d.status, TargetStatus.capturing);
      expect(d.message, isNull);
    });

    test('events flowing, nodes present, near-zero edges, symbols mostly resolved: noEdges', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 90,
        hexFallback: 2,
        nodeCount: 100,
        edgeCount: 1,
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'flat.exe',
      );
      expect(d.status, TargetStatus.noEdges);
      expect(d.message, contains('no ownership structure could be inferred'));
      expect(d.message, contains('Map view'));
      expect(d.message, isNot(contains('Symbols unavailable for this target.')));
    });

    test('events flowing, nodes present, near-zero edges, symbols predominantly hex: unsymbolized', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 2,
        hexFallback: 98,
        nodeCount: 100,
        edgeCount: 0,
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'stripped.exe',
      );
      expect(d.status, TargetStatus.unsymbolized);
      expect(d.message, contains('no ownership structure could be inferred'));
      expect(d.message, contains('Symbols unavailable for this target.'));
    });

    test('zero nodes with events flowing (e.g. all born+freed within a tick): capturing, not noEdges', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 10,
        hexFallback: 0,
        nodeCount: 0,
        edgeCount: 0,
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'churny.exe',
      );
      expect(d.status, TargetStatus.capturing, reason: 'noEdges requires nodes to actually be present');
    });

    test('edge ratio right at the threshold is still "no edges" (uses <, not <=, at the boundary below it)', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 90,
        hexFallback: 10,
        nodeCount: 100,
        edgeCount: 4, // ratio 0.04 < 0.05 threshold
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'x.exe',
      );
      expect(d.status, TargetStatus.noEdges);
    });

    test('edge ratio above the threshold is capturing, not noEdges', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 500,
        symbolsResolved: 90,
        hexFallback: 10,
        nodeCount: 100,
        edgeCount: 10, // ratio 0.10 >= 0.05 threshold
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'x.exe',
      );
      expect(d.status, TargetStatus.capturing);
    });

    test('raw counts are always carried through regardless of status', () {
      final d = TargetDiagnosis.classify(
        eventsReceived: 7,
        symbolsResolved: 3,
        hexFallback: 4,
        nodeCount: 5,
        edgeCount: 6,
        pastNoEventsWindow: true,
        targetPid: 1,
        targetName: 'x.exe',
      );
      expect(d.eventsReceived, 7);
      expect(d.symbolsResolved, 3);
      expect(d.hexFallback, 4);
      expect(d.nodeCount, 5);
      expect(d.edgeCount, 6);
    });
  });

  group('TargetDiagnosis.initial', () {
    test('is capturing with no message and all-zero counts', () {
      final d = TargetDiagnosis.initial();
      expect(d.status, TargetStatus.capturing);
      expect(d.message, isNull);
      expect(d.eventsReceived, 0);
      expect(d.nodeCount, 0);
    });
  });
}
