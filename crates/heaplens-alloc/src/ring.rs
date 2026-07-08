use std::alloc::{handle_alloc_error, GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use heaplens_protocol::AllocEvent;

/// Ring capacity — must be a power of two.
pub const CAP: usize = 65_536;

/// SPSC lock-free ring buffer. Backing store is allocated via `System`
/// directly (invariant §12.5). The producer owns `tail`; the consumer owns `head`.
pub struct Ring {
    slots: *mut AllocEvent,
    head: AtomicUsize,
    tail: AtomicUsize,
    pub dropped: AtomicU64,
    pub producer_alive: AtomicBool,
}

// SAFETY: The SPSC protocol guarantees that only the producer writes `tail`
// and only the consumer writes `head`. No two threads touch the same slot
// simultaneously. Raw pointer is therefore safe to send/share across threads.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub fn new() -> Self {
        let layout = Layout::array::<AllocEvent>(CAP).expect("ring layout");
        // SAFETY: layout is non-zero. We check for null (OOM) below.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Ring {
            slots: ptr as *mut AllocEvent,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            producer_alive: AtomicBool::new(true),
        }
    }

    /// Producer side (hot path). Returns false if ring is full (event dropped).
    /// Never allocates, never blocks.
    #[inline]
    pub fn push(&self, ev: AllocEvent) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) & (CAP - 1);
        if next == self.head.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: `tail < CAP`; slots is a valid array of CAP elements.
        // The producer owns slot[tail]; the consumer won't read it until
        // the tail store (Release) below makes the write visible.
        unsafe { self.slots.add(tail).write(ev) };
        self.tail.store(next, Ordering::Release);
        true
    }

    /// Consumer side (writer thread only). Returns None when ring is empty.
    pub fn pop(&self) -> Option<AllocEvent> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `head < CAP`; the producer wrote slot[head] before
        // advancing tail (Release), and we observed that advance (Acquire).
        let ev = unsafe { self.slots.add(head).read() };
        self.head.store((head + 1) & (CAP - 1), Ordering::Release);
        Some(ev)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let layout = Layout::array::<AllocEvent>(CAP).expect("ring layout");
        // SAFETY: `self.slots` was allocated with this exact layout.
        unsafe { System.dealloc(self.slots as *mut u8, layout) };
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// All live rings. The writer drains these; producers register at TLS init.
/// A Mutex here is acceptable — producers never touch the registry on the
/// hot path; registration happens once per thread at first `push`.
static REGISTRY: OnceLock<Mutex<Vec<Arc<Ring>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Arc<Ring>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// RAII handle held in TLS. On thread exit (Drop), marks the ring
/// producer-dead so the writer knows it can remove the ring after draining.
struct RingHandle(Arc<Ring>);

impl Drop for RingHandle {
    fn drop(&mut self) {
        // Signal writer: producer is gone; remaining events are still readable.
        // Note: the Arc in REGISTRY keeps the Ring alive until drain_all removes it.
        self.0.producer_alive.store(false, Ordering::Release);
    }
}

thread_local! {
    // Initialised lazily on first push(). Init allocates via the global
    // allocator (Arc::new, Vec::push), but the recursion guard is always
    // set before push() is called from record(), so those allocations are
    // suppressed by the guard and never re-enter record().
    //
    // Windows note: the Drop destructor runs on thread exit for threads
    // created via std::thread. Main-thread TLS destructors may NOT run on
    // Windows (no DllMain THREAD_DETACH for the main thread); the writer's
    // periodic drain picks up main-thread events instead.
    static THREAD_RING: RingHandle = {
        let ring = Arc::new(Ring::new());
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::clone(&ring));
        RingHandle(ring)
    };
}

/// Push an event onto the current thread's ring. Lock-free; never allocates
/// after TLS is initialised (first call per thread has one-time init cost).
#[inline]
pub fn push(ev: AllocEvent) -> bool {
    THREAD_RING.with(|h| h.0.push(ev))
}

/// Drain all events from all registered rings.
///
/// # Panics (debug builds)
/// Panics if the calling thread has not called `guard::force_enter_permanent()`.
/// This function must only be called from the writer thread.
pub fn drain_all(mut f: impl FnMut(AllocEvent)) {
    debug_assert!(
        crate::guard::is_set(),
        "drain_all must be called only from a thread where the recursion guard is permanently set (writer thread)"
    );
    let mut reg = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut i = 0;
    while i < reg.len() {
        while let Some(ev) = reg[i].pop() {
            f(ev);
        }
        if !reg[i].producer_alive.load(Ordering::Acquire) {
            // The Acquire on producer_alive synchronizes with the thread's death Release,
            // which transitively happens-after the last push's tail Release.
            // A second drain here picks up any event the first pass missed on weak-memory hardware.
            while let Some(ev) = reg[i].pop() {
                f(ev);
            }
            reg.swap_remove(i);
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use heaplens_protocol::{AllocEvent, EventKind};

    fn ev(n: u64) -> AllocEvent {
        AllocEvent::new(EventKind::Alloc, n, 0, 64, 8, n, [0u64; 16], 0)
    }

    #[test]
    fn push_pop_roundtrip() {
        let ring = Ring::new();
        assert!(ring.push(ev(1)));
        assert!(ring.push(ev(2)));
        assert_eq!(ring.pop().unwrap().ptr, 1);
        assert_eq!(ring.pop().unwrap().ptr, 2);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn fifo_order() {
        let ring = Ring::new();
        for i in 0..10u64 {
            ring.push(ev(i));
        }
        for i in 0..10u64 {
            assert_eq!(ring.pop().unwrap().ptr, i);
        }
    }

    #[test]
    fn full_ring_returns_false_and_increments_dropped() {
        let ring = Ring::new();
        // Fill to capacity-1 (ring has CAP slots but can hold CAP-1 items)
        let mut pushed = 0usize;
        for i in 0..CAP as u64 {
            if ring.push(ev(i)) { pushed += 1; } else { break; }
        }
        assert_eq!(pushed, CAP - 1, "ring holds exactly CAP-1 events when full");
        // Next push must fail and increment dropped
        let before = ring.dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert!(!ring.push(ev(99999)));
        let after = ring.dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after, before + 1);
    }

    #[test]
    fn registry_drain_after_thread_exit() {
        // A spawned thread pushes an event; after the thread exits, drain_all
        // must still return that event (the ring stays alive via Arc in registry).
        let handle = std::thread::spawn(|| {
            // Trigger THREAD_RING init + push by calling the module-level push.
            // Note: this will lock the registry briefly, which is safe here.
            crate::ring::push(ev(0xDEAD));
        });
        handle.join().unwrap();

        // Give TLS destructor time to run and mark producer_alive = false.
        std::thread::sleep(std::time::Duration::from_millis(10));

        // drain_all requires the recursion guard to be permanently set.
        crate::guard::force_enter_permanent();

        let mut found = false;
        drain_all(|e| {
            if e.ptr == 0xDEAD { found = true; }
        });
        assert!(found, "event from exited thread must be drainable");
    }
}
