//! Shared allocation sites for `checkout_service.rs` (console) and
//! `checkout_service_gui.rs` (iced GUI demo) — included via `#[path]` by
//! both binaries, not a separate crate. This is the one part of either
//! target that's already proven to produce correct φ attribution; **do
//! not change allocation shape, frame structure, or timing here** without
//! re-validating against the wire test (see the module comment on
//! `checkout_service.rs` and `checkout_service_gui.rs`'s own validation
//! notes).
//!
//! Every function here is `#[inline(never)]` for the same reason: phi
//! links a child to an owner by matching the owner's effective call site
//! against the *ancestor frames* of the child's own captured stack, and an
//! inlined function never contributes its own frame to that match. The
//! owner allocations themselves (pool manager, queue owner) are *not*
//! defined here — they must stay directly in each binary's own top-level
//! calling frame (`main`, or the GUI's update-loop equivalent), exactly as
//! `checkout_service.rs`'s own doc comment on
//! `payment_gateway_pool_checkout_connections` explains. Moving them here
//! would put them in a frame neither binary's `main`/update-loop calls
//! from directly, breaking that ancestry match.

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
