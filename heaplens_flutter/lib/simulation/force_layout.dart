import 'dart:math' as math;

import 'package:vector_math/vector_math.dart' show Vector2;

import '../models/node.dart';

/// Minimum/maximum on-screen radius (px) for a [SimNode], derived from
/// `sqrt(size)` and clamped into this range so both tiny and huge
/// allocations stay legible on the canvas.
const double kMinNodeRadius = 4.0;
const double kMaxNodeRadius = 40.0;

/// Rest length (px) of the spring connecting an owner node to an owned node
/// along a graph edge.
const double kSpringRestLength = 80.0;

/// Spring stiffness applied along each ownership edge.
const double kSpringStrength = 0.06;

/// Pairwise repulsion coefficient (O(n^2) Coulomb-like force) that keeps
/// unrelated nodes apart.
const double kRepulsionStrength = 3000.0;

/// Gentle pull toward the canvas center so disconnected components don't
/// drift off-screen forever.
const double kGravityStrength = 0.02;

/// Per-step velocity damping factor, applied multiplicatively every
/// [ForceLayout.step].
const double kVelocityDamping = 0.85;

/// Wall-clock seconds a freed/removed node's [SimNode] spends fading out
/// ([SimNode.fade] descending 1.0 -> 0.0) before it is deleted from
/// [ForceLayout.simNodes]. This countdown is advanced only by
/// [ForceLayout.step] calls (an internal simulation clock), never by real
/// wall-clock time or by when the remove event itself arrived.
const double kFadeDurationSeconds = 1.0;

/// ENF9: once the number of *live* nodes known to the layout exceeds this,
/// [ForceLayout] switches from one [SimNode] per node to one aggregate
/// [SimNode] per distinct `symbol` (summed size, tracked count). See the
/// switch point in [ForceLayout._syncAggregation] — search that method
/// when touching aggregation behavior. Must activate strictly above the
/// threshold, not at or below it.
const int kAggregationThreshold = 500;

/// Half-width (px) of the random jitter box used when spawning a node: near
/// its owner (so it doesn't land exactly on top of it) or near canvas
/// center when there is no positioned owner.
const double kSpawnJitter = 20.0;

/// Radius (px) for a `NodeDto.size`, clamped to [kMinNodeRadius]..[kMaxNodeRadius].
double radiusForSize(int size) =>
    math.sqrt(size.toDouble()).clamp(kMinNodeRadius, kMaxNodeRadius);

/// Physical state of one node (or, once aggregated, one symbol-bucket) on
/// the force-directed canvas.
///
/// Deliberately minimal per the locked design (Q4): position/velocity/
/// radius/fade only — no id, no symbol, no edge list. [ForceLayout] keeps
/// all of that bookkeeping in side maps keyed by the same id used as the
/// key into [ForceLayout.simNodes], so this struct stays a pure physics
/// value type.
class SimNode {
  Vector2 position;
  Vector2 velocity;
  double radius;

  /// 1.0 = fully visible. Stays at 1.0 for the node's whole life; once the
  /// node is removed/freed (see [ForceLayout.removeNode]) it counts down to
  /// 0.0 over [kFadeDurationSeconds] of simulated time (driven by
  /// [ForceLayout.step], not wall-clock time), and the [SimNode] is deleted
  /// the instant it reaches 0.
  double fade;

  SimNode({
    required this.position,
    required this.velocity,
    required this.radius,
    this.fade = 1.0,
  });
}

/// Pure-Dart (no `dart:ui`, no widget imports, no networking) force-directed
/// layout engine for the ownership graph.
///
/// This module does not watch `graph_provider.dart` itself. A later task
/// (main.dart / canvas wiring) reads `graphProvider`'s revision, diffs the
/// previous vs. current node-id sets, and drives this class through the
/// API below. That caller owns the render loop's `Ticker`/frame callback;
/// [ForceLayout] only advances its internal clock when [step] is called.
///
/// Public API for that caller:
///   - [addNode]: a brand-new `NodeDto` appeared (snapshot, or a diff's
///     `add` list). Spawns a [SimNode] near its owner's current position
///     (owner = whichever *other* node in [currentNodes] lists this node's
///     id in its own `edges`), or near canvas center with jitter if no
///     owner is found / the owner has no [SimNode] yet.
///   - [updateNode]: an existing node's `NodeDto` changed (e.g. `size`).
///     Touches [SimNode.radius] only — never position/velocity.
///   - [removeNode]: node was removed (diff `remove`) or freed. Starts a
///     fade-out; the `SimNode` is deleted once the fade completes on a
///     later [step] call, not synchronously.
///   - [step]: advance the physics simulation and fade timers by `dt`
///     seconds, and re-check the aggregation threshold (ENF9).
class ForceLayout {
  /// Canvas-space center used for owner-less spawns and the gravity well.
  final double centerX;
  final double centerY;

