use heaplens_protocol::{AllocEvent, EventKind, GraphMessage};
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::resolver::Resolver;

fn make_ev(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 8];
    let len = stack.len().min(8);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(kind, ptr, old_ptr, size, 8, ts, s, len as u8)
}

// φ inference: owner of N = live node whose stack[0] appears anywhere in N.stack
#[test]
fn phi_inference_finds_owner_by_stack_overlap() {
    let mut g = OwnershipGraph::new();

    // Alloc owner O with stack[0] = 0xAAAA
    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o_ev);

    // Alloc child C whose stack contains 0xAAAA at position 1
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    // Find child node (ptr 0x2000)
    let child = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let owner = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert_eq!(child.edges.contains(&owner.id), false); // edges_out are on the owner
    // The owner's NodeDto edges should contain child's id
    let owner_dto = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert!(owner_dto.edges.contains(&child.id));
}

// φ inference: no match → root (owner = None)
#[test]
fn phi_inference_root_when_no_match() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0x9999]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add.len(), 1);
    // No owner means edges from parent: if root, no parent pushed it to edges_out
    // Just verify it's in add with id assigned
    assert_eq!(add[0].ptr, 0x1000);
}

// φ inference: tie-break by greatest ts
#[test]
fn phi_inference_tiebreak_by_greatest_ts() {
    let mut g = OwnershipGraph::new();

    // Two candidates with same stack[0] value appearing in child's stack
    let o1 = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o1);
    let o2 = make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[0xAAAA]); // same stack[0], greater ts
    g.on_alloc(&o2);

    let c_ev = make_ev(EventKind::Alloc, 0x3000, 0, 32, 300, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    // Child should be owned by o2 (ts=200 > ts=100), so o2's NodeDto.edges contains child.id
    let o2_dto = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let c_dto = add.iter().find(|n| n.ptr == 0x3000).unwrap();
    assert!(o2_dto.edges.contains(&c_dto.id), "o2 should own the child (greatest ts)");

    let o1_dto = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert!(!o1_dto.edges.contains(&c_dto.id), "o1 should not own the child");
}

// dealloc: child.owner becomes None, child.had_owner_once becomes true
#[test]
fn dealloc_orphans_children() {
    let mut g = OwnershipGraph::new();

    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o_ev);
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    let r = Resolver::new();
    let _ = g.drain_diff(&r);

    g.on_dealloc(0x1000);

    let diff = g.drain_diff(&r);
    let (_, updated, removed) = unwrap_diff(diff);

    assert!(!removed.is_empty(), "owner should be in removed");
    assert!(!updated.is_empty(), "child should be in updated");

    // Verify no id appears in both update and remove
    let update_ids: std::collections::HashSet<u64> = updated.iter().map(|n| n.id).collect();
    for &rid in &removed {
        assert!(!update_ids.contains(&rid), "id {rid} must not be in both update and remove");
    }

    // Verify had_owner_once on the still-live child
    let child_node = g.node_by_ptr(0x2000).expect("child still live");
    assert!(child_node.had_owner_once, "child.had_owner_once must be true after owner freed");
    assert!(child_node.owner.is_none(), "child.owner must be None after owner freed");
}

// cascaded dealloc within same tick: remove ∩ update must be disjoint
#[test]
fn cascaded_dealloc_same_tick_disjoint() {
    let mut g = OwnershipGraph::new();

    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&o_ev);
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[0xBBBB, 0xAAAA]);
    g.on_alloc(&c_ev);

    // No drain between the two deallocs — both within the same tick
    g.on_dealloc(0x2000); // child first
    g.on_dealloc(0x1000); // then owner

    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, update, remove) = unwrap_diff(diff);

    let remove_set: std::collections::HashSet<u64> = remove.iter().copied().collect();
    for n in &add {
        assert!(!remove_set.contains(&n.id), "add/remove overlap: id {}", n.id);
    }
    for n in &update {
        assert!(!remove_set.contains(&n.id), "update/remove overlap: id {}", n.id);
    }
}

// realloc: ptr migrates, size updates, same id retained
#[test]
fn realloc_migrates_ptr_and_updates_size() {
    let mut g = OwnershipGraph::new();

    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let original_id = add[0].id;

    // Realloc: old_ptr=0x1000, new_ptr=0x2000, new_size=128
    g.on_realloc(0x1000, 0x2000, 128);

    let diff2 = g.drain_diff(&r);
    let (_, updated, _) = unwrap_diff(diff2);

    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].id, original_id, "same id after realloc");
    assert_eq!(updated[0].ptr, 0x2000, "ptr updated");
    assert_eq!(updated[0].size, 128, "size updated");
}

// diff accumulation: second drain_diff returns empty
#[test]
fn drain_diff_clears_accumulators() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let _ = g.drain_diff(&r);

    let diff2 = g.drain_diff(&r);
    let (add, updated, removed) = unwrap_diff(diff2);
    assert!(add.is_empty() && updated.is_empty() && removed.is_empty(),
        "second drain should be empty");
}

// resolver join: symbol attached at drain_diff time
#[test]
fn drain_diff_attaches_symbol_from_resolver() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    g.on_alloc(&ev);

    let mut r = Resolver::new();
    r.insert(0xAAAA, "my_alloc_site".to_owned());

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "my_alloc_site");
}

// resolver fallback: unknown addr → "0x…"
#[test]
fn drain_diff_uses_hex_fallback_for_unknown_symbol() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xDEAD]);
    g.on_alloc(&ev);
    let r = Resolver::new();
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "0xdead");
}

fn unwrap_diff(msg: GraphMessage) -> (Vec<heaplens_protocol::NodeDto>, Vec<heaplens_protocol::NodeDto>, Vec<u64>) {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => (add, update, remove),
        GraphMessage::Snapshot { .. } => panic!("expected Diff, got Snapshot"),
    }
}
