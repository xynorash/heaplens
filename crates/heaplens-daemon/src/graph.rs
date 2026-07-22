use std::collections::{HashMap, HashSet};

use heaplens_protocol::{AllocEvent, EventKind, GraphMessage, NodeDto, NodeState};

use crate::resolver::Resolver;

pub struct Node {
    pub id: u64,
    pub ptr: u64,
    pub size: u64,
    pub ts: u64,
    pub live: bool,
    pub stack: [u64; 16],
    pub stack_len: u8,
    /// Id of the node that owns this one (φ inference result).
    pub owner: Option<u64>,
    /// Ids of nodes this node owns.
    pub edges_out: Vec<u64>,
    /// True once an owner was assigned and then freed — used by M4 orphan detection.
    pub had_owner_once: bool,
    /// Current anomaly classification; updated by anomaly::sweep.
    pub state: NodeState,
    /// `ts_nanos` of the dealloc event that freed this node's owner, if any —
    /// set once in `on_dealloc` when the owner is freed and this node is
    /// orphaned. Observability-only: never read by `infer_ownership` or
    /// `anomaly::sweep`'s state predicates, so it cannot influence detection
    /// timing or outcome. Exists so H1 (detection-latency measurement) has a
    /// real owner-free timestamp to measure from instead of inferring one.
    pub owner_free_ts: Option<u64>,
    /// This node's own classification in `site_index`/`pending_site_ids`
    /// (2026-07-22 processing-ceiling fix) — computed once at insertion by
    /// `classify_effective_site` and otherwise stable (see `SiteClass`'s doc
    /// comment). Stored on the node so eviction (`drain_diff`) knows which
    /// index bucket to clean up without recomputing anything.
    pub site_class: SiteClass,
}

/// A node's own effective-site classification for `OwnershipGraph::site_index`
/// / `pending_site_ids` — a stability-aware variant of what
/// `OwnershipGraph::effective_site_name` computes fresh on every call.
///
/// `effective_site_index`'s skip rule treats "unknown to the resolver" and
/// "known machinery" identically (both get skipped past) — which is correct
/// for a fresh, uncached recomputation, but wrong to bake into a persistent
/// index: an address that's merely *unresolved* today could resolve to real,
/// non-machinery code tomorrow, which would change — possibly to an *earlier*
/// stack position than whatever this scan currently lands on — the node's
/// true effective site. Indexing that node under today's (possibly
/// premature) answer would silently miss it as a future φ candidate once
/// resolution catches up. `SiteClass` exists to tell "this answer can never
/// change" (`Resolved`/`NoSite`, both built entirely from addresses the
/// resolver already has a definite answer for) apart from "this answer is
/// provisional" (`Pending`, hit an address the resolver hasn't seen yet) —
/// only `Pending` nodes need re-checking, in `infer_ownership`, as new
/// symbols arrive.
#[derive(Clone, Debug, PartialEq)]
pub enum SiteClass {
    /// A real (non-machinery) effective site was found, and nothing skipped
    /// on the way to it was merely unresolved — this name is final.
    Resolved(String),
    /// Every address considered (up to `stack_len`) is either `0` or known
    /// machinery — this node structurally has no effective site, and that
    /// can never change (mirrors `effective_site_index` returning `None`
    /// when every address is *definitively* classified).
    NoSite,
    /// At least one address considered before a `Resolved`/`NoSite`
    /// conclusion could be reached is still unknown to the resolver. Not
    /// safe to index anywhere permanent yet — re-classified opportunistically
    /// in `infer_ownership` until it resolves one way or the other.
    Pending,
}