  final math.Random _random;

  /// The live simulation state, keyed by node id when not aggregated, or by
  /// a synthetic (negative) per-symbol id once [isAggregated] is true.
  final Map<int, SimNode> simNodes = <int, SimNode>{};

  /// Last-known `NodeDto` per real node id. Maintained regardless of
  /// aggregation mode so that switching modes (or recomputing an
  /// aggregate's summed size) can be done from authoritative data instead
  /// of derived simulation state.
  final Map<int, NodeDto> _rawNodes = <int, NodeDto>{};

  /// Seconds remaining before deletion, keyed the same way as [simNodes].
  /// Presence in this map is what "fading out" means; absence means fully
  /// alive (or already deleted).
  final Map<int, double> _fadeRemaining = <int, double>{};

  bool _aggregated = false;

  /// symbol -> synthetic aggregate id (only populated while [_aggregated]).
  final Map<String, int> _symbolToAggId = <String, int>{};

  /// synthetic aggregate id -> raw node ids it currently represents.
  final Map<int, Set<int>> _aggMembers = <int, Set<int>>{};

  int _nextSyntheticId = -1;

  ForceLayout({
    this.centerX = 400.0,
    this.centerY = 300.0,
    math.Random? random,
  }) : _random = random ?? math.Random();

  /// Whether the layout is currently in aggregated (per-symbol) mode. Exposed
  /// mainly for tests / diagnostics.
  bool get isAggregated => _aggregated;

  /// Number of nodes flagged `live` in the last-known `NodeDto` for each id
  /// currently tracked by the layout. This is what [kAggregationThreshold]
  /// is compared against.
  int get liveNodeCount => _rawNodes.values.where((n) => n.live).length;

  // ---------------------------------------------------------------------
  // Public mutation API
  // ---------------------------------------------------------------------

  /// Registers a newly-added node and spawns its [SimNode].
  ///
  /// [currentNodes] is the full current node map (e.g. from
  /// `graphProvider`'s notifier) used solely to find [node]'s owner: the
  /// other node whose `edges` list contains `node.id`. Spawn lands near
  /// that owner's current position (with jitter) if the owner exists and
  /// already has a [SimNode]; otherwise it spawns near canvas center (with
  /// jitter).
  void addNode(NodeDto node, Map<int, NodeDto> currentNodes) {
    _rawNodes[node.id] = node;
    _fadeRemaining.remove(node.id); // defensive: re-added after a fade start

    if (_aggregated) {
      _addToAggregate(node);
      return;
    }

    final ownerId = _findOwnerId(node.id, currentNodes);
    simNodes[node.id] = SimNode(
      position: _spawnPosition(ownerId),
      velocity: Vector2.zero(),
      radius: radiusForSize(node.size),
    );
  }

  /// Applies a field update (e.g. a `size` change) for a node the layout
  /// already knows about. Only ever touches [SimNode.radius] — position and
  /// velocity are left completely alone so the node doesn't visually jump.
  ///
  /// If the layout doesn't have a [SimNode] for this id yet (e.g. the
  /// update raced ahead of the add, or the node was previously folded into
  /// an aggregate and this is its first update after falling back below
  /// the threshold), it is spawned fresh via the same owner-less path as
  /// [addNode] — this still only *creates* a new position, never mutates an
  /// existing one.
  void updateNode(NodeDto node) {
    _rawNodes[node.id] = node;

    if (_aggregated) {
      // Aggregate radius/size is recomputed wholesale from _rawNodes the
      // next time membership changes or a mode switch rebuild happens.
      final key = _symbolToAggId[node.symbol];
      if (key != null) _recomputeAggregateRadius(key);
      return;
    }

    final sim = simNodes[node.id];
    if (sim == null) {
      simNodes[node.id] = SimNode(
        position: _spawnPosition(null),
        velocity: Vector2.zero(),
        radius: radiusForSize(node.size),
      );
      return;
    }
    sim.radius = radiusForSize(node.size);
  }

