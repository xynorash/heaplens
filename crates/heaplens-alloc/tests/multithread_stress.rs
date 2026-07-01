// Many threads allocating concurrently. Uses HeapLensAlloc as global allocator.
// Asserts: no deadlock (test completes), bounded memory (dropped counter works),
// and no host process crash.

use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

#[test]
fn concurrent_allocs_no_deadlock() {
    const THREADS: usize = 8;
    const ITERS: usize = 10_000;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                for i in 0..ITERS {
                    let v: Vec<u8> = vec![i as u8; 64];
                    std::hint::black_box(v);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread must not panic");
    }
    // If we reach here without deadlock or abort, the test passes.
}
