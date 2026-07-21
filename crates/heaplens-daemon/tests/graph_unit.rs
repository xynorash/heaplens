use heaplens_protocol::{AllocEvent, EventKind, GraphMessage, NodeState};
use heaplens_daemon::anomaly::sweep;
use heaplens_daemon::config::Config;
use heaplens_daemon::graph::OwnershipGraph;
use heaplens_daemon::resolver::Resolver;

fn make_ev(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, ts: u64, stack: &[u64]) -> AllocEvent {
    let mut s = [0u64; 16];
    let len = stack.len().min(16);
    s[..len].copy_from_slice(&stack[..len]);
    AllocEvent::new(kind, ptr, old_ptr, size, 8, ts, s, len as u8)
}

/// A resolver where every address in `sites` is classified as genuine caller
/// code (non-machinery). Test stacks are written as `[own_site, ancestor_site,
/// ...]` — with every site in this list, `own_site` lands at effective-site
/// index 0, matching the old tests' shape while going through the new
/// classification-based lookup rather than a raw index.
fn resolver_with_real_sites(sites: &[u64]) -> Resolver {
    let mut r = Resolver::new();
    for &a in sites {
        r.insert(a, format!("site_0x{a:x}"), false);
    }
    r
}

fn unwrap_diff(msg: GraphMessage) -> (Vec<heaplens_protocol::NodeDto>, Vec<heaplens_protocol::NodeDto>, Vec<u64>) {
    match msg {
        GraphMessage::Diff { add, update, remove, .. } => (add, update, remove),
        other => panic!("expected Diff, got {other:?}"),
    }
}

// --- Fix 3(a): star topology ---------------------------------------------
// N children sharing one leaf call site, all owned by one owner further up
// the stack: owner gets N edges_out, and — the actual bug this branch
// fixes — no child ever matches another child (would collapse into a chain
// if sibling exclusion weren't applied).
#[test]
fn phi_star_topology_children_share_owner_not_each_other() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();

    let owner_ev = make_ev(EventKind::Alloc, 0x1000, 0, 4096, 100, &[owner_site]);
    g.on_alloc(&owner_ev, &r);

    for i in 0..5u64 {
        let child_ev = make_ev(EventKind::Alloc, 0x2000 + i, 0, 128, 200 + i, &[leaf_site, owner_site]);
        g.on_alloc(&child_ev, &r);
    }

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    let owner = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    assert_eq!(owner.edges.len(), 5, "owner should have all 5 children as edges_out");

    for i in 0..5u64 {
        let child = add.iter().find(|n| n.ptr == 0x2000 + i).unwrap();
        assert!(child.edges.is_empty(), "child must not own any sibling");
        assert!(owner.edges.contains(&child.id));
    }
}

// --- Fix 3(b): genuine chain (A -> B -> C) --------------------------------
#[test]
fn phi_genuine_chain_a_owns_b_owns_c() {
    let a_site = 0xA000;
    let b_site = 0xB000;
    let c_site = 0xC000;
    let r = resolver_with_real_sites(&[a_site, b_site, c_site]);
    let mut g = OwnershipGraph::new();

    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[a_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[b_site, a_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x3000, 0, 64, 300, &[c_site, b_site]), &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    let a = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let b = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let c = add.iter().find(|n| n.ptr == 0x3000).unwrap();

    assert!(a.edges.contains(&b.id), "A should own B");
    assert!(b.edges.contains(&c.id), "B should own C");
    assert!(!a.edges.contains(&c.id), "A must not directly own C");
}

// --- Fix 3(c): sibling exclusion ------------------------------------------
// Two nodes with identical effective site (no distinct owner further up):
// each one's own site is excluded from its own search window, so neither
// can match the other.
#[test]
fn phi_sibling_exclusion_identical_site_no_edge() {
    let shared_site = 0xDDDD;
    let r = resolver_with_real_sites(&[shared_site]);
    let mut g = OwnershipGraph::new();

    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[shared_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[shared_site]), &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    let n1 = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let n2 = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    assert!(n1.edges.is_empty());
    assert!(n2.edges.is_empty());
}

// --- Fix 3(d): root (no candidate) -----------------------------------------
#[test]
fn phi_root_when_no_candidate_matches() {
    let site = 0x9999;
    let r = resolver_with_real_sites(&[site]);
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[site]);
    g.on_alloc(&ev, &r);
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add.len(), 1);
    assert_eq!(add[0].ptr, 0x1000);
    assert!(add[0].edges.is_empty());
}