pub struct OwnershipGraph {
    /// All nodes, keyed by id.
    nodes: HashMap<u64, Node>,
    /// Maps live pointer → node id (removed on dealloc).
    by_ptr: HashMap<u64, u64>,
    next_id: u64,
    // Diff accumulators — cleared by drain_diff.
    added: Vec<u64>,
    updated: HashSet<u64>,
    removed: Vec<u64>,
    /// Rolling max of ev.ts_nanos across all received events. Used as Diff.ts.
    pub max_ts_seen: u64,
    /// owner id → ids of its live children (2026-07-22 processing-ceiling
    /// fix) — the reverse of `Node.owner`, letting `on_dealloc` find a dying
    /// node's children in O(children) instead of scanning every node in the
    /// graph. Maintained wherever `.owner` is written: populated in
    /// `on_alloc` alongside `edges_out.push`; both the key (the whole
    /// bucket, since every child of a dying node is about to be orphaned
    /// anyway) and the dying node's own membership as a value under its
    /// *own* owner's bucket are removed in `on_dealloc`, in the same place
    /// `edges_out` gets the identical mutation. Never deferred to eviction —
    /// unlike `site_index` below, `on_dealloc` already visits every place
    /// this index could still reference a dying node, so nothing is left
    /// dangling by the time eviction runs.
    owner_index: HashMap<u64, Vec<u64>>,
    /// Node ids grouped by their own *stable* effective-site name (see
    /// `SiteClass`) — lets `infer_ownership` look up "which live nodes could
    /// match name X" in O(candidates for X) instead of scanning every node
    /// in the graph and recomputing its effective site on every single
    /// allocation. Populated in `on_alloc`, promoted into from
    /// `pending_site_ids` in `infer_ownership` as symbols resolve. A node's
    /// bucket entry is removed only at the same point `self.nodes` itself
    /// evicts the node (`drain_diff`) — deferred, not immediate, exactly
    /// mirroring `self.nodes`'s own deferred-to-eviction cleanup: a
    /// dead-but-not-yet-evicted node must remain a *filterable* (via
    /// `n.live`) candidate here, not a vanished one, for the same reason
    /// `self.nodes` itself keeps it around that long.
    ///
    /// `HashSet`, not `Vec` (2026-07-22 drain_diff-cost fix): a workload with
    /// only a handful of distinct real call sites (this project's own
    /// synthetic injection targets among them) puts a very large number of
    /// nodes under the same one or two names — confirmed via profiling that
    /// removing a single dying node from a popular bucket via `Vec::retain`
    /// (O(bucket size)) was ~63-65% of *all* graph-task wall time under
    /// sustained load, dwarfing the diff-construction work eviction was
    /// supposed to be a small tail of. `HashSet::remove` is O(1) average;
    /// iteration for `infer_ownership`'s candidate scan is unaffected (same
    /// `for &cid in ids` shape either type supports), and set semantics are
    /// exactly what this was always logically storing — a node's id can only
    /// ever appear once under its one stable name.
    site_index: HashMap<String, HashSet<u64>>,
    /// Node ids whose own effective-site classification is still provisional
    /// (`SiteClass::Pending`) — re-checked on every `infer_ownership` call
    /// and promoted into `site_index` (or dropped as permanently `NoSite`)
    /// the moment the resolver catches up. In real traffic this stays empty
    /// almost all the time (the writer resolves every address a batch's
    /// events reference before sending that batch), so the re-check cost is
    /// negligible; it exists for correctness in the general case, not as an
    /// optimization. Cleaned up at the same eviction point as `site_index`.
    pending_site_ids: HashSet<u64>,
}

impl Default for OwnershipGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl OwnershipGraph {
    pub fn new() -> Self {
        OwnershipGraph {
            nodes: HashMap::new(),
            by_ptr: HashMap::new(),
            next_id: 0,
            added: Vec::new(),
            updated: HashSet::new(),
            removed: Vec::new(),
            max_ts_seen: 0,
            owner_index: HashMap::new(),
            site_index: HashMap::new(),
            pending_site_ids: HashSet::new(),
        }
    }

