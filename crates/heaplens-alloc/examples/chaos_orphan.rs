// Chaos scenario: orphan detection. Owner allocated first (so φ's
// ownership inference, which only considers already-live nodes, can
// assign it), then children, then the owner alone is freed while children
// stay live. Children should flip to Orphan once tau_ms elapses.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

use std::time::{Duration, Instant};

#[inline(never)]
fn make_children() -> Vec<Box<[u8; 64]>> {
    (0..5).map(|_| Box::new([0u8; 64])).collect()
}

fn main() {
    let owner = Box::new(0u8);
    let owner_ptr: *mut u8 = Box::into_raw(owner);
    println!("OWNER_PTR=0x{:x}", owner_ptr as u64);
    println!("CHILD_SYMBOL=chaos_orphan::make_children");

    let children = make_children();

    // Healthy hold: owner + children all live, nothing orphaned yet — a
    // human (or a screenshot) watching the graph should see a plain healthy
    // star for a full 15s before anything changes. Ticks every 20ms (not a
    // single sleep) to keep max_ts_seen advancing — anomaly age is computed
    // from event timestamps only, never wall-clock (see demo_producer.rs).
    let healthy_start = Instant::now();
    while healthy_start.elapsed() < Duration::from_millis(15_000) {
        let hb = vec![0u8; 8];
        std::hint::black_box(&hb);
        drop(hb);
        std::thread::sleep(Duration::from_millis(20));
    }

    // SAFETY: owner_ptr came from Box::into_raw immediately above and is
    // freed exactly once, here.
    unsafe {
        drop(Box::from_raw(owner_ptr));
    }
    std::mem::forget(children);

    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(7000) {
        let hb = vec![0u8; 8];
        std::hint::black_box(&hb);
        drop(hb);
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("DONE");
}
