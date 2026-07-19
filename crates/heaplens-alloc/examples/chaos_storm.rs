// Chaos scenario: allocation storm. A single call site allocates far
// faster than storm_rate_threshold (default 1000/sec) within
// storm_window_ms (default 1000ms). StormTracker::record does not set
// NodeState (see anomaly.rs) — it's logged via `warn!` in main.rs's graph
// loop. This scenario is verified by grepping the daemon's own log output
// for "allocation storm", not via WS/NodeState.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

#[inline(never)]
fn storm_alloc(n: usize) {
    let v = vec![0u8; 16];
    std::hint::black_box(&v);
    drop(v);
    let _ = n;
}

fn main() {
    // Healthy hold: the same call site, but at a slow, sparse rate (well
    // under the default 1000/sec threshold) for a full 15s, so a human (or
    // a screenshot) sees ordinary healthy activity before the storm hits.
    let healthy_start = std::time::Instant::now();
    let mut i = 0usize;
    while healthy_start.elapsed() < std::time::Duration::from_millis(15_000) {
        storm_alloc(i);
        i += 1;
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 2000 allocations at one call site, no sleeps — far exceeds the
    // default 1000/sec threshold within the default 1000ms window.
    for i in 0..2000 {
        storm_alloc(i);
    }

    // The alloc loop itself finishes in well under a millisecond, faster
    // than the writer thread's own flush cycle — without this, the
    // process (and its pipe connection) can exit before most of the 2000
    // events are ever sent, so the daemon never accumulates enough events
    // at this site to cross the storm threshold. Confirmed directly: a
    // manual run without this sleep showed the pipe connect+disconnect
    // ~4ms apart with zero storm warning logged. Same root cause as H2's
    // writer-teardown finding (writer isn't joined on exit).
    std::thread::sleep(std::time::Duration::from_millis(500));
    println!("DONE");
}
