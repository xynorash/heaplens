use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

/// Allocates at a leaf call site whose captured stack includes the caller's
/// frame — see `demo_producer.rs`'s `make_family` for the same shape used to
/// build a working owner/children star (confirmed live during the
/// fix/canvas-render investigation: phi correctly attaches every child
/// allocated this way to the one owner still on the stack).
#[inline(never)]
fn leaf_alloc(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// One owner allocation, followed by `n_children` leaf allocations from the
/// same call site inside this frame. `heaplens-daemon`'s Hot classification
/// (`anomaly.rs`: `is_hot = node.edges_out.len() > config.hot_cluster_threshold`,
/// default threshold 32) is purely structural — no time window, no waiting
/// on `tau_ms` — so the owner should flip to Hot (amber) as soon as phi has
/// attached all `n_children` to it, well before the hold period ends.
///
/// As in `demo_producer.rs`'s `make_family`, `children`'s own backing
/// storage (`Vec::with_capacity`) must be allocated *before* `owner`: it
/// allocates at this same call site, so it is also a same-symbol candidate
/// phi's recency tie-break considers for each child. Allocated after
/// `owner`, it would win that tie-break and every child would attach to the
/// children container instead of to `owner` — the container would flip Hot,
/// not the node actually meant to be observed.
#[inline(never)]
fn make_star(n_children: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut children = Vec::with_capacity(n_children);
    let owner = vec![0u8; 4096];
    for _ in 0..n_children {
        children.push(leaf_alloc(128));
    }
    (owner, children)
}

/// Keeps event timestamps advancing during the hold — see `demo_producer.rs`
/// for why (`max_ts_seen` only advances via new events, never wall-clock).
/// Not strictly required for Hot (whose predicate has no time component,
/// unlike Orphan's `tau_ms` wait), but kept for consistency with the other
/// producers and so the daemon's WS broadcast doesn't go fully idle for 30s.
fn heartbeat(seconds: u64) {
    let ticks = seconds * 2; // one heartbeat every 500ms
    for _ in 0..ticks {
        let buf = vec![0u8; 16];
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(buf);
    }
}

/// Slow, watchable scenario closing the M5/M6 Hot visual-verification gate:
/// one owner with 40 children (safely above the default hot_cluster_threshold
/// of 32) held alive for 30s so a human has time to see the owner render
/// amber. Deliberately never frees the owner mid-hold: `anomaly.rs`'s sweep
/// checks Orphan before Hot and Orphan wins if both would match, so freeing
/// the owner early would make its children eligible for Orphan instead of
/// letting the owner sit and be observed as Hot.
///
/// Distinct from `leak_unbounded.rs` (the fast, sleep-free H2 benchmark
/// workload with the same underlying shape) — this file exists purely for
/// human visual confirmation and the demo, not for timing measurement.
fn main() {
    println!("hot_producer: allocating 1 owner + 40 children (owner should flip to Hot/amber)");
    let (owner, children) = make_star(40);
    println!(
        "holding for 30s ({} children live off one owner) for visual confirmation",
        children.len()
    );
    heartbeat(30);

    println!("freeing owner and children");
    drop(owner);
    drop(children);

    // Wait for the writer thread's flush interval to drain the ring.
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("hot_producer: done");
}
