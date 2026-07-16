// Contract test for the Stage 7 Step 4 control-channel models, mirroring
// contract_test.dart's style. Wire contract source of truth:
// crates/heaplens-protocol/src/control.rs.
import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/control.dart';

void main() {
  group('ProcessInfo', () {
    test('parses from JSON', () {
      final p = ProcessInfo.fromJson({'pid': 1234, 'name': 'target.exe', 'arch': 'x64'});
      expect(p.pid, 1234);
      expect(p.name, 'target.exe');
      expect(p.arch, 'x64');
    });
  });

  group('ControlRequest.toJson', () {
    test('ListProcessesRequest', () {
      expect(const ListProcessesRequest().toJson(), {'type': 'list_processes'});
    });

    test('AttachTargetRequest carries pid', () {
      expect(const AttachTargetRequest(4242).toJson(), {'type': 'attach_target', 'pid': 4242});
    });

    test('DetachTargetRequest', () {
      expect(const DetachTargetRequest().toJson(), {'type': 'detach_target'});
    });
  });

  group('ControlResponse.fromJson', () {
    test('process_list', () {
      final resp = ControlResponse.fromJson({
        'type': 'process_list',
        'processes': [
          {'pid': 1, 'name': 'a.exe', 'arch': 'x64'},
          {'pid': 2, 'name': 'b.exe', 'arch': 'x86'},
        ],
      });
      expect(resp, isA<ProcessListResponse>());
      final list = (resp as ProcessListResponse).processes;
      expect(list, hasLength(2));
      expect(list[0].pid, 1);
      expect(list[1].arch, 'x86');
    });

    test('attach_result ok', () {
      final resp = ControlResponse.fromJson({'type': 'attach_result', 'ok': true, 'message': 'attached to pid 4242'});
      expect(resp, isA<AttachResultResponse>());
      final r = resp as AttachResultResponse;
      expect(r.ok, isTrue);
      expect(r.message, 'attached to pid 4242');
    });

    test('attach_result failure carries the daemon message verbatim', () {
      final resp = ControlResponse.fromJson({
        'type': 'attach_result',
        'ok': false,
        'message': 'cannot open process 999999999 — access denied, or the process does not exist.',
      });
      final r = resp as AttachResultResponse;
      expect(r.ok, isFalse);
      expect(r.message, contains('access denied'));
    });

    test('detach_result', () {
      final resp = ControlResponse.fromJson({'type': 'detach_result', 'ok': true, 'message': 'detached'});
      expect(resp, isA<DetachResultResponse>());
      expect((resp as DetachResultResponse).ok, isTrue);
    });

    test('target_exited carries the pid', () {
      final resp = ControlResponse.fromJson({'type': 'target_exited', 'pid': 777});
      expect(resp, isA<TargetExitedResponse>());
      expect((resp as TargetExitedResponse).pid, 777);
    });

    test('unknown type throws FormatException rather than silently misparsing', () {
      expect(
        () => ControlResponse.fromJson({'type': 'something_new'}),
        throwsA(isA<FormatException>()),
      );
    });
  });

  group('ControlResponse.wireTypes', () {
    test('lists exactly the four recognized tags', () {
      expect(
        ControlResponse.wireTypes,
        containsAll(['process_list', 'attach_result', 'detach_result', 'target_exited']),
      );
      expect(ControlResponse.wireTypes, hasLength(4));
    });

    test('does not overlap with GraphMessage tags', () {
      // Regression guard for the dispatch-before-parse design in
      // ws_provider.dart: if this ever collided with "snapshot"/"diff", the
      // "check wireTypes first" routing would silently misroute graph
      // messages to the control parser.
      expect(ControlResponse.wireTypes, isNot(contains('snapshot')));
      expect(ControlResponse.wireTypes, isNot(contains('diff')));
    });
  });
}