// φ: an entirely-machinery stack (no non-machinery frame at all) is also root.
#[test]
fn phi_root_when_stack_is_entirely_machinery() {
    let mut r = Resolver::new();
    r.insert(0xF00D, "heaplens_alloc::capture::capture_stack".to_owned(), true);
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xF00D]);
    g.on_alloc(&ev, &r);
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].edges.len(), 0);
}

// --- Fix 3(e): tie-break by greatest ts, among valid candidates only -------
#[test]
fn phi_tiebreak_by_greatest_ts_among_valid_candidates() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let decoy_site = 0xEEEE; // a real site that just never appears in the child's stack
    let r = resolver_with_real_sites(&[owner_site, leaf_site, decoy_site]);
    let mut g = OwnershipGraph::new();

    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[owner_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x9000, 0, 64, 150, &[decoy_site]), &r); // never matched
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[owner_site]), &r); // same site, greater ts

    let c_ev = make_ev(EventKind::Alloc, 0x3000, 0, 32, 300, &[leaf_site, owner_site]);
    g.on_alloc(&c_ev, &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);

    let older = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let newer = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let decoy = add.iter().find(|n| n.ptr == 0x9000).unwrap();
    let child = add.iter().find(|n| n.ptr == 0x3000).unwrap();

    assert!(newer.edges.contains(&child.id), "greatest-ts candidate should own the child");
    assert!(!older.edges.contains(&child.id));
    assert!(decoy.edges.is_empty(), "decoy never matched the child's search window");
}

// --- Regression: the original bug ------------------------------------------
// Two allocations from genuinely different real call sites, both preceded by
// the same shared machinery frame (as every allocation is in production,
// since every event funnels through capture_stack -> record -> alloc). They
// must not be attributed to each other just because they share that leading
// frame — this is exactly the universal-chain failure mode that was found.
#[test]
fn phi_shared_machinery_prefix_does_not_cause_false_ownership() {
    let mut r = Resolver::new();
    r.insert(0xF00D, "heaplens_alloc::record".to_owned(), true);
    r.insert(0xAAAA, "producer::site_a".to_owned(), false);
    r.insert(0xBBBB, "producer::site_b".to_owned(), false);

    let mut g = OwnershipGraph::new();
    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xF00D, 0xAAAA]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[0xF00D, 0xBBBB]), &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let first = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let second = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    assert!(first.edges.is_empty());
    assert!(second.edges.is_empty(), "second alloc must not be chained onto the first via the shared machinery frame");
}

// --- Fix 2b: function-name granularity -------------------------------------
// Reproduces the exact shape that produced 0 edges against the real
// wire_producer.exe: an owner allocated by one statement in a function, and
// a child allocated (via a helper) by a *different* statement in the same
// function. The owner's own effective-site address and the child's ancestor
// address are deliberately different (addrA != addrB) but both resolve to
// the same function name — only name-based matching can link them.
#[test]
fn phi_links_owner_and_child_via_same_enclosing_function_different_addresses() {
    let addr_a = 0xA1; // owner's own site: `let outer = vec![...]` inside nested_alloc
    let addr_b = 0xA2; // child's ancestor: the `leaf_alloc(...)` call site inside nested_alloc
    let leaf_site = 0xB1;
    assert_ne!(addr_a, addr_b, "the two statements must be genuinely different addresses");

    let mut r = Resolver::new();
    r.insert(addr_a, "producer::nested_alloc".to_owned(), false);
    r.insert(addr_b, "producer::nested_alloc".to_owned(), false); // same function, different IP
    r.insert(leaf_site, "producer::leaf_alloc".to_owned(), false);

    let mut g = OwnershipGraph::new();
    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 128, 100, &[addr_a]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[leaf_site, addr_b]), &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let owner = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let child = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    assert!(owner.edges.contains(&child.id), "owner should own child via shared enclosing function name, despite different exact addresses");
}

