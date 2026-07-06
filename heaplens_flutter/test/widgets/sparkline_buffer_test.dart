import 'package:flutter_test/flutter_test.dart';
import 'package:heaplens_flutter/models/node.dart';
import 'package:heaplens_flutter/widgets/node_detail.dart';

NodeDto _node({required int id, required int ts, required int size}) {
  return NodeDto(
    id: id,
    ptr: 0,
    size: size,
    ts: ts,
    symbol: 'sym',
    live: true,
    state: NodeStateDto.healthy,
    edges: const [],
  );
}

void main() {
  test('records (ts, size) samples for a node', () {
    final buffer = SparklineBuffer();
    buffer.record(_node(id: 1, ts: 1, size: 10));
    buffer.record(_node(id: 1, ts: 2, size: 20));

    expect(buffer.nodeId, 1);
    expect(buffer.samples, [(ts: 1, size: 10), (ts: 2, size: 20)]);
  });

  test('deduplicates consecutive samples with an unchanged size', () {
    final buffer = SparklineBuffer();
    buffer.record(_node(id: 1, ts: 1, size: 10));
    buffer.record(_node(id: 1, ts: 2, size: 10));
    buffer.record(_node(id: 1, ts: 3, size: 20));

    expect(buffer.samples, [(ts: 1, size: 10), (ts: 3, size: 20)]);
  });

  test('resets and discards old samples when fed a different node id', () {
    final buffer = SparklineBuffer();
    buffer.record(_node(id: 1, ts: 1, size: 10));
    buffer.record(_node(id: 1, ts: 2, size: 20));
    expect(buffer.samples.length, 2);

    buffer.record(_node(id: 2, ts: 3, size: 999));

    expect(buffer.nodeId, 2);
    expect(buffer.samples, [(ts: 3, size: 999)]);
  });

  test('reset() clears samples and forgets the tracked node id', () {
    final buffer = SparklineBuffer();
    buffer.record(_node(id: 1, ts: 1, size: 10));

    buffer.reset();

    expect(buffer.nodeId, isNull);
    expect(buffer.samples, isEmpty);
  });

  test('stays bounded at kSparklineCapacity despite many updates', () {
    final buffer = SparklineBuffer();
    for (var i = 0; i < kSparklineCapacity * 3; i++) {
      // Distinct sizes each time so nothing gets deduplicated away.
      buffer.record(_node(id: 1, ts: i, size: i));
    }

    expect(buffer.samples.length, kSparklineCapacity);
    // Oldest entries were evicted: the buffer should hold the most recent
    // kSparklineCapacity samples, i.e. sizes from
    // (3*cap - cap) .. (3*cap - 1).
    final expectedFirstSize = kSparklineCapacity * 3 - kSparklineCapacity;
    expect(buffer.samples.first.size, expectedFirstSize);
    expect(buffer.samples.last.size, kSparklineCapacity * 3 - 1);
  });
}
