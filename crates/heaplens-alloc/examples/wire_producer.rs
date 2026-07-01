use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

/// Allocates at a leaf call site. `#[inline(never)]` ensures this frame
/// appears in the stack captured by the global allocator, giving φ inference
/// a distinct stack[0] to match against the caller's allocation.
#[inline(never)]
fn leaf_alloc(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// Allocates an outer buffer, then calls leaf_alloc. Because leaf_alloc's
/// captured stack includes this frame's PC, φ should infer that the outer
/// buffer owns the inner one — assuming the outer alloc happened first and
/// its stack[0] appears somewhere in leaf_alloc's stack.
#[inline(never)]
fn nested_alloc(n: usize) -> (Vec<u8>, Vec<u8>) {
    let outer = vec![0u8; n];       // outer: stack[0] = nested_alloc's frame
    let inner = leaf_alloc(n / 2);  // inner: stack includes nested_alloc's frame
    (outer, inner)
}

fn main() {
    // Perform > 64 allocs to force at least one count-triggered flush (batch cap = 64).
    // 100 × nested_alloc = 200 Box/Vec allocs + Vec reallocations ≫ 64 events.
    let mut items: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(100);
    for _ in 0..100 {
        items.push(nested_alloc(128));
    }

    // Hold briefly, then drop to generate dealloc events.
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(items);

    // Wait for the writer thread's 1ms flush interval to drain the ring.
    // The writer retries at 100µs; 500ms is >> the maximum latency.
    std::thread::sleep(std::time::Duration::from_millis(500));
}
