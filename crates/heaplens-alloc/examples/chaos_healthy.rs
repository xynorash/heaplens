// Chaos scenario: healthy baseline. Matched alloc/dealloc pairs, no
// leaks, no large clusters, no storm. Nothing should ever anomaly-flip.
use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

fn main() {
    for _ in 0..20 {
        let v = vec![0u8; 64];
        std::hint::black_box(&v);
        drop(v);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("DONE");
}