// --- Documented, accepted limitation ---------------------------------------
// Two unrelated containers allocated directly in the same calling function
// are indistinguishable candidates for a later child of either — φ resolves
// this only by the greatest-ts tie-break, not by which container actually
// caused the allocation. This is not a bug: it is the cost of function-level
// granularity, and this test documents/locks in the actual (imprecise)
// behavior rather than asserting correctness that the design cannot provide.
#[test]
fn phi_ambiguity_two_unrelated_containers_in_same_function() {
    let main_site_a = 0xF1; // `let vec_a = vec![...];` in main
    let main_site_b = 0xF2; // `let vec_b = vec![...];` in main, unrelated to vec_a
    let call_site = 0xF3;   // `helper()` call in main, later allocates a child
    let child_site = 0xC1;

    let mut r = Resolver::new();
    r.insert(main_site_a, "producer::main".to_owned(), false);
    r.insert(main_site_b, "producer::main".to_owned(), false);
    r.insert(call_site, "producer::main".to_owned(), false);
    r.insert(child_site, "producer::helper".to_owned(), false);

    let mut g = OwnershipGraph::new();
    // vec_a allocated first, vec_b allocated later (greater ts) — both live,
    // both resolve to "producer::main", neither is actually related to the
    // child that main() later causes helper() to allocate.
    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[main_site_a]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 64, 200, &[main_site_b]), &r);

    let child_ev = make_ev(EventKind::Alloc, 0x3000, 0, 32, 300, &[child_site, call_site]);
    g.on_alloc(&child_ev, &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let vec_a = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let vec_b = add.iter().find(|n| n.ptr == 0x2000).unwrap();
    let child = add.iter().find(|n| n.ptr == 0x3000).unwrap();

    // Documented behavior: the child is attributed to whichever "main"-site
    // node has the greatest ts (vec_b), not to neither (which would be the
    // structurally honest answer, but is not what this design computes).
    assert!(vec_b.edges.contains(&child.id), "greatest-ts candidate wins, even though it has no real relationship to the child — this is the documented ambiguity, not a bug");
    assert!(!vec_a.edges.contains(&child.id));
}

// --- Fix 2b, discriminator precision ---------------------------------------
// φ's actual discriminator among same-effective-site-name live candidates is
// *recency*: greatest `ts` wins (see `infer_ownership`'s `max_by_key((ts,
// id))`). There is no thread id, no call/return bracketing, no dynamic
// extent — `Node` carries none of that. This pair of tests pins down exactly
// what that buys and what it doesn't, using the real access pattern
// `wire_producer`/`demo_producer` exercise: the *same* function invoked K
// times, with earlier invocations still live (held by the caller) when later
// ones start — this is not hypothetical, it's what `nested_alloc` called in
// a loop over a retained `Vec` actually produces.

