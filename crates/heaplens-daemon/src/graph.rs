use std::collections::{HashMap, HashSet};

use heaplens_protocol::{AllocEvent, EventKind, GraphMessage, NodeDto, NodeState};

use crate::resolver::Resolver;

pub struct Node {
    pub id: u64,
    pub ptr: u64,
    pub size: u64,
    pub ts: u64,
    pub live: bool,
    pub stack: [u64; 8],
    pub stack_len: u8,
    /// Id of the node that owns this one (φ inference result).
    pub owner: Option<u64>,
    /// Ids of nodes this node owns.
    pub edges_out: Vec<u64>,
    /// True once an owner was assigned and then freed — used by M4 orphan detection.
    pub had_owner_once: bool,
    /// Current anomaly classification; updated by anomaly::sweep.
    pub state: NodeState,
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
    /// Monotonic timestamp of the last processed event (used as Diff.ts).
    last_ts: u64,
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
            last_ts: 0,
        }
    }

    pub fn on_alloc(&mut self, ev: &AllocEvent) {
        self.last_ts = self.last_ts.max(ev.ts_nanos);
        let id = self.next_id;
        self.next_id += 1;

        let owner_id = self.infer_ownership(&ev.stack, ev.stack_len);

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

    pub fn on_dealloc(&mut self, ptr: u64) {
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

    pub fn on_realloc(&mut self, old_ptr: u64, new_ptr: u64, new_size: u64) {
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
                    self.last_ts,
                    [0u64; 8],
                    0,
                );
                self.on_alloc(&ev);
            }
        }
    }

    pub fn drain_diff(&mut self, resolver: &Resolver) -> GraphMessage {
        let ts = self.last_ts;

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

    /// φ: find the live node whose `stack[0]` appears anywhere in `new_stack[0..stack_len]`.
    /// Among candidates, prefer greatest `ts`; tie-break by greatest `id`.
    fn infer_ownership(&self, new_stack: &[u64; 8], stack_len: u8) -> Option<u64> {
        let len = stack_len as usize;
        let search_set: HashSet<u64> = new_stack[..len]
            .iter()
            .copied()
            .filter(|&a| a != 0)
            .collect();

        self.nodes
            .values()
            .filter(|n| n.live && n.stack_len > 0 && search_set.contains(&n.stack[0]))
            .max_by_key(|n| (n.ts, n.id))
            .map(|n| n.id)
    }

    pub fn node_by_ptr(&self, ptr: u64) -> Option<&Node> {
        self.by_ptr.get(&ptr).and_then(|&id| self.nodes.get(&id))
    }

    fn node_to_dto(n: &Node, resolver: &Resolver) -> NodeDto {
        let symbol = if n.stack_len > 0 {
            resolver.name_for(n.stack[0])
        } else {
            "?".to_owned()
        };
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
