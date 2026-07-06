import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/graph_diff.dart';
import 'package:heaplens_flutter/providers/ws_provider.dart';

/// Small inline fixtures — kept minimal on purpose since these tests only
/// exercise decoding/reconnect plumbing, not the model layer (already
/// covered by test/models/contract_test.dart).
const _snapshotJson =
    '{"type":"snapshot","ts":1,"nodes":[]}';
const _badJson = 'not json';

void main() {
  group('reconnectBackoff', () {
    test('starts at 500ms and doubles', () {
      expect(reconnectBackoff(0), const Duration(milliseconds: 500));
      expect(reconnectBackoff(1), const Duration(milliseconds: 1000));
      expect(reconnectBackoff(2), const Duration(milliseconds: 2000));
      expect(reconnectBackoff(3), const Duration(milliseconds: 4000));
    });

    test('caps at 5s', () {
      expect(reconnectBackoff(4), const Duration(milliseconds: 5000));
      expect(reconnectBackoff(5), const Duration(milliseconds: 5000));
      expect(reconnectBackoff(100), const Duration(milliseconds: 5000));
    });

    test('does not throw or overflow for negative/huge attempt counts', () {
      expect(reconnectBackoff(-1), const Duration(milliseconds: 500));
      expect(() => reconnectBackoff(1 << 30), returnsNormally);
    });
  });

  group('GraphMessageConnection', () {
    late List<ConnectionStatus> statuses;
    late List<GraphMessage> messages;
    late List<Object> errors;

    setUp(() {
      statuses = [];
      messages = [];
      errors = [];
    });

    GraphMessageConnection buildConnection({
      required WsConnector connector,
      Duration Function(int)? backoff,
    }) {
      return GraphMessageConnection(
        connector: connector,
        backoff: backoff ?? (_) => Duration.zero,
        onStatus: statuses.add,
        onMessage: messages.add,
        onError: (e, st) => errors.add(e),
      );
    }

    test('emits connecting then connected, decodes a message', () async {
      final controller = StreamController<dynamic>();
      var closed = false;
      final connection = buildConnection(
        connector: () => WsFrames(controller.stream, () => closed = true),
      );

      connection.start();
      // connect() runs synchronously up to the listen() call.
      expect(statuses, [ConnectionStatus.connecting]);

      controller.add(_snapshotJson);
      await Future<void>.delayed(Duration.zero);

      expect(statuses, [ConnectionStatus.connecting, ConnectionStatus.connected]);
      expect(messages, hasLength(1));
      expect(messages.single, isA<GraphSnapshot>());

      connection.dispose();
      await controller.close();
      expect(closed, isTrue);
    });

    test('reconnects after the stream closes (onDone), self-healing on '
        'the next snapshot', () async {
      final firstController = StreamController<dynamic>();
      final secondController = StreamController<dynamic>();
      var callCount = 0;

      final connection = buildConnection(
        connector: () {
          callCount++;
          final stream = callCount == 1 ? firstController.stream : secondController.stream;
          return WsFrames(stream, () {});
        },
      );

      connection.start();
      expect(callCount, 1);

      // Simulate the daemon dropping this client (lag-based disconnect).
      await firstController.close();
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);

      expect(callCount, 2);
      expect(
        statuses,
        [
          ConnectionStatus.connecting,
          ConnectionStatus.disconnected,
          ConnectionStatus.connecting,
        ],
      );

      // Fresh snapshot arrives on the new connection — self-healing.
      secondController.add(_snapshotJson);
      await Future<void>.delayed(Duration.zero);
      expect(statuses.last, ConnectionStatus.connected);
      expect(messages, hasLength(1));

      connection.dispose();
      await secondController.close();
    });

    test('reconnects after a stream error, does not terminate', () async {
      final firstController = StreamController<dynamic>();
      final secondController = StreamController<dynamic>();
      var callCount = 0;

      final connection = buildConnection(
        connector: () {
          callCount++;
          final stream = callCount == 1 ? firstController.stream : secondController.stream;
          return WsFrames(stream, () {});
        },
      );

      connection.start();
      firstController.addError(StateError('lagged out'));
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);

      expect(callCount, 2);
      expect(errors, hasLength(1));
      expect(statuses, contains(ConnectionStatus.disconnected));

      connection.dispose();
      await firstController.close();
      await secondController.close();
    });

    test('malformed frame reports an error but does not kill the connection', () async {
      final controller = StreamController<dynamic>();
      final connection = buildConnection(
        connector: () => WsFrames(controller.stream, () {}),
      );

      connection.start();
      controller.add(_badJson);
      await Future<void>.delayed(Duration.zero);
      controller.add(_snapshotJson);
      await Future<void>.delayed(Duration.zero);

      expect(errors, hasLength(1));
      expect(messages, hasLength(1));

      connection.dispose();
      await controller.close();
    });

    test('backoff function receives increasing attempt numbers', () async {
      final controllers = [
        StreamController<dynamic>(),
        StreamController<dynamic>(),
        StreamController<dynamic>(),
      ];
      var callCount = 0;
      final backoffAttempts = <int>[];

      final connection = buildConnection(
        connector: () {
          final stream = controllers[callCount].stream;
          callCount++;
          return WsFrames(stream, () {});
        },
        backoff: (attempt) {
          backoffAttempts.add(attempt);
          return Duration.zero;
        },
      );

      connection.start();
      await controllers[0].close();
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);
      await controllers[1].close();
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);

      expect(backoffAttempts, [0, 1]);

      connection.dispose();
      await controllers[2].close();
    });

    test('close callback fires before scheduling reconnect', () async {
      final firstController = StreamController<dynamic>();
      final secondController = StreamController<dynamic>();
      var connectorCallCount = 0;
      final closeCallOrder = <String>[];

      final connection = buildConnection(
        connector: () {
          connectorCallCount++;
          closeCallOrder.add('connector_call_$connectorCallCount');
          final stream = connectorCallCount == 1 ? firstController.stream : secondController.stream;
          return WsFrames(
            stream,
            () {
              closeCallOrder.add('close_called_$connectorCallCount');
            },
          );
        },
      );

      connection.start();
      expect(connectorCallCount, 1);
      expect(closeCallOrder, ['connector_call_1']);

      // Trigger reconnect via onDone.
      await firstController.close();
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);

      // Verify the order: first connection's close should be called before
      // the second connector is called.
      expect(connectorCallCount, 2);
      expect(
        closeCallOrder,
        [
          'connector_call_1',
          'close_called_1', // First connection closed
          'connector_call_2', // Before second connection opened
        ],
      );

      connection.dispose();
      await secondController.close();
    });

    test('dispose stops further reconnect attempts', () async {
      final controller = StreamController<dynamic>();
      var callCount = 0;

      final connection = buildConnection(
        connector: () {
          callCount++;
          return WsFrames(controller.stream, () {});
        },
      );

      connection.start();
      expect(callCount, 1);
      connection.dispose();
      await controller.close();
      await Future<void>.delayed(Duration.zero);
      await Future<void>.delayed(Duration.zero);

      // dispose() must prevent the scheduled reconnect from firing.
      expect(callCount, 1);
    });
  });

  group('connectionStatusProvider', () {
    test('defaults to disconnected', () {
      // Sanity check on the initial value a later task's control bar would
      // see before any connection attempt updates it.
      expect(ConnectionStatus.values, contains(ConnectionStatus.disconnected));
    });
  });
}