// K owners of the identical symbol, each still live when the next is
// allocated (so all K are simultaneously candidates for later children) —
// exactly `wire_producer`'s shape. Recency correctly partitions children to
// their true owner here because each owner's children are all allocated
// strictly between that owner's ts and the next owner's ts (nested/sequential
// access — nothing overlaps).
#[test]
fn phi_k_invocations_of_same_function_partition_by_recency_when_sequential() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();

    let k = 8u64;
    let m = 10u64;
    let mut ts = 0u64;
    for i in 0..k {
        ts += 1;
        let owner_ptr = 0x1000 + i * 0x100;
        g.on_alloc(&make_ev(EventKind::Alloc, owner_ptr, 0, 64, ts, &[owner_site]), &r);
        for j in 0..m {
            ts += 1;
            let child_ptr = owner_ptr + 0x10 + j;
            g.on_alloc(&make_ev(EventKind::Alloc, child_ptr, 0, 32, ts, &[leaf_site, owner_site]), &r);
        }
    }

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let owners: Vec<_> = add.iter().filter(|n| !n.edges.is_empty()).collect();
    assert_eq!(owners.len(), k as usize, "expected {k} distinct owners of the same symbol");
    for o in &owners {
        assert_eq!(o.edges.len(), m as usize, "each owner should keep exactly its own {m} children");
    }
    let mut claimed = std::collections::HashSet::new();
    for o in &owners {
        for &cid in &o.edges {
            assert!(claimed.insert(cid), "child {cid} claimed by more than one same-symbol owner");
        }
    }
}

// The documented failure mode: a child logically belonging to an EARLIER
// owner, but allocated AFTER a newer same-name owner already exists (e.g.
// two overlapping/concurrent calls to the same function — the jury's "two
// threads both in make_family" question), gets attributed to the newer
// owner. Recency has no notion of "which owner's dynamic extent this
// allocation falls within" — only "which same-named node was most recently
// allocated." This is the accepted cost of function-granularity + recency
// tie-break, not a bug; the K-invocation test above shows it's the exception,
// not the common case, for the access pattern this project's producers use.
#[test]
fn phi_recency_discriminator_misattributes_late_child_to_newer_same_name_owner() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();

    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[owner_site]), &r); // owner1
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[leaf_site, owner_site]), &r); // child1a: correctly owner1 (only live candidate)
    g.on_alloc(&make_ev(EventKind::Alloc, 0x3000, 0, 64, 300, &[owner_site]), &r); // owner2, now also live and same name

    // owner1's second child arrives after owner2 already exists.
    let late_child = make_ev(EventKind::Alloc, 0x4000, 0, 32, 400, &[leaf_site, owner_site]);
    g.on_alloc(&late_child, &r);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let owner1 = add.iter().find(|n| n.ptr == 0x1000).unwrap();
    let owner2 = add.iter().find(|n| n.ptr == 0x3000).unwrap();
    let late = add.iter().find(|n| n.ptr == 0x4000).unwrap();

    assert!(owner2.edges.contains(&late.id), "recency tie-break attributes the late child to the newer same-name owner, not its true owner");
    assert!(!owner1.edges.contains(&late.id));
}

// dealloc: child.owner becomes None, child.had_owner_once becomes true
#[test]
fn dealloc_orphans_children() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();

    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[owner_site]);
    g.on_alloc(&o_ev, &r);
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[leaf_site, owner_site]);
    g.on_alloc(&c_ev, &r);

    let _ = g.drain_diff(&r);

    g.on_dealloc(0x1000, 500);

    let diff = g.drain_diff(&r);
    let (_, updated, removed) = unwrap_diff(diff);

    assert!(!removed.is_empty(), "owner should be in removed");
    assert!(!updated.is_empty(), "child should be in updated");

    let update_ids: std::collections::HashSet<u64> = updated.iter().map(|n| n.id).collect();
    for &rid in &removed {
        assert!(!update_ids.contains(&rid), "id {rid} must not be in both update and remove");
    }

    let child_node = g.node_by_ptr(0x2000).expect("child still live");
    assert!(child_node.had_owner_once, "child.had_owner_once must be true after owner freed");
    assert!(child_node.owner.is_none(), "child.owner must be None after owner freed");
}

