import 'dart:math' as math;

import 'package:vector_math/vector_math.dart' show Vector2;

import '../models/node.dart';

/// Minimum/maximum on-screen radius (px) for a [SimNode]. See
/// [radiusForSize] for how a `NodeDto.size` maps into this range.
const double kMinNodeRadius = 5.0;
const double kMaxNodeRadius = 38.0;

/// Reference allocation size (bytes) that maps to [kMaxNodeRadius] in
/// [radiusForSize]'s log curve — 1 MiB. Not a hard ceiling: any size at or
/// above this still renders at [kMaxNodeRadius] (the curve clamps), it's
/// just the point past which growing further stops being visually
/// distinguishable, which is the right trade-off for a graph where
/// "there's a big one here" matters more than precisely how big.
const double kLargeSizeReference = 1024 * 1024;

/// Rest length (px) of the spring connecting an owner node to an owned node
/// along a graph edge.
const double kSpringRestLength = 80.0;

/// Spring stiffness applied along each ownership edge.
const double kSpringStrength = 0.06;

/// Pairwise repulsion coefficient (O(n^2) Coulomb-like force) that keeps
/// unrelated nodes apart.
const double kRepulsionStrength = 3000.0;

/// Gentle pull toward the canvas center so disconnected components don't
/// drift off-screen forever. Kept low deliberately — enough to hold the
/// graph on-screen, not enough to fight the collision pass and pile
/// disconnected roots back into a clump. See [_resolveCollisions] for the
/// hard no-overlap guarantee; gravity's only job is staying on-canvas.
const double kGravityStrength = 0.012;

/// Floor (px) on the repulsion gap (`distance - (r_i + r_j)`) used as the
/// denominator in [ForceLayout._applyForces]'s repulsion force. Without
/// this, two nodes whose *edges* are touching or overlapping (gap near
/// zero or negative) would produce a force approaching or crossing
/// infinity — this clamps the closest-range repulsion to a large but
/// finite push instead of a blowup, and lets [_resolveCollisions] (a hard
/// positional constraint, not a force) do the actual no-overlap work.
const double kMinRepulsionGap = 2.0;

/// Minimum gap (px) [_resolveCollisions] maintains between two drawn
/// circles' edges — nodes rest near but not touching.
const double kCollisionMargin = 3.0;

/// Side length (px) of the spatial-hash grid cell used by
/// [ForceLayout._resolveCollisions]. Sized to `2 * kMaxNodeRadius +
/// kCollisionMargin` so that any pair of nodes close enough to overlap
/// (their radii sum is at most `2 * kMaxNodeRadius`) always falls within
/// one cell of each other — checking a node's own cell plus its 8
/// neighbors is therefore guaranteed to find every real collision
/// candidate, without an O(n^2) all-pairs scan.
const double kCollisionCellSize =
    kMaxNodeRadius * 2 + kCollisionMargin;

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

/// ENF9 hysteresis: once aggregated, [ForceLayout] does not fall back to
/// individual mode until [ForceLayout.liveNodeCount] drops *below* this
/// (lower) threshold. This creates a dead zone between
/// [kAggregationExitThreshold] and [kAggregationThreshold] where neither
/// transition fires, so live-count churn oscillating near the boundary
/// (e.g. 498, 501, 499, 502...) doesn't repeatedly force a full
/// [ForceLayout._rebuildFromRaw] reset in each direction. Must stay
/// strictly below [kAggregationThreshold].
const int kAggregationExitThreshold = 450;

/// Half-width (px) of the random jitter box used when spawning a node: near
/// its owner (so it doesn't land exactly on top of it) or near canvas
/// center when there is no positioned owner.
const double kSpawnJitter = 20.0;

