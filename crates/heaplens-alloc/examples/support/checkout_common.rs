//! Shared allocation sites AND shared event logic for `checkout_service.rs`
//! (console), `checkout_service_tui.rs` (ratatui TUI), and
//! `checkout_service_gui.rs` (iced GUI demo, parked) — included via
//! `#[path]` by each binary, not a separate crate. This is the one part of
//! any target that's already proven to produce correct φ attribution; **do
//! not change allocation shape, frame structure, or timing here** without
//! re-validating against the wire test.
//!
//! Every function here is `#[inline(never)]` for the same reason: phi
//! links a child to an owner by matching the owner's effective call site
//! against the *ancestor frames* of the child's own captured stack, and an
//! inlined function never contributes its own frame to that match.
//!
//! The event-trigger functions below (`leak_pool_tick`, `hot_cluster_tick`,
//! `hot_cluster_heal`, `storm_burst`) own BOTH the owner allocation and its
//! children's allocation calls, in the same function — this is what makes
//! them safe to share across every target regardless of that target's own
//! control flow (a blocking loop for the console, an event-driven tick for
//! the TUI): phi only cares that an owner and its children share the same
//! *immediate calling frame*, and since that frame is now this shared
//! function itself, it stays consistent no matter which binary calls it,
//! or how many times. Do not split an owner's allocation from its
//! children's into two separate functions — see `hot_cluster_tick`'s own
//! doc comment for why the "start" and "grow" calls must stay one function
//! called twice, not two functions.

/// Stand-in for a serialized Response body. Doubles as ordinary healthy
/// traffic in both binaries — matched alloc/dealloc pairs, nothing ever
/// accumulates.
#[inline(never)]
pub fn request_handler_handle(n: usize) {
    let response = vec![0u8; 96];
    std::hint::black_box(&response);
    drop(response);
    let _ = n;
}

/// Each checked-out connection — a real one would hold a socket, auth
/// token, and buffers; `128` bytes stands in for that struct. Caller must
/// allocate the pool manager directly in its own frame first — see the
/// module doc comment.
#[inline(never)]
pub fn payment_gateway_pool_checkout_connections(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|_| vec![0u8; 128]).collect()
}

/// A real order — a real one would hold line items, a customer id,
/// shipping address; `96` bytes stands in for that struct. Caller must
/// allocate the queue owner directly in its own frame first — see the
/// module doc comment.
#[inline(never)]
pub fn order_queue_accept_orders(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|_| vec![0u8; 96]).collect()
}

/// A single metrics log entry — small, high-frequency, exactly the shape
/// that turns into a storm when something upstream stops throttling how
/// often it fires.
#[inline(never)]
pub fn metrics_flush_write_entry(n: usize) {
    let entry = vec![0u8; 24];
    std::hint::black_box(&entry);
    drop(entry);
    let _ = n;
}

/// Leak trigger, shared by every target. Call once with `release: false` to
/// check out the pool manager and its 12 connections together (so both
/// share this function as their φ-owner call site); call again later with
/// `release: true` to drop the manager while the connections are still
/// held — the bug. The caller owns storing/moving the connections
/// afterward (into a permanent leaked-connections accumulator); this
/// function only performs the allocation/drop pair itself.
#[inline(never)]
pub fn leak_pool_tick(
    pool_manager: &mut Option<Vec<u8>>,
    pending_connections: &mut Option<Vec<Vec<u8>>>,
    release: bool,
) {
    if !release {
        *pool_manager = Some(vec![0u8; 256]);
        *pending_connections = Some(payment_gateway_pool_checkout_connections(12));
    } else {
        *pool_manager = None; // the bug: manager torn down, connections never released
    }
}

/// Hot-cluster growth trigger, shared by every target. Call once with
/// `add_extra: false` to allocate the queue owner and its initial 10-order
/// batch (only if not already owned); call again later with
/// `add_extra: true` to add the +30 batch that pushes it past the healthy
/// threshold. Both calls MUST go through this same function — the initial
/// batch and the "+30 more" batch are two separate allocation calls, and
/// phi only credits the queue owner with a child allocation if this
/// function's own frame is an ancestor of that child's captured stack.
/// Splitting "start" and "grow" into two different functions would break
/// that: the +30 batch's ancestor chain would then go through the *other*
/// function's frame, which is not the queue owner's own effective site,
/// and phi would never link it back.
///
/// `backlog.reserve(40)` happens *before* `queue_owner` is allocated, not
/// after — matching `hot_producer.rs`'s own documented safe ordering
/// ("children's own backing storage must be allocated before owner, not
/// after"). `backlog` starts empty (`Vec::new()`), so without this
/// upfront reserve, its first `.extend()` call would grow its own backing
/// array *from inside this same function* — the same effective site as
/// `queue_owner` — making it a more-recently-allocated same-site
/// candidate than `queue_owner` by the time the second (`add_extra`)
/// batch arrives. Phi's recency tie-break would then attribute that
/// batch to backlog's own backing array instead of `queue_owner`,
/// splitting the 40 total children roughly 10/30 across two different
/// nodes, neither of which crosses `hot_cluster_threshold` (32) alone —
/// confirmed as the actual cause of `queue_owner` never flipping Hot in
/// the graph despite the console correctly logging "40 orders and still
/// growing." Reserving the full capacity upfront means every later
/// `.extend()` fits in already-allocated space, so no such reallocation
/// — and no such competing candidate — ever occurs.
#[inline(never)]
pub fn hot_cluster_tick(queue_owner: &mut Option<Vec<u8>>, backlog: &mut Vec<Vec<u8>>, add_extra: bool) {
    if queue_owner.is_none() {
        backlog.reserve(40);
        *queue_owner = Some(vec![0u8; 512]);
        backlog.extend(order_queue_accept_orders(10));
    }
    if add_extra {
        backlog.extend(order_queue_accept_orders(30));
    }
}

/// Hot-cluster recovery, shared by every target. Frees the backlog down to
/// a small healthy depth and moves the owner plus the remaining orders
/// into the caller's permanent "healthy" accumulators, rather than
/// dropping everything — a family that fully vanishes the instant it
/// heals reads identically to "nothing was ever here," not "this grew,
/// then came back under control." Returns the new total remnant count
/// (across every heal so far) for the caller's own logging. No new
/// allocations happen here, so this needs no owner/child call-site care —
/// it's a plain free/move operation.
pub fn hot_cluster_heal(
    queue_owner: &mut Option<Vec<u8>>,
    backlog: &mut Vec<Vec<u8>>,
    healthy_owners: &mut Vec<Vec<u8>>,
    healthy_remnants: &mut Vec<Vec<u8>>,
) -> usize {
    backlog.truncate(3);
    healthy_remnants.append(backlog);
    if let Some(owner) = queue_owner.take() {
        healthy_owners.push(owner);
    }
    healthy_remnants.len()
}

/// Alloc-storm burst, shared by every target: `n` unthrottled metrics-flush
/// writes back to back.
#[inline(never)]
pub fn storm_burst(n: usize) {
    for i in 0..n {
        metrics_flush_write_entry(i);
    }
}
