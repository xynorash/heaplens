import 'dart:async';
import 'dart:convert';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import '../models/graph_diff.dart';

/// Daemon WebSocket endpoint. Fixed for M5 — a later task may make this
/// configurable (e.g. a settings screen), but for now a single top-level
/// const is simplest.
const String kDaemonWsUrl = 'ws://127.0.0.1:9999';

/// Connection lifecycle status, exposed separately from the message stream
/// so a later task (the control bar) can watch "are we connected?" without
/// subscribing to (and re-rendering on) every graph message.
enum ConnectionStatus { connecting, connected, disconnected }

/// Current connection status. Updated by [graphMessageProvider]'s
/// underlying [GraphMessageConnection] as it connects/disconnects/retries.
final connectionStatusProvider =
    StateProvider<ConnectionStatus>((ref) => ConnectionStatus.disconnected);

/// Computes the delay before reconnect attempt number [attempt] (0-indexed:
/// the first retry after an initial connection failure/drop is attempt 0).
/// Exponential backoff from 500ms, doubling each attempt, capped at 5s.
///
/// Factored out as a pure, dependency-free function so it can be unit
/// tested without touching a socket or a timer.
Duration reconnectBackoff(int attempt) {
  const base = Duration(milliseconds: 500);
  const cap = Duration(milliseconds: 5000);
  // Clamp the shift amount so this can't overflow for pathologically large
  // attempt counts (a client that's been offline for a long time).
  final shift = attempt < 0 ? 0 : (attempt > 10 ? 10 : attempt);
  final scaled = base * (1 << shift);
  return scaled > cap ? cap : scaled;
}

/// A single opened connection's raw frame stream, plus a way to tear it
/// down. Kept as a small value type (rather than exposing [WebSocketChannel]
/// directly through the reconnect loop) so [GraphMessageConnection] can be
/// unit tested with a fake connector that never touches a real socket.
class WsFrames {
  const WsFrames(this.stream, this.close);

  /// Raw incoming frames (JSON text, per the daemon wire protocol).
  final Stream<dynamic> stream;

  /// Tears down the underlying connection, if any.
  final void Function() close;
}

/// Opens one connection attempt. Called again by [GraphMessageConnection]
/// each time it needs to (re)connect.
typedef WsConnector = WsFrames Function();

/// Default connector: opens a real WebSocket to [kDaemonWsUrl].
WsFrames _connectToDaemon() {
  final channel = WebSocketChannel.connect(Uri.parse(kDaemonWsUrl));
  return WsFrames(channel.stream, () => channel.sink.close());
}

/// Drives one WebSocket connection at a time, decoding incoming frames into
/// [GraphMessage]s, and reconnects with backoff whenever the connection
/// drops (via error or a clean `onDone`).
///
/// The daemon intentionally disconnects clients that fall behind on its
/// broadcast channel (a lag-based disconnect policy — see M4). A dropped
/// connection is therefore expected, ordinary behavior here, not a fatal
/// condition: this class must keep retrying rather than giving up after the
/// first failure. Because the daemon always sends a fresh `snapshot`
/// message immediately on any new connection, each reconnect is
/// self-healing — a downstream consumer (the `graph_provider` of a later
/// task) that clears and repopulates its state from `snapshot` messages
/// will end up correct after a reconnect cycle, not left with stale or
/// duplicated nodes.
///
/// This is a plain class (not itself a provider) specifically so it can be
/// unit tested with a fake [WsConnector] and a fake `backoff` function —
/// no real socket or real timers required.
class GraphMessageConnection {
  GraphMessageConnection({
    WsConnector? connector,
    Duration Function(int attempt)? backoff,
    required this.onStatus,
    required this.onMessage,
    required this.onError,
  })  : connector = connector ?? _connectToDaemon,
        backoff = backoff ?? reconnectBackoff;

  final WsConnector connector;
  final Duration Function(int attempt) backoff;
  final void Function(ConnectionStatus status) onStatus;
  final void Function(GraphMessage message) onMessage;
  final void Function(Object error, StackTrace stackTrace) onError;

  int _attempt = 0;
  bool _disposed = false;
  StreamSubscription<dynamic>? _sub;
  Timer? _retryTimer;
  void Function()? _closeCurrent;

  /// Begins the connect loop. Safe to call at most once per instance.
  void start() => _connect();

  void _connect() {
    if (_disposed) return;
    onStatus(ConnectionStatus.connecting);

    late WsFrames frames;
    try {
      frames = connector();
    } catch (e, st) {
      onError(e, st);
      _scheduleReconnect();
      return;
    }
    _closeCurrent = frames.close;

    _sub = frames.stream.listen(
      (raw) {
        // Any message, including the very next one after a reconnect,
        // means the connection is healthy again.
        _attempt = 0;
        onStatus(ConnectionStatus.connected);
        try {
          final decoded = jsonDecode(raw as String) as Map<String, dynamic>;
          onMessage(GraphMessage.fromJson(decoded));
        } catch (e, st) {
          onError(e, st);
        }
      },
      onError: (Object e, StackTrace st) {
        onError(e, st);
        _scheduleReconnect();
      },
      onDone: _scheduleReconnect,
      cancelOnError: true,
    );
  }

  void _scheduleReconnect() {
    if (_disposed) return;
    onStatus(ConnectionStatus.disconnected);
    final delay = backoff(_attempt);
    _attempt++;
    _retryTimer = Timer(delay, _connect);
  }

  /// Tears down the current connection (if any) and stops retrying.
  void dispose() {
    _disposed = true;
    _retryTimer?.cancel();
    _sub?.cancel();
    _closeCurrent?.call();
  }
}

/// Streams decoded [GraphMessage]s from the daemon, reconnecting with
/// backoff across drops (see [GraphMessageConnection] doc for why this
/// matters). Connection status is mirrored into [connectionStatusProvider]
/// as a side effect, so widgets that only care about connectivity don't
/// need to watch this (and rebuild on every message).
///
/// Implemented as a hand-rolled `StreamController`-backed provider (rather
/// than an `async*` generator) because the reconnect loop needs to survive
/// stream-internal errors/`onDone` without ever letting those propagate out
/// as a terminal event on the provider's stream — an `async*` loop would
/// naturally end the stream on the first disconnect, which is exactly the
/// behavior we must not have here.
final graphMessageProvider = StreamProvider<GraphMessage>((ref) {
  final controller = StreamController<GraphMessage>();

  final connection = GraphMessageConnection(
    onStatus: (status) =>
        ref.read(connectionStatusProvider.notifier).state = status,
    onMessage: controller.add,
    onError: controller.addError,
  );
  connection.start();

  ref.onDispose(() {
    connection.dispose();
    controller.close();
  });

  return controller.stream;
});