// cascaded dealloc within same tick: remove ∩ update must be disjoint
#[test]
fn cascaded_dealloc_same_tick_disjoint() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();

    let o_ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[owner_site]);
    g.on_alloc(&o_ev, &r);
    let c_ev = make_ev(EventKind::Alloc, 0x2000, 0, 32, 200, &[leaf_site, owner_site]);
    g.on_alloc(&c_ev, &r);

    // No drain between the two deallocs — both within the same tick
    g.on_dealloc(0x2000, 500); // child first
    g.on_dealloc(0x1000, 600); // then owner

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
    let site = 0xAAAA;
    let r = resolver_with_real_sites(&[site]);
    let mut g = OwnershipGraph::new();

    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[site]);
    g.on_alloc(&ev, &r);
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    let original_id = add[0].id;

    // Realloc: old_ptr=0x1000, new_ptr=0x2000, new_size=128
    g.on_realloc(0x1000, 0x2000, 128, &r);

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
    let site = 0xAAAA;
    let r = resolver_with_real_sites(&[site]);
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[site]);
    g.on_alloc(&ev, &r);
    let _ = g.drain_diff(&r);

    let diff2 = g.drain_diff(&r);
    let (add, updated, removed) = unwrap_diff(diff2);
    assert!(add.is_empty() && updated.is_empty() && removed.is_empty(),
        "second drain should be empty");
}

// resolver join: symbol attached at drain_diff time, from the effective site
#[test]
fn drain_diff_attaches_symbol_from_resolver() {
    let mut g = OwnershipGraph::new();
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xAAAA]);
    let r0 = Resolver::new();
    g.on_alloc(&ev, &r0);

    let mut r = Resolver::new();
    r.insert(0xAAAA, "my_alloc_site".to_owned(), false);

    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "my_alloc_site");
}

// resolver fallback: no non-machinery frame found → "?"
#[test]
fn drain_diff_uses_placeholder_for_unresolved_symbol() {
    let mut g = OwnershipGraph::new();
    // 0xDEAD is unknown to the resolver at drain time, so it's treated as
    // machinery and no effective site is found.
    let ev = make_ev(EventKind::Alloc, 0x1000, 0, 64, 100, &[0xDEAD]);
    let r = Resolver::new();
    g.on_alloc(&ev, &r);
    let diff = g.drain_diff(&r);
    let (add, _, _) = unwrap_diff(diff);
    assert_eq!(add[0].symbol, "?");
}

