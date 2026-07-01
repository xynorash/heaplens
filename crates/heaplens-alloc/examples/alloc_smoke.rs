// Smoke test: run with no daemon. The writer retries the pipe connection in
// the background. The host process must not crash, hang, or abort.
// Events are produced into the ring and dropped when it fills.

use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

fn main() {
    let rounds = 1_000;
    for i in 0..rounds {
        // Allocate and immediately drop a variety of sizes.
        let _s: String = format!("hello-{i}");
        let _v: Vec<u64> = (0..16).collect();
        let _b: Box<[u8; 128]> = Box::new([i as u8; 128]);
    }
    println!("alloc_smoke: {rounds} rounds completed — no crash, no hang.");
}
