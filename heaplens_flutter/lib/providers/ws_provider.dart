import 'dart:async';
import 'dart:convert';

import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

import '../models/control.dart';
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

/// No-op default for [WsFrames.send] — every call site written before
/// Stage 7 Step 4 constructs [WsFrames] with just (stream, close); this
/// keeps those constructions valid without a mass rewrite (a fake
/// connector in a test that never exercises sending has nothing meaningful
/// to send to anyway).
void _noopSend(String _) {}

/// A single opened connection's raw frame stream, a way to tear it down, and
/// a way to send an outbound frame. Kept as a small value type (rather than
/// exposing [WebSocketChannel] directly through the reconnect loop) so
/// [GraphMessageConnection] can be unit tested with a fake connector that
/// never touches a real socket.
class WsFrames {
  const WsFrames(this.stream, this.close, [this.send = _noopSend]);

  /// Raw incoming frames (JSON text, per the daemon wire protocol).
  final Stream<dynamic> stream;

  /// Tears down the underlying connection, if any.
  final void Function() close;

  /// Sends a raw outbound text frame (encoded JSON) — Stage 7 §3's control
  /// requests (`ListProcesses`/`AttachTarget`/`DetachTarget`).
  final void Function(String text) send;
}

/// Opens one connection attempt. Called again by [GraphMessageConnection]
/// each time it needs to (re)connect.
typedef WsConnector = WsFrames Function();

/// Default connector: opens a real WebSocket to [kDaemonWsUrl].
WsFrames _connectToDaemon() {
  final channel = WebSocketChannel.connect(Uri.parse(kDaemonWsUrl));
  return WsFrames(
    channel.stream,
    () => channel.sink.close(),
    (text) => channel.sink.add(text),
  );
}

/// Drives one WebSocket connection at a time, decoding incoming frames into
/// [GraphMessage]s (via [onMessage]) or [ControlResponse]s (via
/// [onControlMessage] — Stage 7 §3/§4.4: process-list/attach/detach replies
/// and the unprompted `TargetExited` push), and reconnects with backoff
/// whenever the connection drops (via error or a clean `onDone`).
///
/// Both message shapes share one WS connection and are distinguished by
/// their wire `"type"` tag *before* attempting to parse either — checking
/// the tag against [ControlResponse.wireTypes] first is unambiguous by
/// construction, unlike a try-`GraphMessage`-then-fall-back-to-`ControlResponse`
/// approach, which would work today only by accident.
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
    this.onControlMessage,
  })  : connector = connector ?? _connectToDaemon,
        backoff = backoff ?? reconnectBackoff;

  final WsConnector connector;
  final Duration Function(int attempt) backoff;
  final void Function(ConnectionStatus status) onStatus;
  final void Function(GraphMessage message) onMessage;
  final void Function(Object error, StackTrace stackTrace) onError;
  final void Function(ControlResponse message)? onControlMessage;

  int _attempt = 0;
  bool _disposed = false;
  StreamSubscription<dynamic>? _sub;
  Timer? _retryTimer;
  void Function()? _closeCurrent;
  void Function(String text)? _sendCurrent;

  /// Begins the connect loop. Safe to call at most once per instance.
  void start() => _connect();

  /// Sends one control request, if currently connected. Silently dropped
  /// while disconnected/reconnecting — there is no queueing. A user action
  /// taken during a reconnect window has nothing live to reach yet; the
  /// picker UI surfaces [connectionStatusProvider] so this should be rare
  /// in practice, not a silent black hole in the common case.
  void sendRequest(ControlRequest request) {
    final send = _sendCurrent;
    if (send == null) return;
    send(jsonEncode(request.toJson()));
  }

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
    _sendCurrent = frames.send;

    _sub = frames.stream.listen(
      (raw) {
        // Any message, including the very next one after a reconnect,
        // means the connection is healthy again.
        _attempt = 0;
        onStatus(ConnectionStatus.connected);
        try {
          final decoded = jsonDecode(raw as String) as Map<String, dynamic>;
          final type = decoded['type'] as String?;
          if (type != null && ControlResponse.wireTypes.contains(type)) {
            onControlMessage?.call(ControlResponse.fromJson(decoded));
          } else {
            onMessage(GraphMessage.fromJson(decoded));
          }
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
    _closeCurrent?.call();
    _sendCurrent = null;
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

/// Bundles the single shared [GraphMessageConnection] with the two broadcast
/// controllers it feeds — kept together so `graphMessageProvider` and
/// `controlResponseProvider` can each derive their stream from the *same*
/// connection/controllers instead of each owning an independent WS
/// connection (which would double the daemon's connection count per Flutter
/// client and the reconnect/backoff state to keep in sync, for no benefit).
class _WsBundle {
  _WsBundle(this.connection, this.graphController, this.controlController);

  final GraphMessageConnection connection;
  final StreamController<GraphMessage> graphController;
  final StreamController<ControlResponse> controlController;
}

final _wsBundleProvider = Provider<_WsBundle>((ref) {
  final graphController = StreamController<GraphMessage>.broadcast();
  final controlController = StreamController<ControlResponse>.broadcast();

  final connection = GraphMessageConnection(
    onStatus: (status) =>
        ref.read(connectionStatusProvider.notifier).state = status,
    onMessage: graphController.add,
    onControlMessage: controlController.add,
    onError: (e, st) {
      graphController.addError(e, st);
      controlController.addError(e, st);
    },
  );
  // Deferred to a microtask: `connection.start()` synchronously calls
  // `onStatus` (writing to `connectionStatusProvider`) before this
  // provider's own build function would otherwise have returned. Riverpod
  // forbids a provider modifying another provider's state while it is
  // still building ("Providers are not allowed to modify other providers
  // during their initialization") and throws in debug mode if this
  // happens — this only surfaces with the real connector (every existing
  // test overrides `graphMessageProvider` with a fake stream, bypassing
  // this code path entirely), so it was only caught by a live run against
  // the real daemon.
  Future.microtask(connection.start);

  ref.onDispose(() {
    connection.dispose();
    graphController.close();
    controlController.close();
  });

  return _WsBundle(connection, graphController, controlController);
});

/// Streams decoded [GraphMessage]s from the daemon. See
/// [GraphMessageConnection]'s doc for the reconnect/self-healing contract
/// this relies on.
final graphMessageProvider = StreamProvider<GraphMessage>((ref) {
  return ref.watch(_wsBundleProvider).graphController.stream;
});

/// Streams decoded [ControlResponse]s from the daemon — replies to
/// [ControlRequest]s sent via `ref.read(wsConnectionProvider).sendRequest`,
/// plus the unprompted `TargetExited` push (§4.4).
final controlResponseProvider = StreamProvider<ControlResponse>((ref) {
  return ref.watch(_wsBundleProvider).controlController.stream;
});

/// The live connection, for widgets that need to actually send a control
/// request (the process picker's `ListProcesses`/`AttachTarget`/
/// `DetachTarget`) rather than just observe incoming messages.
final wsConnectionProvider = Provider<GraphMessageConnection>((ref) {
  return ref.watch(_wsBundleProvider).connection;
});
