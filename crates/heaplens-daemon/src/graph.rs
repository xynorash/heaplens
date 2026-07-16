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
        }
    }

    pub fn on_alloc(&mut self, ev: &AllocEvent, resolver: &Resolver) {
        self.max_ts_seen = self.max_ts_seen.max(ev.ts_nanos);
        let id = self.next_id;
        self.next_id += 1;

        let owner_id = self.infer_ownership(&ev.stack, ev.stack_len, resolver);

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
        };

        // Register as a child of the owner.
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.push(id);
                self.updated.insert(oid);
            }
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

        // Collect children to orphan — avoid borrow issues by collecting ids first.
        let children: Vec<u64> = self
            .nodes
            .values()
            .filter(|n| n.live && n.owner == Some(id))
            .map(|n| n.id)
            .collect();

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

        // Remove this node from its owner's edges_out list.
        let owner_id = self.nodes.get(&id).and_then(|n| n.owner);
        if let Some(oid) = owner_id {
            if let Some(owner) = self.nodes.get_mut(&oid) {
                owner.edges_out.retain(|&e| e != id);
                self.updated.insert(oid);
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
        self.removed.clear();

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
    fn effective_site_name(stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> Option<String> {
        let idx = Self::effective_site_index(stack, stack_len, resolver)?;
        Some(resolver.name_for(stack[idx]))
    }

    /// φ: find the live node whose effective site *function* appears at
    /// depth ≥ 1 in `new_stack` (i.e. anywhere in `new_stack` *after* the new
    /// node's own effective site, which is excluded). Excluding the new
    /// node's own site prevents sibling allocations from the same call site
    /// matching each other (a sibling's effective site equals the new node's
    /// own excluded site) while still matching a genuine owner further up
    /// the stack. Among candidates, prefer greatest `ts`; tie-break by
    /// greatest `id`.
    fn infer_ownership(&self, new_stack: &[u64; 16], stack_len: u8, resolver: &Resolver) -> Option<u64> {
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

        self.nodes
            .values()
            .filter(|n| {
                n.live
                    && Self::effective_site_name(&n.stack, n.stack_len, resolver)
                        .is_some_and(|name| search_set.contains(&name))
            })
            .max_by_key(|n| (n.ts, n.id))
            .map(|n| n.id)
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