  /// Begins a fade-out for [id] (a diff `remove`, or a freed node). Does
  /// NOT delete the [SimNode] immediately — [step] advances the fade and
  /// deletes it once [kFadeDurationSeconds] of simulated time has elapsed.
  void removeNode(int id) {
    _rawNodes.remove(id);

    if (_aggregated) {
      _removeFromAggregate(id);
      return;
    }

    if (simNodes.containsKey(id) && !_fadeRemaining.containsKey(id)) {
      _fadeRemaining[id] = kFadeDurationSeconds;
    }
  }

  /// Advances the physics simulation and fade timers by [dt] seconds, and
  /// re-checks the ENF9 aggregation threshold.
  void step(double dt) {
    _syncAggregation();
    _applyForces(dt);
    _advanceFades(dt);
  }

  // ---------------------------------------------------------------------
  // Owner lookup / spawn positioning
  // ---------------------------------------------------------------------

  int? _findOwnerId(int nodeId, Map<int, NodeDto> currentNodes) {
    for (final entry in currentNodes.entries) {
      if (entry.value.edges.contains(nodeId)) return entry.key;
    }
    return null;
  }

  Vector2 _spawnPosition(int? ownerId) {
    final ownerSim = ownerId == null ? null : simNodes[ownerId];
    final base = ownerSim?.position ?? Vector2(centerX, centerY);
    return base +
        Vector2(
          (_random.nextDouble() * 2 - 1) * kSpawnJitter,
          (_random.nextDouble() * 2 - 1) * kSpawnJitter,
        );
  }

  // ---------------------------------------------------------------------
  // Aggregation (ENF9)
  // ---------------------------------------------------------------------

  /// *** ENF9 switch point ***
  /// Compares [liveNodeCount] against [kAggregationThreshold] and flips
  /// [_aggregated] when it crosses the boundary, rebuilding [simNodes] from
  /// [_rawNodes] under the new mode. Strictly-greater-than: 500 live nodes
  /// stays in individual mode, 501 switches to aggregated. Any future
  /// maintainer touching this threshold or its comparison operator changes
  /// real on-screen behavior (500 individually-rendered nodes vs. one dot
  /// per symbol) — read ENF9 in the build spec before changing it.
  void _syncAggregation() {
    final shouldAggregate = liveNodeCount > kAggregationThreshold;
    if (shouldAggregate == _aggregated) return;
    _aggregated = shouldAggregate;
    _rebuildFromRaw();
  }

  /// Rebuilds all simulation state from [_rawNodes] under the current
  /// [_aggregated] mode. This is a hard reset of positions/velocities/fades
  /// for the mode being rebuilt into — an accepted trade-off since mode
  /// switches only happen at the 500-node threshold boundary, which is
  /// rare relative to normal add/update/remove traffic.
  void _rebuildFromRaw() {
    simNodes.clear();
    _fadeRemaining.clear();
    _symbolToAggId.clear();
    _aggMembers.clear();
    _nextSyntheticId = -1;

    if (_aggregated) {
      for (final node in _rawNodes.values) {
        _addToAggregate(node);
      }
    } else {
      for (final node in _rawNodes.values) {
        final ownerId = _findOwnerId(node.id, _rawNodes);
        simNodes[node.id] = SimNode(
          position: _spawnPosition(ownerId),
          velocity: Vector2.zero(),
          radius: radiusForSize(node.size),
        );
      }
    }
  }

  void _addToAggregate(NodeDto node) {
    final key = _symbolToAggId.putIfAbsent(node.symbol, () {
      final id = _nextSyntheticId--;
      simNodes[id] = SimNode(
        position: _spawnPosition(null),
        velocity: Vector2.zero(),
        radius: kMinNodeRadius,
      );
      _aggMembers[id] = <int>{};
      return id;
    });
    _aggMembers[key]!.add(node.id);
    _recomputeAggregateRadius(key);
  }

