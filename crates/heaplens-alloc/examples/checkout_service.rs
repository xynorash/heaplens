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

// Shared with checkout_service_tui.rs (and checkout_service_gui.rs, parked)
// — see checkout_common.rs's own module doc comment for why the event
// functions below are safe to share verbatim across every target
// regardless of each target's own control flow.
#[path = "support/checkout_common.rs"]
mod checkout_common;
use checkout_common::{
    hot_cluster_heal, hot_cluster_tick, leak_pool_tick, request_handler_handle, storm_burst,
};

const PHASE_MS: u64 = 30_000;

/// Keeps event timestamps advancing during a hold — `heaplens-daemon`'s
/// anomaly age (`max_ts_seen`) only advances via new captured events, never
/// wall-clock, so every phase needs a steady trickle of real allocation
/// traffic even while "waiting."
fn request_handler_serve_for(duration_ms: u64) {
    let start = Instant::now();
    let mut count = 0usize;
    while start.elapsed() < Duration::from_millis(duration_ms) {
        request_handler_handle(count);
        count += 1;
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn main() {
    println!("[checkout_service] starting — request_handler online, payment_gateway_pool warm, order_queue idle");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Connections leaked by payment_gateway_pool persist across every
    // cycle, deliberately never freed — see the module doc comment.
    let mut leaked_connections: Vec<Vec<u8>> = Vec::new();

    // Recovered order_queue owners/backlog remnants, kept alive (never
    // dropped) after each hot-cluster phase heals. Unlike the leak above,
    // this is not a bug — it's what lets the recovery be *visible* as a
    // "back to healthy" family in the graph rather than the owner and its
    // children vanishing outright the instant the phase ends, which reads
    // identically to "nothing was ever here" rather than "this grew, then
    // came back under control."
    let mut healthy_queue_owners: Vec<Vec<u8>> = Vec::new();
    let mut healthy_backlog_remnants: Vec<Vec<u8>> = Vec::new();

    loop {
        // ── T+0s: healthy ──────────────────────────────────────────────
        println!("=== EVENT: nominal — request_handler serving traffic normally ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        request_handler_serve_for(PHASE_MS);

        // ── T+15s: LEAK — pool manager dropped while connections live ──
        println!("=== EVENT: FLAW — payment_gateway_pool manager torn down while 12 connections still checked out (leak) ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut pool_manager: Option<Vec<u8>> = None;
        let mut pending_connections: Option<Vec<Vec<u8>>> = None;
        leak_pool_tick(&mut pool_manager, &mut pending_connections, false);
        request_handler_serve_for(2_000); // manager and connections visibly live together first
        leak_pool_tick(&mut pool_manager, &mut pending_connections, true); // the bug
        let mut connections = pending_connections.take().unwrap_or_default();
        println!("    payment_gateway_pool: manager freed — {} connections now orphaned and will never be released", connections.len());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        leaked_connections.append(&mut connections);
        request_handler_serve_for(PHASE_MS - 2_000);

        // ── T+30s: HOT CLUSTER — order backlog grows unbounded ─────────
        println!("=== EVENT: FLAW — order_queue backlog growing past healthy size (hot cluster) ===");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let mut queue_owner: Option<Vec<u8>> = None;
        let mut backlog: Vec<Vec<u8>> = Vec::new();
        hot_cluster_tick(&mut queue_owner, &mut backlog, false); // still under the healthy threshold
        request_handler_serve_for(3_000);
        hot_cluster_tick(&mut queue_owner, &mut backlog, true); // now well past it
        println!("    order_queue: backlog at {} orders and still growing", backlog.len());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        request_handler_serve_for(PHASE_MS - 3_000);
        // Recovered: processed down to a healthy depth, unlike the leak
        // above — but "recovered" means the family is still there, just
        // small and no longer growing, not that it vanished.
        let remnant_total = hot_cluster_heal(
            &mut queue_owner,
            &mut backlog,
            &mut healthy_queue_owners,
            &mut healthy_backlog_remnants,
        );
        println!("    order_queue: backlog drained, back to a healthy depth ({remnant_total} orders kept alive across all cycles)");
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
            storm_burst(2_000);
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
