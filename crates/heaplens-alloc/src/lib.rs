mod guard;
mod ring;
mod capture;
mod writer;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Once;
use heaplens_protocol::EventKind;
use heaplens_protocol::AllocEvent;

/// A `#[global_allocator]` that intercepts every (de/re)allocation and ships
/// raw `AllocEvent` records off-process via a named pipe, without blocking
/// or allocating on the hot path.
///
/// # Usage
/// ```no_run
/// use heaplens_alloc::HeapLensAlloc;
/// #[global_allocator]
/// static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();
/// ```
///
/// Activate before the program's first allocation. The background writer thread
/// is spawned lazily on first record; it permanently holds the recursion guard
/// so none of its own allocations are ever recorded.
pub struct HeapLensAlloc;

impl HeapLensAlloc {
    pub const fn new() -> Self { HeapLensAlloc }
}

static WRITER_ONCE: Once = Once::new();

/// Spawn the writer thread exactly once. Safe to call from the hot path:
/// after the first successful call_once, subsequent calls are a single
/// atomic load (no allocation, no blocking).
///
/// The spawn itself allocates ("heaplens-writer" thread name string), but it
/// runs under the recursion guard, so those allocations are suppressed.
#[inline]
fn ensure_writer() {
    WRITER_ONCE.call_once(|| {
        // Errors here are unrecoverable but must not panic the host.
        // If spawn fails (e.g., out of threads), events silently pile up
        // in the ring until it fills, after which they are dropped.
        let _ = std::thread::Builder::new()
            .name("heaplens-writer".to_owned())
            .spawn(writer::run);
    });
}

/// Record one allocation event. Hot path.
///
/// Invariants enforced here:
/// 1. Guard checked first — re-entrant calls return immediately.
/// 2. Guard set via RAII ScopedGuard — cleared even on panic.
/// 3. No allocation, no locking.
/// 4. ring::push failure (full ring) is a silent drop.
#[inline]
fn record(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, align: u32) {
    // 1. Re-entrancy check — must be the very first thing.
    if guard::is_set() { return; }

    // 2. Acquire guard (panic-safe RAII).
    let _g = guard::ScopedGuard::enter();

    // 3. Timestamp (no alloc).
    let ts = capture::timestamp_nanos();

    // 4. Raw stack capture (no alloc; unsafe justified in capture.rs).
    let (stack, stack_len) = capture::capture_stack();

    // 5. Construct event (no alloc; enforces _pad = [0,0] via AllocEvent::new).
    let ev = AllocEvent::new(kind, ptr, old_ptr, size, align, ts, stack, stack_len);

    // 6. Push to per-thread ring (lock-free, no alloc after TLS init).
    //    Returns false if full — event is silently dropped.
    ring::push(ev);

    // 7. Ensure writer thread is running. Safe under guard: any allocations
    //    inside call_once are suppressed by the guard.
    ensure_writer();

    // _g drops here, clearing the guard even if earlier steps panicked.
}

unsafe impl GlobalAlloc for HeapLensAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if ptr.is_null() {
            return ptr;
        }
        record(EventKind::Alloc, ptr as u64, 0, layout.size() as u64, layout.align() as u32);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(EventKind::Dealloc, ptr as u64, 0, layout.size() as u64, layout.align() as u32);
        System.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        if new_ptr.is_null() {
            return new_ptr;
        }
        record(
            EventKind::Realloc,
            new_ptr as u64,
            ptr as u64,
            new_size as u64,
            layout.align() as u32,
        );
        new_ptr
    }
}