    pub fn on_alloc(&mut self, ev: &AllocEvent, resolver: &Resolver) {
        self.max_ts_seen = self.max_ts_seen.max(ev.ts_nanos);
        let id = self.next_id;
        self.next_id += 1;

        let owner_id = self.infer_ownership(&ev.stack, ev.stack_len, resolver);
        let site_class = Self::classify_effective_site(&ev.stack, ev.stack_len, resolver);

        let node = Node {
            id,
            ptr: ev.ptr,
            size: ev.size,
            ts: ev.ts_nanos,
            live: true,
            stack: ev.stack,
            stack_len: ev.stack_len,
            owner: owner_id,
            edges_out: Vec::new(),
            had_owner_once: owner_id.is_some(),
            state: NodeState::Healthy,
            owner_free_ts: None,
            site_class: site_class.clone(),
        };

        // Register as a child of the owner.
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.push(id);
                self.updated.insert(oid);
            }
            self.owner_index.entry(oid).or_default().push(id);
        }

        // Index this node's own site so it can be found as a candidate
        // owner for future allocations — mirrors exactly what the old
        // full-scan would have found by recomputing `effective_site_name`
        // for this node on every later `infer_ownership` call.
        match site_class {
            SiteClass::Resolved(name) => {
                self.site_index.entry(name).or_default().insert(id);
            }
            SiteClass::Pending => {
                self.pending_site_ids.insert(id);
            }
            SiteClass::NoSite => {}
        }

        self.nodes.insert(id, node);
        self.by_ptr.insert(ev.ptr, id);
        self.added.push(id);
    }

    pub fn on_dealloc(&mut self, ptr: u64, ts_nanos: u64) {
        let id = match self.by_ptr.remove(&ptr) {
            Some(id) => id,
            None => return,
        };

        self.removed.push(id);

        // Collect (and clear) this node's children via the owner index —
        // replaces the old full self.nodes.values() scan. Removing the
        // whole bucket here is correct, not just convenient: every child
        // found is about to have its own `owner` cleared below, so none of
        // them belongs under this key (or any key) in the index afterward
        // regardless — no per-child list surgery needed.
        let children: Vec<u64> = self.owner_index.remove(&id).unwrap_or_default();

        for cid in children {
            if let Some(child) = self.nodes.get_mut(&cid) {
                child.owner = None;
                child.had_owner_once = true;
                // Observability-only: records which dealloc caused this —
                // does not feed into ownership or anomaly-state logic.
                child.owner_free_ts = Some(ts_nanos);
            }
            self.updated.insert(cid);
        }

        // Remove this node from its owner's edges_out list and owner-index
        // bucket alike — same mutation, same moment, for the same reason.
        let owner_id = self.nodes.get(&id).and_then(|n| n.owner);
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.retain(|&e| e != id);
                self.updated.insert(oid);
            }
            if let Some(v) = self.owner_index.get_mut(&oid) {
                v.retain(|&e| e != id);
            }
        }

        if let Some(node) = self.nodes.get_mut(&id) {
            node.live = false;
        }
    }

    pub fn on_realloc(&mut self, old_ptr: u64, new_ptr: u64, new_size: u64, resolver: &Resolver) {
        match self.by_ptr.remove(&old_ptr) {
            Some(id) => {
                self.by_ptr.insert(new_ptr, id);
                if let Some(node) = self.nodes.get_mut(&id) {
                    node.ptr = new_ptr;
                    node.size = new_size;
                }
                self.updated.insert(id);
            }
            None => {
                // old_ptr not tracked — treat as a fresh alloc at new_ptr.
                let ev = AllocEvent::new(
                    EventKind::Alloc,
                    new_ptr,
                    0,
                    new_size,
                    0,
                    self.max_ts_seen,
                    [0u64; 16],
                    0,
                );
                self.on_alloc(&ev, resolver);
            }
        }
    }

    pub fn drain_diff(&mut self, resolver: &Resolver) -> GraphMessage {
        let ts = self.max_ts_seen;

        let added_set: HashSet<u64> = self.added.iter().copied().collect();
        let removed_set: HashSet<u64> = self.removed.iter().copied().collect();

        // Nodes born and freed within the same tick are invisible to the consumer.
        let add: Vec<NodeDto> = self.added.iter()
            .filter(|&&id| !removed_set.contains(&id))
            .filter_map(|&id| self.nodes.get(&id))
            .map(|n| Self::node_to_dto(n, resolver))
            .collect();

        let update: Vec<NodeDto> = self.updated.iter()
            .filter(|&&id| !added_set.contains(&id) && !removed_set.contains(&id))
            .filter_map(|&id| self.nodes.get(&id))
            .map(|n| Self::node_to_dto(n, resolver))
            .collect();

        // Only remove nodes the consumer has previously seen (not born this tick).
        let remove: Vec<u64> = self.removed.iter()
            .filter(|&&id| !added_set.contains(&id))
            .copied()
            .collect();

        self.added.clear();
        self.updated.clear();

        // Evict dead nodes from `self.nodes` now — the fix for the
        // unbounded-growth/O(N²) `infer_ownership` scan defect (confirmed
        // 2026-07-22: `on_dealloc` never removed nodes at all, so every
        // allocation ever seen became a permanent HashMap entry, and
        // `infer_ownership`'s per-allocation scan over `self.nodes.values()`
        // got slower as history piled up — total_nodes reached 102,225 with
        // live_nodes at 7 on a provably alloc/free-balanced workload, and
        // the daemon fell 49 seconds behind its own event stream).
        //
        // Safe exactly here, not sooner and not later — every reader of
        // `self.nodes` was checked (not assumed) before choosing this point:
        //   - `add`/`update` above already exclude any id in `removed_set`
        //     before calling `self.nodes.get()` — a node dying this tick
        //     (whether born this tick or earlier) is never read by them.
        //   - `remove` above never calls `self.nodes.get()` at all — it only
        //     copies ids out of `self.removed`.
        //   - `sweep` (anomaly.rs) and `infer_ownership` both filter on
        //     `n.live` before touching anything else — a dead node is inert
        //     to both regardless of whether it's still in the map.
        //   - `on_dealloc`'s own children-reorphaning scan also filters on
        //     `n.live` — and by the time a node reaches `self.removed`,
        //     `on_dealloc` has already unlinked it in both directions (its
        //     owner's `edges_out` no longer references it; its own children,
        //     if any, already had their `owner` cleared) — so no live node
        //     holds a dangling reference to an id evicted here.
        // Evicting any earlier (e.g. directly in `on_dealloc`) would also be
        // safe by this same accounting, but would remove a node mid-tick,
        // before `add`/`update` have run for it this same tick — leaving
        // strictly less margin for a future change to this function to
        // silently start relying on that read. Evicting here keeps a node
        // visible in `self.nodes` for the exact duration current diff/sweep
        // logic can observe it, and gone the instant that window closes.
        //
        // `site_index`/`pending_site_ids` (2026-07-22 processing-ceiling
        // fix) get the *same* deferred-to-eviction treatment, right here,
        // for the identical reason `self.nodes` itself does: a node that
        // died this tick must still be a findable (via `n.live`-filtered)
        // candidate for anything that looked it up between `on_dealloc` and
        // this point, so its index entry cannot vanish any sooner than
        // `self.nodes`'s own entry does. `owner_index` is deliberately NOT
        // touched here — `on_dealloc` already fully cleans a dying node out
        // of it immediately (see `on_dealloc`'s doc comment), so by the time
        // an id reaches `self.removed`, `owner_index` holds no reference to
        // it in either direction; touching it again here would be a no-op.
        for id in self.removed.drain(..) {
            if let Some(node) = self.nodes.get(&id) {
                match &node.site_class {
                    SiteClass::Resolved(name) => {
                        if let Some(v) = self.site_index.get_mut(name) {
                            v.remove(&id);
                            if v.is_empty() {
                                self.site_index.remove(name);
                            }
                        }
                    }
                    SiteClass::Pending => {
                        self.pending_site_ids.remove(&id);
                    }
                    SiteClass::NoSite => {}
                }
            }
            self.nodes.remove(&id);
        }

        GraphMessage::Diff { ts, add, update, remove }
    }

    /// The first frame in `stack[0..stack_len]` that the resolver does not
    /// classify as shared instrumentation — i.e. the real call site. Unknown
    /// addresses (SYMBOLS frame not yet arrived) are treated as machinery by
    /// `Resolver::is_machinery` and skipped past; this is recomputed fresh on
    /// every call rather than cached on the node, so a late-arriving SYMBOLS
    /// frame is picked up on the very next phi match instead of being frozen
    /// at alloc time. Returns `None` if every captured frame is machinery
    /// (stack too shallow relative to instrumentation depth) or the stack is
    /// empty.
    fn effective_site_index(stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> Option<usize> {
        (0..stack_len as usize).find(|&i| {
            let addr = stack[i];
            addr != 0 && !resolver.is_machinery(addr)
        })
    }

    /// φ's granularity is the *function*, not the instruction. Two different
    /// allocation statements in the same function (e.g. `let owner =
    /// vec![...]` followed by a loop calling a helper that allocates
    /// children) resolve to different instruction addresses — exact-IP
    /// matching can never link them, confirmed empirically against a real
    /// compiled binary (0 edges from `wire_producer`'s `nested_alloc`,
    /// despite the ancestor frame genuinely passing through the owner's
    /// enclosing function). `backtrace::resolve` maps any address to its
    /// *containing function's* symbol regardless of which statement inside
    /// it — so matching resolved names instead of raw addresses links these
    /// cases correctly.
    ///
    /// This is a deliberate, documented widening of what a φ edge claims:
    /// not "allocated at the owner's exact site" but "allocated on a call
    /// path that passes through the owner's allocating function." See
    /// `phi_ambiguity_two_unrelated_containers_in_same_function` for the
    /// known, accepted misattribution this widening admits — two unrelated
    /// containers allocated directly in the same calling function become
    /// indistinguishable candidates for a later child of either, resolved
    /// only by the greatest-ts tie-break, not by which one actually caused
    /// the allocation. Unknown/unresolved addresses have no name and cannot
    /// match anything, which is intentional: an address can't be
    /// misattributed to a function nobody has heard of yet.
    ///
    /// 2026-07-22: no longer called from production code (`infer_ownership`
    /// now goes through `classify_effective_site`/`site_index` instead) —
    /// kept for `owner_effective_site_name_matches_the_name_a_childs_search_set_looks_for`,
    /// which pins down that a `Resolved` classification's name is exactly
    /// what this function would independently compute for the same stack.
    #[allow(dead_code)]
    fn effective_site_name(stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> Option<String> {
        let idx = Self::effective_site_index(stack, stack_len, resolver)?;
        Some(resolver.name_for(stack[idx]))
    }

    /// Stability-aware sibling of `effective_site_name` — see `SiteClass`'s
    /// doc comment for why the two must differ. Same skip rule as
    /// `effective_site_index` (`addr != 0 && !is_machinery(addr)`), but
    /// additionally distinguishes an address that's *definitively* known
    /// machinery from one that's merely unresolved so far.
    fn classify_effective_site(stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> SiteClass {
        for i in 0..stack_len as usize {
            let addr = stack[i];
            if addr == 0 {
                continue;
            }
            if !resolver.is_known(addr) {
                return SiteClass::Pending;
            }
            if !resolver.is_machinery(addr) {
                return SiteClass::Resolved(resolver.name_for(addr));
            }
            // Known machinery — keep scanning past it, same as
            // `effective_site_index`.
        }
        SiteClass::NoSite
    }

    /// φ: find the live node whose effective site *function* appears at
    /// depth ≥ 1 in `new_stack` (i.e. anywhere in `new_stack` *after* the new
    /// node's own effective site, which is excluded). Excluding the new
    /// node's own site prevents sibling allocations from the same call site
    /// matching each other (a sibling's effective site equals the new node's
    /// own excluded site) while still matching a genuine owner further up
    /// the stack. Among candidates, prefer greatest `ts`; tie-break by
    /// greatest `id`.
    ///
    /// 2026-07-22 processing-ceiling fix: this used to scan every node in
    /// `self.nodes` on every call (O(N) per allocation, confirmed via
    /// profiling to be ~50% of all graph-task time under sustained load —
    /// see the fix's commit for the measurement). Candidates now come from
    /// `site_index`, narrowed to exactly the names in `search_set`, plus a
    /// `pending_site_ids` re-check that keeps the index correct as symbols
    /// resolve (see `SiteClass`). The tie-break (`max` over `(ts, id)`) is
    /// unchanged — same key, just applied to a pre-narrowed candidate set
    /// instead of the whole map.
    fn infer_ownership(&mut self, new_stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> Option<u64> {
        let len = stack_len as usize;
        let own_idx = match Self::effective_site_index(new_stack, stack_len, resolver) {
            Some(i) => i,
            None => return None,
        };

        let search_set: HashSet<String> = new_stack[own_idx + 1..len]
            .iter()
            .copied()
            .filter(|&a| a != 0 && !resolver.is_machinery(a))
            .map(|a| resolver.name_for(a))
            .collect();

        if search_set.is_empty() {
            return None;
        }

        // Self-healing: promote/demote every still-`Pending` node using the
        // current resolver state, before consulting the index below. In
        // real traffic this loop is over an empty (or near-empty) set — see
        // `pending_site_ids`'s doc comment — so this is not reintroducing
        // the O(N) cost being fixed; it exists so a genuinely-late symbol
        // (the scenario `effective_site_name`'s "recomputed fresh" doc
        // comment already guarantees) still gets found on the very next
        // call, matching pre-fix behavior exactly rather than approximating it.
        let pending_ids: Vec<u64> = self.pending_site_ids.iter().copied().collect();
        for pid in pending_ids {
            let reclass = match self.nodes.get(&pid) {
                Some(node) => Self::classify_effective_site(&node.stack, node.stack_len, resolver),
                None => continue, // defensive: shouldn't happen, pending ids are evicted alongside self.nodes
            };
            match reclass {
                SiteClass::Pending => {} // still unresolved, leave as-is
                SiteClass::Resolved(name) => {
                    self.pending_site_ids.remove(&pid);
                    self.site_index.entry(name.clone()).or_default().insert(pid);
                    if let Some(node) = self.nodes.get_mut(&pid) {
                        node.site_class = SiteClass::Resolved(name);
                    }
                }
                SiteClass::NoSite => {
                    self.pending_site_ids.remove(&pid);
                    if let Some(node) = self.nodes.get_mut(&pid) {
                        node.site_class = SiteClass::NoSite;
                    }
                }
            }
        }

        let mut best: Option<(u64, u64)> = None; // (ts, id) — same tie-break key as the old max_by_key
        for name in &search_set {
            let Some(ids) = self.site_index.get(name) else { continue };
            for &cid in ids {
                let Some(node) = self.nodes.get(&cid) else { continue };
                if !node.live {
                    continue;
                }
                let key = (node.ts, node.id);
                if best.map_or(true, |b| key > b) {
                    best = Some(key);
                }
            }
        }
        best.map(|(_, id)| id)
    }

    pub fn node_by_ptr(&self, ptr: u64) -> Option<&Node> {
        self.by_ptr.get(&ptr).and_then(|&id| self.nodes.get(&id))
    }

    pub fn snapshot(&self, resolver: &Resolver) -> GraphMessage {
        let nodes: Vec<NodeDto> = self.nodes.values()
            .filter(|n| n.live)
            .map(|n| Self::node_to_dto(n, resolver))
            .collect();
        GraphMessage::Snapshot { ts: self.max_ts_seen, nodes }
    }

    pub fn mark_updated(&mut self, id: u64) {
        self.updated.insert(id);
    }

    pub fn nodes_mut(&mut self) -> &mut std::collections::HashMap<u64, Node> {
        &mut self.nodes
    }

    pub fn nodes(&self) -> &std::collections::HashMap<u64, Node> {
        &self.nodes
    }

    /// Observability-only: counts, across currently-live nodes, how many
    /// resolve to a real symbol name vs. fall back to a hex address (or
    /// have no locatable site at all). Reuses the exact same
    /// `effective_site_index`/`name_for` computation `node_to_dto` already
    /// does per node — this is a read-only aggregate over that, not a new
    /// classification, so it cannot diverge from what the wire actually
    /// sends. Exists so the Flutter target-diagnostics banner can tell
    /// "unsymbolized target" apart from other zero-edge causes without
    /// re-deriving the hex-prefix check per node itself.
    pub fn symbol_stats(&self, resolver: &Resolver) -> (u64, u64) {
        let mut resolved = 0u64;
        let mut hex_fallback = 0u64;
        for n in self.nodes.values().filter(|n| n.live) {
            let is_hex = match Self::effective_site_index(&n.stack, n.stack_len, resolver) {
                Some(i) => resolver.name_for(n.stack[i]).starts_with("0x"),
                None => true,
            };
            if is_hex {
                hex_fallback += 1;
            } else {
                resolved += 1;
            }
        }
        (resolved, hex_fallback)
    }

    fn node_to_dto(n: &Node, resolver: &Resolver) -> NodeDto {
        let symbol = Self::effective_site_index(&n.stack, n.stack_len, resolver)
            .map(|i| resolver.name_for(n.stack[i]))
            .unwrap_or_else(|| "?".to_owned());
        NodeDto {
            id: n.id,
            ptr: n.ptr,
            size: n.size,
            ts: n.ts,
            symbol,
            live: n.live,
            state: n.state.clone(),
            edges: n.edges_out.clone(),
        }
    }
}

#[cfg(test)]
mod symbol_stats_tests {
    use super::*;

    /// Three live nodes: one with a resolved effective site, one whose
    /// effective site address the resolver never learned (hex fallback),
    /// and one with no locatable site at all (every frame classified as
    /// machinery — also hex fallback, per `symbol_stats`'s doc comment). A
    /// fourth, dead node is excluded entirely — `symbol_stats` only counts
    /// live nodes, matching what `node_to_dto`/the wire actually reports for
    /// the currently-visible graph.
    #[test]
    fn counts_resolved_vs_hex_fallback_across_live_nodes_only() {
        let mut r = Resolver::new();
        r.insert(0xAAA1, "myapp::resolved_site".to_owned(), false);
        // 0xBBB1 is deliberately never inserted — name_for falls back to hex.
        r.insert(0xCCC1, "alloc::vec::Vec<T>::with_capacity".to_owned(), true); // machinery only

        let mut g = OwnershipGraph::new();

        let mut resolved_stack = [0u64; 16];
        resolved_stack[0] = 0xAAA1;
        g.on_alloc(&AllocEvent::new(EventKind::Alloc, 0x1000, 0, 8, 8, 100, resolved_stack, 1), &r);

        let mut hex_stack = [0u64; 16];
        hex_stack[0] = 0xBBB1;
        g.on_alloc(&AllocEvent::new(EventKind::Alloc, 0x2000, 0, 8, 8, 200, hex_stack, 1), &r);

        let mut machinery_only_stack = [0u64; 16];
        machinery_only_stack[0] = 0xCCC1;
        g.on_alloc(&AllocEvent::new(EventKind::Alloc, 0x3000, 0, 8, 8, 300, machinery_only_stack, 1), &r);

        // A fourth, now-dead node — must not be counted at all.
        let mut dead_stack = [0u64; 16];
        dead_stack[0] = 0xAAA1;
        g.on_alloc(&AllocEvent::new(EventKind::Alloc, 0x4000, 0, 8, 8, 400, dead_stack, 1), &r);
        g.on_dealloc(0x4000, 500);

        let (resolved, hex_fallback) = g.symbol_stats(&r);
        assert_eq!(resolved, 1, "only the truly-resolved live node should count as resolved");
        assert_eq!(hex_fallback, 2, "unresolved-address and no-locatable-site live nodes both count as hex fallback");
    }

    #[test]
    fn empty_graph_reports_zero_for_both_counts() {
        let g = OwnershipGraph::new();
        let r = Resolver::new();
        assert_eq!(g.symbol_stats(&r), (0, 0));
    }
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    /// φ's core invariant, stated directly rather than only observed
    /// through `on_alloc`'s resulting edges: **an owner's own effective
    /// site name must appear in the set of names a would-be child searches
    /// for.** `effective_site_name` (how a node names itself) and
    /// `infer_ownership`'s search set (how a child looks for an owner) are
    /// two separate computations over two different stacks; nothing in the
    /// type system forces them to agree. They *happen* to agree today
    /// because both call the same `effective_site_index`/`is_machinery`
    /// skip rule — but that agreement is exactly what the
    /// `Vec::with_capacity` bug violated one layer upstream, when
    /// `is_machinery_symbol` (in `heaplens-alloc`) classified one stdlib
    /// allocation idiom differently from another, silently shifting where
    /// `effective_site_index` landed for a container's own stack without
    /// touching how children compute their search sets. This test pins the
    /// invariant down explicitly so a future change to either computation
    /// (not just to the classifier) that breaks their agreement fails here,
    /// not three stages later against a real target.
    #[test]
    fn owner_effective_site_name_matches_the_name_a_childs_search_set_looks_for() {
        let mut r = Resolver::new();
        // Owner's own stack: a stdlib allocation-plumbing frame (machinery)
        // sitting in front of the owner's real call site — exactly the
        // Vec::with_capacity shape (own_idx must skip past the plumbing
        // frame to reach the real site, not stop on it).
        r.insert(0xAAA1, "alloc::vec::Vec<T>::with_capacity".to_owned(), true);
        r.insert(0xAAA2, "myapp::main".to_owned(), false);
        // Child's own stack: its own real site, then the SAME real
        // ancestor site the owner names itself by.
        r.insert(0xBBB1, "myapp::helper".to_owned(), false);

        let mut owner_stack = [0u64; 16];
        owner_stack[0] = 0xAAA1;
        owner_stack[1] = 0xAAA2;
        let owner_name = OwnershipGraph::effective_site_name(&owner_stack, 2, &r)
            .expect("owner must resolve to a real effective site, not the machinery frame");
        assert_eq!(
            owner_name, "myapp::main",
            "owner's effective site must skip the machinery frame and land on its real call site"
        );

        let mut child_stack = [0u64; 16];
        child_stack[0] = 0xBBB1;
        child_stack[1] = 0xAAA2;
        let child_own_idx = OwnershipGraph::effective_site_index(&child_stack, 2, &r)
            .expect("child must resolve its own effective site");
        let child_search_set: HashSet<String> = child_stack[child_own_idx + 1..2]
            .iter()
            .copied()
            .filter(|&a| a != 0 && !r.is_machinery(a))
            .map(|a| r.name_for(a))
            .collect();

        assert!(
            child_search_set.contains(&owner_name),
            "the owner's effective site name must appear in the child's search set — \
             got owner_name={owner_name:?}, child_search_set={child_search_set:?}"
        );
    }
}

// 2026-07-22 processing-ceiling fix: proves `site_index`/`owner_index`
// never diverge from what a full scan would find, across the exact
// lifecycle the eviction question is about — alloc, ownership assignment,
// reorphaning on dealloc, and eviction at drain_diff — plus late symbol
// resolution, the one case the index design has to actively defend against
// (see `SiteClass`'s doc comment). This is the test that would have failed
// immediately had the site index not been cleaned up at the eviction point.
#[cfg(test)]
mod index_consistency_tests {
    use super::*;

    fn make_ev(ptr: u64, ts: u64, stack: &[u64]) -> AllocEvent {
        let mut s = [0u64; 16];
        let len = stack.len().min(16);
        s[..len].copy_from_slice(&stack[..len]);
        AllocEvent::new(EventKind::Alloc, ptr, 0, 32, 8, ts, s, len as u8)
    }

    /// Byte-for-byte replica of the pre-fix `infer_ownership` — scans every
    /// node directly rather than consulting `site_index`/`pending_site_ids`.
    /// Exists only to prove the indexed implementation returns the exact
    /// same answer; if this and `infer_ownership` ever diverge, that is
    /// precisely the "index says something different from a full scan" bug
    /// this test module exists to catch.
    fn brute_force_infer_ownership(
        nodes: &HashMap<u64, Node>,
        new_stack: &[u64; 16],
        stack_len: u8,
        resolver: &Resolver,
    ) -> Option<u64> {
        let len = stack_len as usize;
        let own_idx = OwnershipGraph::effective_site_index(new_stack, stack_len, resolver)?;
        let search_set: HashSet<String> = new_stack[own_idx + 1..len]
            .iter()
            .copied()
            .filter(|&a| a != 0 && !resolver.is_machinery(a))
            .map(|a| resolver.name_for(a))
            .collect();
        nodes
            .values()
            .filter(|n| {
                n.live
                    && OwnershipGraph::effective_site_name(&n.stack, n.stack_len, resolver)
                        .is_some_and(|name| search_set.contains(&name))
            })
            .max_by_key(|n| (n.ts, n.id))
            .map(|n| n.id)
    }

    /// Asserts the indexed `infer_ownership` agrees with a brute-force full
    /// scan for a hypothetical new allocation with `probe_stack`, without
    /// actually creating that allocation — so the same graph state can be
    /// probed repeatedly at different points in a scenario.
    fn assert_index_matches_brute_force(
        g: &mut OwnershipGraph,
        probe_stack: &[u64; 16],
        probe_len: u8,
        r: &Resolver,
    ) {
        let brute = brute_force_infer_ownership(&g.nodes, probe_stack, probe_len, r);
        let indexed = g.infer_ownership(probe_stack, probe_len, r);
        assert_eq!(
            indexed, brute,
            "indexed infer_ownership diverged from a brute-force full scan for probe_stack={probe_stack:?}"
        );
    }

    #[test]
    fn index_matches_brute_force_across_alloc_dealloc_eviction_and_late_resolution() {
        let owner_site = 0xAAAA;
        let leaf_site = 0xBBBB;
        let late_site = 0xCCCC; // deliberately left unresolved at first alloc

        let mut r = Resolver::new();
        r.insert(owner_site, "owner_fn".to_owned(), false);
        r.insert(leaf_site, "leaf_fn".to_owned(), false);
        // late_site intentionally NOT inserted yet.

        let mut g = OwnershipGraph::new();
        let probe_owner_leaf = || {
            let mut s = [0u64; 16];
            s[0] = leaf_site;
            s[1] = owner_site;
            s
        };

        // Step 1: a plain owner node, and a child owned by it — sanity
        // baseline before anything interesting (pending/eviction) happens.
        g.on_alloc(&make_ev(0x1000, 100, &[owner_site]), &r);
        g.on_alloc(&make_ev(0x2000, 200, &[leaf_site, owner_site]), &r);
        assert_index_matches_brute_force(&mut g, &probe_owner_leaf(), 2, &r);

        // Step 2: a node whose own stack references `late_site`, unresolved
        // at creation — must land in `pending_site_ids`, not `site_index`.
        g.on_alloc(&make_ev(0x3000, 300, &[late_site]), &r);
        let pending_id = g.node_by_ptr(0x3000).unwrap().id;
        assert!(
            g.pending_site_ids.contains(&pending_id),
            "unresolved-address node must be pending, not indexed"
        );
        assert!(matches!(g.nodes.get(&pending_id).unwrap().site_class, SiteClass::Pending));

        // A probe for a hypothetical child of late_site must agree with
        // brute force (both find nothing — late_site isn't resolved yet).
        let mut probe_late = [0u64; 16];
        probe_late[0] = leaf_site;
        probe_late[1] = late_site;
        assert_index_matches_brute_force(&mut g, &probe_late, 2, &r);

        // Step 3: late_site resolves. The next `on_alloc` (exactly like
        // production, symbols always arriving via a real event) must
        // promote the pending node into `site_index` and find it as the
        // owner — the scenario `effective_site_name`'s "recomputed fresh"
        // guarantee exists for, now proven for the indexed path too.
        r.insert(late_site, "late_fn".to_owned(), false);
        g.on_alloc(&make_ev(0x4000, 400, &probe_late), &r);
        let late_child = g.node_by_ptr(0x4000).unwrap();
        assert_eq!(
            late_child.owner,
            Some(pending_id),
            "late-resolved node must be found as owner once its symbol arrives"
        );
        assert!(
            !g.pending_site_ids.contains(&pending_id),
            "promoted node must leave pending_site_ids"
        );
        assert!(matches!(
            &g.nodes.get(&pending_id).unwrap().site_class,
            SiteClass::Resolved(n) if n == "late_fn"
        ));

        // Step 4: dealloc the original owner — reorphans its child
        // immediately, and its owner_index bucket must be gone immediately
        // too (not deferred to eviction, per on_dealloc's contract).
        let owner_id = g.node_by_ptr(0x1000).unwrap().id;
        g.on_dealloc(0x1000, 500);
        assert!(
            !g.owner_index.contains_key(&owner_id),
            "owner_index bucket must be cleared immediately on dealloc, not deferred"
        );
        assert_index_matches_brute_force(&mut g, &probe_owner_leaf(), 2, &r);

        // Step 5: drain_diff evicts the dead owner. site_index must hold no
        // reference to it afterward — the exact bug this test would have
        // caught immediately had eviction cleanup been missing.
        let _ = g.drain_diff(&r);
        assert!(
            g.site_index.values().all(|ids| !ids.contains(&owner_id)),
            "evicted node must not remain in site_index under any name"
        );
        assert!(!g.nodes.contains_key(&owner_id), "evicted node must be gone from self.nodes");
        assert_index_matches_brute_force(&mut g, &probe_owner_leaf(), 2, &r);
    }
}