  void _recomputeAggregateRadius(int key) {
    final members = _aggMembers[key];
    final sim = simNodes[key];
    if (members == null || sim == null) return;
    final totalSize = members.fold<int>(
      0,
      (sum, id) => sum + (_rawNodes[id]?.size ?? 0),
    );
    sim.radius = radiusForSize(totalSize);
  }

  /// Number of raw node ids currently folded into the aggregate for [key].
  /// Exposed for tests/diagnostics; not part of [SimNode] itself since the
  /// locked struct has no count field.
  int aggregateCount(int key) => _aggMembers[key]?.length ?? 0;

  void _removeFromAggregate(int id) {
    int? ownerKey;
    for (final entry in _aggMembers.entries) {
      if (entry.value.remove(id)) {
        ownerKey = entry.key;
        break;
      }
    }
    if (ownerKey == null) return;

    final members = _aggMembers[ownerKey]!;
    if (members.isEmpty) {
      _aggMembers.remove(ownerKey);
      _symbolToAggId.removeWhere((_, key) => key == ownerKey);
      if (simNodes.containsKey(ownerKey) &&
          !_fadeRemaining.containsKey(ownerKey)) {
        _fadeRemaining[ownerKey] = kFadeDurationSeconds;
      }
    } else {
      _recomputeAggregateRadius(ownerKey);
    }
  }

  // ---------------------------------------------------------------------
  // Physics
  // ---------------------------------------------------------------------

  void _applyForces(double dt) {
    final ids = simNodes.keys.toList(growable: false);
    if (ids.isEmpty) return;

    final forces = <int, Vector2>{for (final id in ids) id: Vector2.zero()};

    // Pairwise repulsion, O(n^2).
    for (var i = 0; i < ids.length; i++) {
      final aId = ids[i];
      final aPos = simNodes[aId]!.position;
      for (var j = i + 1; j < ids.length; j++) {
        final bId = ids[j];
        final bPos = simNodes[bId]!.position;
        var delta = aPos - bPos;
        var distSq = delta.length2;
        if (distSq < 0.0001) {
          // Coincident points: nudge apart deterministically-ish so the
          // force doesn't divide by ~0.
          delta = Vector2(
            (_random.nextDouble() * 2 - 1) * 0.01 + 0.01,
            (_random.nextDouble() * 2 - 1) * 0.01,
          );
          distSq = delta.length2;
        }
        final dist = math.sqrt(distSq);
        final dir = delta / dist;
        final forceMag = kRepulsionStrength / distSq;
        forces[aId] = forces[aId]! + dir * forceMag;
        forces[bId] = forces[bId]! + dir * -forceMag;
      }
    }

    // Spring attraction along ownership edges (individual mode only —
    // aggregates don't carry a meaningful edge list).
    if (!_aggregated) {
      for (final id in ids) {
        final node = _rawNodes[id];
        if (node == null) continue;
        final self = simNodes[id];
        if (self == null) continue;
        for (final targetId in node.edges) {
          final target = simNodes[targetId];
          if (target == null) continue;
          final delta = target.position - self.position;
          final dist = math.max(delta.length, 0.0001);
          final dir = delta / dist;
          final stretch = dist - kSpringRestLength;
          final force = dir * (kSpringStrength * stretch);
          forces[id] = forces[id]! + force;
          forces[targetId] = forces[targetId]! - force;
        }
      }
    }

    // Gentle gravity to center.
    final center = Vector2(centerX, centerY);
    for (final id in ids) {
      final self = simNodes[id]!;
      forces[id] = forces[id]! + (center - self.position) * kGravityStrength;
    }

    // Integrate: semi-implicit Euler with velocity damping.
    for (final id in ids) {
      final sim = simNodes[id]!;
      sim.velocity = (sim.velocity + forces[id]! * dt) * kVelocityDamping;
      sim.position = sim.position + sim.velocity * dt;
    }
  }

  void _advanceFades(double dt) {
    if (_fadeRemaining.isEmpty) return;
    final toDelete = <int>[];
    _fadeRemaining.updateAll((id, remaining) {
      final next = remaining - dt;
      final sim = simNodes[id];
      if (sim != null) {
        sim.fade = (next / kFadeDurationSeconds).clamp(0.0, 1.0);
      }
      if (next <= 0) toDelete.add(id);
      return next;
    });
    for (final id in toDelete) {
      simNodes.remove(id);
      _fadeRemaining.remove(id);
    }
  }
}
