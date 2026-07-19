// Chaos scenario: hot-cluster detection. Owner allocated first, then more
// than hot_cluster_threshold (default 32) children, owner never freed.
// Owner's node should flip to Hot (edges_out.len() > 32).
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

use std::time::{Duration, Instant};

#[inline(never)]
fn make_children(n: usize) -> Vec<Box<[u8; 32]>> {
    (0..n).map(|_| Box::new([0u8; 32])).collect()
}

fn main() {
    let owner = Box::new(0u8);
    println!("OWNER_SYMBOL=chaos_hot::main");

    // Healthy hold: 10 children (under the default 32 threshold) for a full
    // 15s, so a human (or a screenshot) sees a plain healthy star before the
    // cluster grows into Hot. Ticks every 20ms rather than a single sleep to
    // keep max_ts_seen advancing.
    let mut children = make_children(10);
    let healthy_start = Instant::now();
    while healthy_start.elapsed() < Duration::from_millis(15_000) {
        let hb = vec![0u8; 8];
        std::hint::black_box(&hb);
        drop(hb);
        std::thread::sleep(Duration::from_millis(20));
    }

    // Grow past the threshold (10 + 30 = 40 > 32) — owner should flip Hot.
    children.extend(make_children(30));
    std::mem::forget(children);

    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(3000) {
        let hb = vec![0u8; 8];
        std::hint::black_box(&hb);
        drop(hb);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::mem::forget(owner);
    println!("DONE");
}
