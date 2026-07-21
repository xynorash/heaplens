use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

/// Allocates at a leaf call site whose captured stack includes the caller's
/// frame — see `wire_producer.rs`'s `nested_alloc` for why this shape lets
/// phi ownership inference attach a child to its owner.
#[inline(never)]
fn leaf_alloc(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// One owner allocation, followed by `n_children` leaf allocations from the
/// same call site inside this frame — phi should infer all of them as owned
/// by `owner`.
///
/// The `children` container's own backing storage (`Vec::with_capacity`)
/// must be allocated *before* `owner`, not after: it allocates at this same
/// call site (`make_family`), so it is also a same-symbol candidate phi
/// considers when attributing each child. Phi's tie-break picks the most
/// recent same-symbol candidate — if `Vec::with_capacity` ran after `owner`,
/// it would win that tie-break instead of `owner`, and every child would be
/// (mis)attributed to the children container itself rather than to `owner`.
/// Since that container isn't freed until `children` is dropped — at the
/// same time as the children themselves — the owner-freed/children-orphaned
/// transition this scenario exists to demonstrate would never actually be
/// observable: everything would appear to die together in one bulk removal
/// instead of orphaning at T+60s. Allocating it first gives `owner` the
/// later timestamp, so it correctly wins the tie-break.
#[inline(never)]
fn make_family(n_children: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let mut children = Vec::with_capacity(n_children);
    let owner = vec![0u8; 4096];
    for _ in 0..n_children {
        children.push(leaf_alloc(128));
    }
    (owner, children)
}

/// Keeps event timestamps advancing during a hold period. Anomaly age
/// (`max_ts_seen` in heaplens-daemon) is computed from event timestamps
/// only, never wall-clock — a hold period with zero new events never
/// accrues age no matter how much real time passes (discovered during the
/// M5 Task 10 live run). A steady trickle of tiny, immediately-freed
/// allocations keeps the event stream alive so the daemon's anomaly sweep
/// actually has timestamps to compare `tau_ms` against.
fn heartbeat(seconds: u64) {
    let ticks = seconds * 2; // one heartbeat every 500ms
    for _ in 0..ticks {
        let buf = vec![0u8; 16];
        std::thread::sleep(std::time::Duration::from_millis(500));
        drop(buf);
    }
}

/// Slow, watchable scenario for the M5 canvas visual-verification gate (and
/// the thesis demo): one long-lived owner with ~20 children, held alive for
/// a full minute (enough time for a human to actually look at the screen),
/// then the owner is freed — orphaning the children, which should persist
/// on screen (coral, per `NodeStateDto.orphan`) rather than disappearing
/// with it. Unlike `wire_producer.rs` (which allocates and frees everything
/// within ~2 seconds, purely to exercise the wire format), this scenario is
/// designed to still be on screen when a human checks it.
///
/// This is the soutenance/thesis demo scenario. Timeline maps directly to
/// the demo script: T+0s family allocated (owner + 20 children appear);
/// T+60s owner freed (children flip to coral/orphan — confirmed live,
/// fix/canvas-render investigation); T+80s children freed (fade out, ~1s).
fn main() {
    println!("demo_producer: allocating 1 owner + 20 children");
    let (owner, children) = make_family(20);
    println!(
        "holding for 60s (owner + {} children all live) — heartbeat every 500ms \
         to keep max_ts_seen advancing",
        children.len()
    );
    heartbeat(60);

    println!("freeing owner — remaining children should become orphans");
    drop(owner);
    println!("holding orphaned children for 20s for visual confirmation");
    heartbeat(20);

    println!("freeing children");
    drop(children);

    // Wait for the writer thread's flush interval to drain the ring.
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("demo_producer: done");
}