/// Radius (px) for a `NodeDto.size`, clamped to
/// [kMinNodeRadius]..[kMaxNodeRadius].
///
/// Log-scaled, not `sqrt`-scaled. This is a real fix, not a cosmetic
/// tweak: the allocation sizes this app actually renders span bytes to
/// low kilobytes (every workload used throughout this project's own
/// testing — 1 to a few hundred bytes), and `sqrt` barely moves for that
/// range (`sqrt(500) ≈ 22`, `sqrt(32) ≈ 6`) while a handful of larger
/// outliers blow straight through to the clamp — the result was almost
/// every real node pinned near [kMinNodeRadius] with occasional maxed-out
/// blobs, not a readable size gradient. `log2` compresses the *whole*
/// realistic byte-to-megabyte range into the same pixel budget evenly:
/// each doubling of size is the same visual step, so a 32-byte node, a
/// 500-byte node, and a 64KB node are all clearly, proportionately
/// distinguishable instead of nearly all clustering at the floor.
double radiusForSize(int size) {
  if (size <= 0) return kMinNodeRadius;
  final t = (math.log(size + 1) / math.ln2) / (math.log(kLargeSizeReference + 1) / math.ln2);
  return (kMinNodeRadius + (kMaxNodeRadius - kMinNodeRadius) * t)
      .clamp(kMinNodeRadius, kMaxNodeRadius);
}

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

  /// The node's last-known [NodeStateDto] at the moment [ForceLayout.removeNode]
  /// (or the aggregate-membership equivalent) started this node's fade-out.
  ///
  /// `null` for the whole life of a normal, non-fading [SimNode] — it is only
  /// populated at the instant a fade begins, from whatever `NodeDto` state
  /// was last known for this id (via [ForceLayout._rawNodes]) before that
  /// entry is deleted. This is what lets a painter render a fading node's
  /// color purely from [ForceLayout.simNodes] data, without needing a live
  /// `NodeDto` for the id — by the time the fade is visibly progressing, the
  /// backing `NodeDto` has typically already been removed from the graph
  /// provider's node map (see the design note on `GraphPainter._paintNodes`
  /// in graph_canvas.dart).
  NodeStateDto? lastKnownState;

  SimNode({
    required this.position,
    required this.velocity,
    required this.radius,
    this.fade = 1.0,
    this.lastKnownState,
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

  /// Bulk reconciliation for a full-state `GraphSnapshot` (initial connect,
  /// or a post-reconnect resync). Natural extension of the add/update/remove
  /// trio above — not a hard reset like [_rebuildFromRaw] (which only fires
  /// internally on an ENF9 aggregation-mode flip): existing tracked nodes
  /// that are still present keep their [SimNode] (position/velocity
  /// untouched, only `radius` refreshed via [updateNode]), so most nodes
  /// surviving a reconnect don't visually jump.
  ///
  /// [nodes] is the snapshot's full node list. [currentNodes] is used solely
  /// for owner lookups when spawning brand-new nodes (see [addNode]) — pass
  /// a map built from [nodes] itself (rather than, say, a possibly-stale
  /// external map) so owner lookups are self-contained within this one
  /// snapshot and don't depend on any other provider's state.
  void resetFrom(List<NodeDto> nodes, Map<int, NodeDto> currentNodes) {
    final newIds = nodes.map((n) => n.id).toSet();
    final staleIds =
        _rawNodes.keys.where((id) => !newIds.contains(id)).toList();
    for (final id in staleIds) {
      removeNode(id);
    }
    for (final node in nodes) {
      if (_rawNodes.containsKey(node.id)) {
        updateNode(node);
      } else {
        addNode(node, currentNodes);
      }
    }
  }

  /// Begins a fade-out for [id] (a diff `remove`, or a freed node). Does
  /// NOT delete the [SimNode] immediately — [step] advances the fade and
  /// deletes it once [kFadeDurationSeconds] of simulated time has elapsed.
  ///
  /// Captures the outgoing node's last-known [NodeDto.state] onto
  /// [SimNode.lastKnownState] *before* [_rawNodes] forgets it, so a painter
  /// can render the correct color throughout the fade using [simNodes]
  /// alone — see the doc on [SimNode.lastKnownState] for why this is needed.
  void removeNode(int id) {
    final outgoing = _rawNodes.remove(id);

    if (_aggregated) {
      _removeFromAggregate(id);
      return;
    }

    if (simNodes.containsKey(id) && !_fadeRemaining.containsKey(id)) {
      _fadeRemaining[id] = kFadeDurationSeconds;
      simNodes[id]!.lastKnownState = outgoing?.state ?? NodeStateDto.freed;
    }
  }

  /// Advances the physics simulation and fade timers by [dt] seconds, and
  /// re-checks the ENF9 aggregation threshold.
  void step(double dt) {
    _syncAggregation();
    _applyForces(dt);
    _resolveCollisions();
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
  /// Compares [liveNodeCount] against [kAggregationThreshold] /
  /// [kAggregationExitThreshold] (hysteresis) and flips [_aggregated] when
  /// it crosses the relevant boundary, rebuilding [simNodes] from
  /// [_rawNodes] under the new mode. Entering aggregated mode is still
  /// strictly-greater-than [kAggregationThreshold] (500 live nodes stays in
  /// individual mode, 501 switches to aggregated) — that entry threshold has
  /// not moved. Falling back to individual mode additionally requires
  /// dropping *below* [kAggregationExitThreshold] (450), not merely back to
  /// 500, so churn inside the [kAggregationExitThreshold]..[kAggregationThreshold]
  /// dead zone doesn't thrash between modes. Any future maintainer touching
  /// either threshold or comparison operator changes real on-screen behavior
  /// (individually-rendered nodes vs. one dot per symbol) — read ENF9 in the
  /// build spec before changing it.
  void _syncAggregation() {
    final count = liveNodeCount;
    final shouldAggregate = _aggregated
        ? count >= kAggregationExitThreshold
        : count > kAggregationThreshold;
    if (shouldAggregate == _aggregated) return;
    _aggregated = shouldAggregate;
    _rebuildFromRaw();
  }

  /// Rebuilds all simulation state from [_rawNodes] under the current
  /// [_aggregated] mode. This is a hard reset of positions/velocities for
  /// the mode being rebuilt into — an accepted trade-off since mode
  /// switches only happen at the aggregation threshold boundary, which is
  /// rare relative to normal add/update/remove traffic.
  ///
  /// Nodes that are currently mid-fade (tracked in [_fadeRemaining]) are the
  /// one exception: [removeNode] already deletes their raw id (or aggregate
  /// membership) the instant fade begins, so they have no representation in
  /// [_rawNodes] and would otherwise vanish from this rebuild instead of
  /// completing their ~1s fade-out. Their existing [SimNode] (position,
  /// velocity, fade progress) and remaining countdown are carried over
  /// untouched — they don't participate in physics/membership for the new
  /// mode, they just keep counting down via the normal [_advanceFades] path.
  void _rebuildFromRaw() {
    final fadingSimNodes = <int, SimNode>{
      for (final id in _fadeRemaining.keys)
        if (simNodes.containsKey(id)) id: simNodes[id]!,
    };
    final fadingRemaining = Map<int, double>.from(_fadeRemaining);
    // Avoid a freshly-assigned synthetic aggregate id colliding with a
    // preserved fading synthetic id from the mode being left.
    final preservedMinId = fadingSimNodes.keys.fold<int>(
      0,
      (min, id) => id < min ? id : min,
    );

    simNodes.clear();
    _fadeRemaining.clear();
    _symbolToAggId.clear();
    _aggMembers.clear();
    _nextSyntheticId = preservedMinId < 0 ? preservedMinId - 1 : -1;

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

    simNodes.addAll(fadingSimNodes);
    _fadeRemaining.addAll(fadingRemaining);
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
        // An aggregate bucket represents many raw nodes with potentially
        // different states; `freed` (gray) is the least-surprising fade
        // color for "this whole symbol-bucket emptied out".
        simNodes[ownerKey]!.lastKnownState = NodeStateDto.freed;
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

    // Pairwise repulsion, O(n^2). Radius-aware: computed from the gap
    // between drawn *edges* (distance - (r_i + r_j)), not center-to-center
    // distance. A point-distance model treats two big nodes whose circles
    // already overlap as "far enough" once their centers clear a fixed
    // distance — this is the root cause of the reported blob overlap.
    // Using the edge gap means big nodes push each other apart based on
    // their actual drawn size, and small nodes don't get placed inside a
    // large one just because their centers are far enough apart on paper.
    for (var i = 0; i < ids.length; i++) {
      final aId = ids[i];
      final aSim = simNodes[aId]!;
      final aPos = aSim.position;
      for (var j = i + 1; j < ids.length; j++) {
        final bId = ids[j];
        final bSim = simNodes[bId]!;
        final bPos = bSim.position;
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
        final gap = dist - (aSim.radius + bSim.radius);
        // Clamped against blowups: two big overlapping nodes have a small
        // or negative gap, which would otherwise send forceMag toward (or
        // past) infinity. kMinRepulsionGap floors the denominator instead
        // — a large but finite push. The hard "never actually overlap"
        // guarantee is `_resolveCollisions`, not this force.
        final effectiveGap = math.max(gap, kMinRepulsionGap);
        final forceMag = kRepulsionStrength / (effectiveGap * effectiveGap);
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

  /// Hard no-overlap guarantee, run once per [step] after force integration.
  ///
  /// [_applyForces]'s repulsion is a *soft* force — like any spring/repel
  /// system it can settle into equilibrium with circles still overlapping
  /// (this is exactly the reported "blob" bug: disconnected nodes have no
  /// spring pulling them apart, so soft repulsion alone can leave them
  /// resting edge-into-edge or worse). This pass directly displaces any
  /// pair of nodes whose drawn circles are closer than
  /// `r_i + r_j + kCollisionMargin`, splitting the correction by inverse
  /// radius (the larger node barely moves; the smaller one does most of
  /// the moving) so a big owner node isn't shoved around by a swarm of
  /// tiny children.
  ///
  /// Uses a spatial-hash grid rather than an O(n^2) all-pairs scan: cell
  /// size is `2 * kMaxNodeRadius + kCollisionMargin` (see
  /// [kCollisionCellSize]'s doc for why that size guarantees correctness),
  /// so only same-cell and the 8 neighboring cells are ever checked per
  /// node — O(n·k) where k is the local node density, not O(n^2). Stays
  /// smooth at the ~200 visible nodes this is sized for; the existing
  /// >500 aggregation threshold (ENF9) still caps individual-mode node
  /// count above that.
  void _resolveCollisions() {
    final ids = simNodes.keys.toList(growable: false);
    if (ids.length < 2) return;

    final grid = <int, List<int>>{};
    int cellIndex(double x, double y) {
      final cx = (x / kCollisionCellSize).floor();
      final cy = (y / kCollisionCellSize).floor();
      // Pack two cell coordinates into one int key. Offset by a large
      // constant first so negative cell coordinates (nodes can drift to
      // either side of the canvas origin) never collide with positive
      // ones after packing.
      const offset = 1 << 20;
      return (cx + offset) * (1 << 21) + (cy + offset);
    }

    for (final id in ids) {
      final pos = simNodes[id]!.position;
      grid.putIfAbsent(cellIndex(pos.x, pos.y), () => <int>[]).add(id);
    }

    for (final id in ids) {
      final sim = simNodes[id]!;
      final cx = (sim.position.x / kCollisionCellSize).floor();
      final cy = (sim.position.y / kCollisionCellSize).floor();
      for (var dx = -1; dx <= 1; dx++) {
        for (var dy = -1; dy <= 1; dy++) {
          const offset = 1 << 20;
          final key =
              (cx + dx + offset) * (1 << 21) + (cy + dy + offset);
          final bucket = grid[key];
          if (bucket == null) continue;
          for (final otherId in bucket) {
            // Process each unordered pair exactly once regardless of
            // which cell/neighbor-offset combination finds it first.
            if (otherId <= id) continue;
            _separatePair(id, otherId);
          }
        }
      }
    }
  }

  void _separatePair(int aId, int bId) {
    final a = simNodes[aId];
    final b = simNodes[bId];
    if (a == null || b == null) return;

    final delta = a.position - b.position;
    var dist = delta.length;
    final minDist = a.radius + b.radius + kCollisionMargin;
    if (dist >= minDist) return; // not overlapping, nothing to do

    Vector2 dir;
    if (dist < 0.0001) {
      // Exactly coincident: pick a deterministic-ish direction so both
      // still move apart rather than the correction being undefined.
      dir = Vector2(
        (_random.nextDouble() * 2 - 1) * 0.5 + 0.5,
        (_random.nextDouble() * 2 - 1) * 0.5,
      ).normalized();
      dist = 0.0001;
    } else {
      dir = delta / dist;
    }

    final overlap = minDist - dist;
    // Inverse-size weighting: a node's own share of the correction is
    // proportional to the *other* node's radius, so the bigger of the two
    // barely moves and the smaller one does most of the separating.
    final totalRadius = a.radius + b.radius;
    final aShare = totalRadius > 0 ? b.radius / totalRadius : 0.5;
    final bShare = totalRadius > 0 ? a.radius / totalRadius : 0.5;

    a.position = a.position + dir * (overlap * aShare);
    b.position = b.position - dir * (overlap * bShare);
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
