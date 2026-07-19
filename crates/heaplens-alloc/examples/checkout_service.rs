//! `checkout_service` — a stand-in for a real e-commerce checkout backend
//! (payment gateway pooling, an order queue, metrics logging, plain request
//! handling), written to *look* like an ordinary service module rather than
//! a labeled test scenario. Unlike `chaos_orphan`/`chaos_hot`/`chaos_storm`
//! (one flaw each, run once, exit), this program runs forever as a single
//! continuous demo, cycling through all three flaws back-to-back on a fixed
//! 15-second cadence, each in a distinct, realistically-named module:
//!
//!   T+0s   healthy      — `request_handler` serves ordinary requests, no issue.
//!   T+15s  LEAK         — `payment_gateway_pool` drops its pool manager
//!                         while checked-out connections are still held.
//!                         Never cleaned up — a real leak persists forever,
//!                         so these connections stay orphaned across every
//!                         later cycle too (live node count should trend
//!                         upward over the life of the run).
//!   T+30s  HOT CLUSTER   — `order_queue` accepts a backlog that grows past
//!                         a healthy size and keeps growing.
//!   T+45s  ALLOC STORM   — `metrics_flush` bursts a flood of log-write
//!                         allocations far faster than a healthy request
//!                         handler would.
//!   T+60s  → back to healthy, repeat.
//!
//! Every phase transition is announced on stdout so a person watching the
//! console alongside the live graph can see which event is *supposed* to be
//! happening right now and correlate it with what the UI shows.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

use std::time::{Duration, Instant};

const PHASE_MS: u64 = 15_000;

/// Keeps event timestamps advancing during a hold — `heaplens-daemon`'s
/// anomaly age (`max_ts_seen`) only advances via new captured events, never
/// wall-clock, so every phase needs a steady trickle of real allocation
/// traffic even while "waiting." Doubles as `request_handler`'s own
/// healthy-phase workload — matched alloc/dealloc pairs, nothing ever
/// accumulates.
#[inline(never)]
fn request_handler_handle(n: usize) {
    let response = vec![0u8; 96]; // stand-in for a serialized Response body
    std::hint::black_box(&response);
    drop(response);
    let _ = n;
}

fn request_handler_serve_for(duration_ms: u64) {
    let start = Instant::now();
    let mut count = 0usize;
    while start.elapsed() < Duration::from_millis(duration_ms) {
        request_handler_handle(count);
        count += 1;
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// Each checked-out connection — a real one would hold a socket, auth
/// token, and buffers; `128` bytes stands in for that struct.
///
/// The pool manager (`payment_gateway_pool_initialize`, below) is allocated
/// directly in `main`, deliberately not inside its own helper function:
/// phi links a child to an owner by matching the owner's effective call
/// site against the *ancestor frames* of the child's own captured stack.
/// A helper function that allocates the manager and returns would never
/// appear in a later, separately-called connection's stack at all — phi
/// would find no owner to link to, not "no longer owned" (which is what a
/// leak needs to demonstrate to be visible). Allocating the manager
/// directly in `main`, and connections through this distinct helper
/// (called from `main` too), means each connection's stack is
/// `[.., payment_gateway_pool_checkout_connections, main, ..]` — `main` is
/// an ancestor, and `main` is exactly the manager's own effective site.
/// This is the same shape `chaos_orphan.rs`'s `owner`/`make_children`
/// split uses, and `order_queue_accept_orders` below mirrors it too.
#[inline(never)]
fn payment_gateway_pool_checkout_connections(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|_| vec![0u8; 128]).collect()
}

/// A real order — a real one would hold line items, a customer id,
/// shipping address; `96` bytes stands in for that struct.
#[inline(never)]
fn order_queue_accept_orders(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|_| vec![0u8; 96]).collect()
}

/// A single metrics log entry — small, high-frequency, exactly the shape
/// that turns into a storm when something upstream (a retry loop, a bug in
/// a batching layer) stops throttling how often it fires.
#[inline(never)]
fn metrics_flush_write_entry(n: usize) {
    let entry = vec![0u8; 24];
    std::hint::black_box(&entry);
    drop(entry);
    let _ = n;
}

fn main() {
    println!("[checkout_service] starting — request_handler online, payment_gateway_pool warm, order_queue idle");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Connections leaked by payment_gateway_pool persist across every
    // cycle, deliberately never freed — see the module doc comment.
    let mut leaked_connections: Vec<Vec<u8>> = Vec::new();

    loop {
        // ── T+0s: healthy ──────────────────────────────────────────────
        println!("=== EVENT: nominal — request_handler serving traffic normally ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        request_handler_serve_for(PHASE_MS);

        // ── T+15s: LEAK — pool manager dropped while connections live ──
        println!("=== EVENT: FLAW — payment_gateway_pool manager torn down while 12 connections still checked out (leak) ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let pool_manager = vec![0u8; 256]; // PoolManager { connections: Vec<Connection>, retry_policy, tls_config, .. }
        let mut connections = payment_gateway_pool_checkout_connections(12);
        request_handler_serve_for(2_000); // manager and connections visibly live together first
        drop(pool_manager); // the bug: manager torn down, connections never released
        println!("    payment_gateway_pool: manager freed — {} connections now orphaned and will never be released", connections.len());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        leaked_connections.append(&mut connections);
        request_handler_serve_for(PHASE_MS - 2_000);

        // ── T+30s: HOT CLUSTER — order backlog grows unbounded ─────────
        println!("=== EVENT: FLAW — order_queue backlog growing past healthy size (hot cluster) ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let queue_owner = vec![0u8; 512]; // stand-in for OrderQueue { backlog: Vec<Order>, .. }
        let mut backlog = order_queue_accept_orders(10); // still under the healthy threshold
        request_handler_serve_for(3_000);
        backlog.extend(order_queue_accept_orders(30)); // now well past it
        println!("    order_queue: backlog at {} orders and still growing", backlog.len());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        request_handler_serve_for(PHASE_MS - 3_000);
        // Recovered: the backlog gets processed and drained, unlike the leak above.
        drop(backlog);
        drop(queue_owner);
        println!("    order_queue: backlog drained, back to a healthy depth");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        // ── T+45s: ALLOC STORM — metrics flush loses its throttle ──────
        println!("=== EVENT: FLAW — metrics_flush bursting log writes far above the healthy rate (allocation storm) ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let storm_start = Instant::now();
        let mut burst = 0usize;
        while storm_start.elapsed() < Duration::from_millis(PHASE_MS) {
            // A burst of unthrottled writes, then a brief pause before the
            // next burst — keeps the storm visibly "ongoing" for the whole
            // phase rather than a single instantaneous spike that's easy to
            // miss.
            for i in 0..2_000 {
                metrics_flush_write_entry(i);
            }
            burst += 1;
            std::thread::sleep(Duration::from_millis(1_500));
        }
        println!("    metrics_flush: {burst} unthrottled bursts written this phase");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        println!(
            "=== cycle complete — {} connections leaked so far and counting; repeating ===",
            leaked_connections.len()
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
}
