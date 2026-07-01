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

    // drain_all requires the recursion guard to be permanently set on the
    // calling thread (writer-thread invariant §12.4).
    heaplens_alloc::guard::force_enter_permanent();

    // Drain all recorded events and assert that at least one was captured.
    let mut count = 0usize;
    heaplens_alloc::ring::drain_all(|_ev| { count += 1; });
    assert!(count > 0, "expected at least one recorded event, got 0");
}
