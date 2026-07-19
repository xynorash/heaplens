import 'package:flutter/foundation.dart';

/// A process the picker can offer as an attach target. Mirrors
/// `heaplens_protocol::control::ProcessInfo`.
@immutable
class ProcessInfo {
  final int pid;
  final String name;
  /// "x64", "x86", or "unknown" — the daemon reports "unknown" rather than
  /// dropping a process it couldn't query the architecture of (protected/
  /// elevated processes). Callers should treat "unknown" as not-attachable,
  /// same as "x86" — only "x64" is known-safe on this 64-bit build.
  final String arch;

  const ProcessInfo({required this.pid, required this.name, required this.arch});

  factory ProcessInfo.fromJson(Map<String, dynamic> json) {
    return ProcessInfo(
      pid: json['pid'] as int,
      name: json['name'] as String,
      arch: json['arch'] as String,
    );
  }
}

/// Client → daemon control requests. Mirrors
/// `heaplens_protocol::control::ControlRequest`.
///
/// A Dart 3 `sealed` class rather than `freezed` (forbidden for this
/// project, see `graph_diff.dart`'s identical note) — each variant knows how
/// to serialize itself via [toJson].
@immutable
sealed class ControlRequest {
  const ControlRequest();

  Map<String, dynamic> toJson();
}

final class ListProcessesRequest extends ControlRequest {
  const ListProcessesRequest();

  @override
  Map<String, dynamic> toJson() => {'type': 'list_processes'};
}

final class AttachTargetRequest extends ControlRequest {
  final int pid;

  const AttachTargetRequest(this.pid);

  @override
  Map<String, dynamic> toJson() => {'type': 'attach_target', 'pid': pid};
}

final class DetachTargetRequest extends ControlRequest {
  const DetachTargetRequest();

  @override
  Map<String, dynamic> toJson() => {'type': 'detach_target'};
}

/// Daemon → client control responses/pushes. Mirrors
/// `heaplens_protocol::control::ControlResponse`.
///
/// `ProcessList`/`AttachResult`/`DetachResult` are replies to a specific
/// [ControlRequest]. `TargetExited` is an unprompted push, broadcast the
/// moment the daemon detects the attached target process exited — the
/// control-channel counterpart to how graph diffs are pushed, not polled.
@immutable
sealed class ControlResponse {
  const ControlResponse();

  /// Parses a decoded JSON map, dispatching on `json['type']`.
  factory ControlResponse.fromJson(Map<String, dynamic> json) {
    final type = json['type'] as String;
    switch (type) {
      case 'process_list':
        return ProcessListResponse.fromJson(json);
      case 'attach_result':
        return AttachResultResponse.fromJson(json);
      case 'detach_result':
        return DetachResultResponse.fromJson(json);
      case 'target_exited':
        return TargetExitedResponse.fromJson(json);
      default:
        throw FormatException('ControlResponse.fromJson: unknown type "$type"');
    }
  }

  /// The wire `"type"` tag values this class recognizes — used by the WS
  /// layer to decide whether an incoming frame is a [ControlResponse]
  /// (route here) or a `GraphMessage` (route there) before attempting to
  /// parse it as either, since both shapes share one connection.
  static const List<String> wireTypes = [
    'process_list',
    'attach_result',
    'detach_result',
    'target_exited',
  ];
}

final class ProcessListResponse extends ControlResponse {
  final List<ProcessInfo> processes;

  const ProcessListResponse(this.processes);

  factory ProcessListResponse.fromJson(Map<String, dynamic> json) {
    return ProcessListResponse(
      (json['processes'] as List<dynamic>)
          .map((p) => ProcessInfo.fromJson(p as Map<String, dynamic>))
          .toList(),
    );
  }
}

final class AttachResultResponse extends ControlResponse {
  final bool ok;
  final String message;

  const AttachResultResponse({required this.ok, required this.message});

  factory AttachResultResponse.fromJson(Map<String, dynamic> json) {
    return AttachResultResponse(ok: json['ok'] as bool, message: json['message'] as String);
  }
}

final class DetachResultResponse extends ControlResponse {
  final bool ok;
  final String message;

  const DetachResultResponse({required this.ok, required this.message});

  factory DetachResultResponse.fromJson(Map<String, dynamic> json) {
    return DetachResultResponse(ok: json['ok'] as bool, message: json['message'] as String);
  }
}

final class TargetExitedResponse extends ControlResponse {
  final int pid;

  const TargetExitedResponse(this.pid);

  factory TargetExitedResponse.fromJson(Map<String, dynamic> json) {
    return TargetExitedResponse(json['pid'] as int);
  }
}