// Targeted invariant test for the node-eviction fix (2026-07-22, daemon
// memory/O(N²) leak): drives a full parent+child lifecycle — alloc parent,
// alloc child (owned), free parent, an orphan transition, free child —
// across four separate ticks, and asserts `self.nodes.len()` at every
// step, not just that it eventually reaches zero. This is what proves
// eviction is *safe* (drain_diff/sweep output at each tick is exactly what
// pre-fix behavior would have produced) rather than merely that memory
// shrinks — a test that only checked the final count could pass even if
// eviction happened too early and silently corrupted a tick's diff.
#[test]
fn dead_nodes_are_evicted_without_changing_diff_or_orphan_detection() {
    let owner_site = 0xAAAA;
    let leaf_site = 0xBBBB;
    let r = resolver_with_real_sites(&[owner_site, leaf_site]);
    let mut g = OwnershipGraph::new();
    let config = Config {
        tau_ms: 5, // 5ms -> 5_000_000ns, matches orphan_persistence.rs's test
        hot_cluster_threshold: 32,
        storm_rate_threshold: 1000,
        storm_window_ms: 1000,
        pipe_name: r"\\.\pipe\heaplens".to_owned(),
        tick_ms: 33,
        ws_addr: "127.0.0.1:9999".to_owned(),
        db_path: "heaplens.db".to_owned(),
    };

    // ── Tick 1: parent and child both born ──────────────────────────────
    g.on_alloc(&make_ev(EventKind::Alloc, 0x1000, 0, 64, 0, &[owner_site]), &r);
    g.on_alloc(&make_ev(EventKind::Alloc, 0x2000, 0, 32, 1, &[leaf_site, owner_site]), &r);
    let parent_id = g.node_by_ptr(0x1000).unwrap().id;
    let child_id = g.node_by_ptr(0x2000).unwrap().id;

    let diff1 = g.drain_diff(&r);
    let (add1, update1, remove1) = unwrap_diff(diff1);
    assert_eq!(add1.len(), 2, "tick 1: both parent and child must be in add");
    assert!(update1.is_empty() && remove1.is_empty());
    assert_eq!(g.nodes().len(), 2, "tick 1: both nodes present after drain, nothing evicted yet");

    // ── Tick 2: parent freed — child orphaned (owner cleared), parent
    //    must be evicted from self.nodes right after this drain, while the
    //    diff itself is byte-identical to what pre-fix behavior produced
    //    (mirrors dealloc_orphans_children's exact assertions). ──────────
    g.on_dealloc(0x1000, 10);
    let diff2 = g.drain_diff(&r);
    let (add2, update2, remove2) = unwrap_diff(diff2);
    assert!(add2.is_empty(), "tick 2: nothing new born");
    assert_eq!(remove2, vec![parent_id], "tick 2: parent must be the only removed id");
    assert_eq!(update2.len(), 1, "tick 2: child must be the only updated node");
    assert_eq!(update2[0].id, child_id);

    assert_eq!(
        g.nodes().len(), 1,
        "tick 2: parent must be evicted from self.nodes right after this drain \
         (this is the actual fix under test) — child must remain (still live)"
    );
    let child = g.node_by_ptr(0x2000).expect("child still live and findable by ptr");
    assert!(child.owner.is_none() && child.had_owner_once, "child correctly orphaned by on_dealloc");
    assert_eq!(child.owner_free_ts, Some(10));

    // ── Tick 3: enough time passes for sweep() to flip the child to
    //    Orphan — proves anomaly detection is unaffected by the parent's
    //    prior eviction (nothing about orphan classification ever needed
    //    to read the dead parent's Node struct — it only reads the live
    //    child's own owner/had_owner_once/ts fields). ────────────────────
    const ORPHAN_DETECTED_TS_NS: u64 = 10 + 6_000_000; // owner_free_ts + tau_ms(5ms) + margin
    g.on_alloc(&make_ev(EventKind::Alloc, 0x3000, 0, 8, ORPHAN_DETECTED_TS_NS, &[0xCCCC]), &r);
    let sentinel_id = g.node_by_ptr(0x3000).unwrap().id;

    let max_ts = g.max_ts_seen;
    let changed = sweep(g.nodes_mut(), max_ts, &config);
    assert_eq!(changed, vec![child_id], "only the child's state should flip, to Orphan");
    assert_eq!(g.nodes().get(&child_id).unwrap().state, NodeState::Orphan);
    g.mark_updated(child_id); // exactly what main.rs's Tick handler does for each changed id

    let diff3 = g.drain_diff(&r);
    let (add3, update3, remove3) = unwrap_diff(diff3);
    assert_eq!(add3.len(), 1, "tick 3: only the sentinel alloc is new");
    assert_eq!(add3[0].id, sentinel_id);
    assert!(remove3.is_empty());
    assert_eq!(update3.len(), 1, "tick 3: child's Orphan transition must be reported");
    assert_eq!(update3[0].id, child_id);
    assert_eq!(update3[0].state, NodeState::Orphan);

    assert_eq!(
        g.nodes().len(), 2,
        "tick 3: child (still live, now Orphan) and the sentinel both present; \
         nothing evicted this tick since nothing died"
    );

    // ── Tick 4: child finally freed too — must also be evicted, proving
    //    the fix applies uniformly, not just to nodes that die alone. ────
    g.on_dealloc(0x2000, ORPHAN_DETECTED_TS_NS + 1);
    let diff4 = g.drain_diff(&r);
    let (add4, update4, remove4) = unwrap_diff(diff4);
    assert!(add4.is_empty() && update4.is_empty());
    assert_eq!(remove4, vec![child_id], "tick 4: child must be the only removed id");

    assert_eq!(
        g.nodes().len(), 1,
        "tick 4: child evicted; only the still-live sentinel remains — \
         the graph does not leak across a full parent+child+orphan lifecycle"
    );
}
