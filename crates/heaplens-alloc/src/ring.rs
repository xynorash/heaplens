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

/// Per-thread ring storage, keyed by Fiber Local Storage rather than a
/// plain `thread_local!`.
///
/// # Why not `thread_local!`
///
/// This used to be exactly that — `thread_local! { static THREAD_RING:
/// RingHandle = ...; }`, with `RingHandle`'s `Drop` impl doing the
/// producer-death signalling described above. That crashed reliably
/// (confirmed via the `self_load_concurrency_stress` regression harness,
/// 2026-07-21 root-cause investigation) on any thread other than the one
/// that called `LoadLibraryW` to load this DLL. Root cause: `heaplens-hook`
/// is always loaded via `LoadLibraryW` at runtime (self-load or real
/// injection, never linked into the process at startup), and a
/// dynamically-loaded module's thread-locals are only reliably wired up
/// for the thread that loaded it — this held even for `RingHandle` despite
/// it having a real `Drop` impl (an earlier fix attempt assumed giving a
/// thread-local's value type a `Drop` impl changes which underlying
/// mechanism rustc/std uses to something safe for this scenario; that
/// assumption was wrong — see `guard.rs`'s doc comment for the full
/// account of that dead end). Full mechanism root-caused via reduction:
/// see the investigation report; not repeated here.
///
/// The fix: bypass `thread_local!` for the ring pointer itself and drive
/// Fiber Local Storage directly (`FlsAlloc`/`FlsGetValue`/`FlsSetValue`) —
/// like `guard.rs`'s `TlsAlloc`-based fix, a slot-index mechanism with no
/// dependency on module linkage or which thread loaded what, confirmed
/// safe from any thread regardless of `LoadLibraryW` timing. Unlike plain
/// `TlsAlloc`, FLS supports a real per-thread exit callback
/// (`FlsAlloc`'s `lpCallback`), which is what preserves this module's
/// existing invariant — a producer thread's ring gets marked dead (and
/// eventually reclaimed by the writer) when that thread exits, without
/// requiring the thread's own cooperation (essential: real injected
/// targets' threads cannot be asked to call an explicit cleanup function
/// before they exit).
#[cfg(windows)]
mod thread_ring {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, OnceLock};
    use windows_sys::Win32::System::Threading::{FlsAlloc, FlsFree, FlsGetValue, FlsSetValue};

    use super::{registry, Ring};

    static FLS_INDEX: OnceLock<u32> = OnceLock::new();

    /// Called by the OS when a thread (or fiber) that ever set this FLS
    /// slot exits. Reconstructs the `Arc<Ring>` this slot owned (the
    /// strong reference taken out in `with_ring` below, distinct from the
    /// registry's own clone), marks the ring's producer dead, then lets
    /// the `Arc` drop — releasing only *this* reference; the registry's
    /// clone keeps the `Ring` itself alive until `drain_all` removes it.
    unsafe extern "system" fn on_thread_exit(data: *const c_void) {
        if data.is_null() {
            return;
        }
        let ring = unsafe { Arc::from_raw(data.cast::<Ring>()) };
        ring.producer_alive.store(false, Ordering::Release);
    }

    fn index() -> u32 {
        *FLS_INDEX.get_or_init(|| unsafe { FlsAlloc(Some(on_thread_exit)) })
    }

    /// Explicit, proactive cleanup — call before there's any chance this
    /// module gets unloaded (i.e. from `detach_impl`, our own controlled
    /// unload point), not left to happen implicitly at process exit.
    ///
    /// Confirmed necessary (root-cause investigation, 2026-07-21): without
    /// this, a thread that made *any* hooked allocation and is still alive
    /// when the whole process later exits — in practice, the thread that
    /// called `attach`/`detach` itself, since even its own `println!`
    /// calls while hooks are live route through this same registration —
    /// keeps a live FLS registration pointing at `on_thread_exit`, code
    /// living inside this DLL. If process teardown unloads/unmaps this DLL
    /// before the OS gets around to running that thread's FLS callback,
    /// the callback fires into freed/unmapped memory — reliably reproduced
    /// as a crash strictly *after* a full clean attach/workload/detach
    /// cycle, i.e. after this module's own job was already done.
    ///
    /// `FlsFree` deregisters the index outright, so no future thread exit —
    /// including ones we have no way to wait for, e.g. an uncooperative
    /// injected target's other threads that are still running when we
    /// detach — can invoke this callback again after this call returns,
    /// regardless of when the process or this DLL actually goes away.
    /// Trade-off, accepted deliberately: any *other* thread that is still
    /// alive with an unflushed ring at this exact moment loses its
    /// automatic dead-producer detection for that ring (it stays
    /// `producer_alive: true` in the registry forever) — a stale entry,
    /// not a crash, and no worse than what already happens to any ring
    /// whose thread hasn't exited by the time `detach` runs.
    pub fn shutdown() {
        let idx = index();
        // Clear our own (the calling thread's) slot directly rather than
        // relying on FlsFree to invoke the callback for it — documented
        // behavior of exactly what FlsFree does for the calling thread's
        // own value differs across doc revisions; doing it explicitly
        // removes any ambiguity.
        let ptr = unsafe { FlsGetValue(idx) };
        if !ptr.is_null() {
            unsafe { on_thread_exit(ptr) };
            unsafe { FlsSetValue(idx, std::ptr::null()) };
        }
        unsafe { FlsFree(idx) };
    }

    /// Runs `f` against the current thread's ring, creating and
    /// registering it on first call. Initialisation allocates (`Arc::new`,
    /// `Vec::push` into the registry) via the global allocator, but the
    /// recursion guard is always set before this runs (called only from
    /// `record()`), so those allocations are suppressed and never
    /// re-enter `record()`.
    #[inline]
    pub fn with_ring<R>(f: impl FnOnce(&Ring) -> R) -> R {
        let idx = index();
        let ptr = unsafe { FlsGetValue(idx) } as *const Ring;
        if !ptr.is_null() {
            // SAFETY: this slot's Arc reference (taken out below, or by a
            // prior call on this same thread) stays alive until this
            // thread exits and `on_thread_exit` runs — which cannot be
            // happening concurrently with this call, since both only ever
            // run on this thread.
            return f(unsafe { &*ptr });
        }
        let ring = Arc::new(Ring::new());
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::clone(&ring));
        let raw = Arc::into_raw(ring);
        unsafe { FlsSetValue(idx, raw as *const c_void) };
        // SAFETY: `raw` is the pointer this slot now owns; no other
        // reference to it is dereferenced concurrently (see above).
        f(unsafe { &*raw })
    }
}

/// Non-Windows fallback — see `guard.rs`'s equivalent for why this project
/// otherwise only targets Windows.
#[cfg(not(windows))]
mod thread_ring {
    use std::sync::Arc;

    use super::{registry, Ring};

    struct RingHandle(Arc<Ring>);

    impl Drop for RingHandle {
        fn drop(&mut self) {
            self.0.producer_alive.store(false, std::sync::atomic::Ordering::Release);
        }
    }

    thread_local! {
        static THREAD_RING: RingHandle = {
            let ring = Arc::new(Ring::new());
            registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(Arc::clone(&ring));
            RingHandle(ring)
        };
    }

    pub fn with_ring<R>(f: impl FnOnce(&Ring) -> R) -> R {
        THREAD_RING.with(|h| f(&h.0))
    }

    /// No-op here — plain `thread_local!` destructors don't have the
    /// DLL-unload-ordering hazard the Windows path works around.
    pub fn shutdown() {}
}

/// Push an event onto the current thread's ring. Lock-free; never allocates
/// after first-call-per-thread initialisation.
#[inline]
pub fn push(ev: AllocEvent) -> bool {
    thread_ring::with_ring(|ring| ring.push(ev))
}

/// Proactively tears down the per-thread ring storage mechanism itself —
/// see `thread_ring::shutdown`'s doc comment. Must be called from
/// `heaplens-hook`'s `detach_impl` (its own controlled unload point),
/// before there's any chance this module gets unloaded.
pub fn shutdown() {
    thread_ring::shutdown();
}

/// Drain up to `max` events total across all registered rings, calling `f`
/// for each.
///
/// # Why capped, not unconditional (2026-07-22 fix)
///
/// This used to drain every ring down to empty, unconditionally, in one
/// call — no matter how much was pending. The writer thread's own batching
/// discipline ("flush every 64 events or 1ms, whichever first",
/// `writer.rs`'s `BATCH_CAP`/`FLUSH_INTERVAL`) only gates *when* to flush
/// the accumulated batch; it never bounded what a single `drain_all` call
/// could stuff into that batch beforehand. Each ring holds up to `CAP - 1`
/// (65,535) events; with N producer threads each with their own ring, a
/// writer thread that falls behind for any reason (a slow pipe write under
/// daemon backpressure, a burst of first-time symbol resolutions, an OS
/// scheduling gap under heavy concurrent load) could return to a
/// `drain_all` call that swept up to `N * 65,535` events in one shot,
/// blowing past not just the intended 64-event batch size but the wire
/// protocol's own `u16` event-count field (`heaplens_protocol::frame`'s
/// `encode_events`) — confirmed as the actual cause of a writer-thread
/// panic (`events count exceeds u16::MAX`) under sustained 12-thread
/// concurrent injection load, which in turn broke clean detach (the
/// panicked thread never reached `mark_writer_stopped()`).
///
/// Capping here makes an oversized batch structurally impossible from this
/// call site, rather than merely handling it gracefully at a higher limit:
/// callers with a batch-size budget (the writer thread) must pass their
/// *remaining* capacity, not drain everything unconditionally. Stopping
/// early — mid-ring, or between rings — is always safe and resumable: a
/// `Ring`'s own head/tail cursors are exactly where a partial drain leaves
/// them, so the next `drain_all` call continues correctly from there. A
/// ring whose producer died is only actually swept from the registry once
/// it drains down to empty — hitting the cap mid-cleanup just defers that
/// removal to a later call, it never skips or corrupts it.
///
/// # Panics (debug builds)
/// Panics if the calling thread has not called `guard::force_enter_permanent()`.
/// This function must only be called from the writer thread.
pub fn drain_all(max: usize, mut f: impl FnMut(AllocEvent)) {
    debug_assert!(
        crate::guard::is_set(),
        "drain_all must be called only from a thread where the recursion guard is permanently set (writer thread)"
    );
    let mut reg = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut i = 0;
    let mut drained = 0usize;
    while i < reg.len() {
        while drained < max {
            match reg[i].pop() {
                Some(ev) => { f(ev); drained += 1; }
                None => break,
            }
        }
        if drained >= max {
            return; // cap reached — remaining rings/events wait for the next call
        }
        // Ring i is empty (we only get here when the pop-loop above broke on
        // `None`, not on the cap) — same dead-producer check/cleanup as before.
        if !reg[i].producer_alive.load(Ordering::Acquire) {
            // The Acquire on producer_alive synchronizes with the thread's death Release,
            // which transitively happens-after the last push's tail Release.
            // A second drain here picks up any event the first pass missed on weak-memory hardware.
            while drained < max {
                match reg[i].pop() {
                    Some(ev) => { f(ev); drained += 1; }
                    None => break,
                }
            }
            if drained >= max {
                return;
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

    /// `registry()` is a single process-global `Mutex<Vec<Arc<Ring>>>`, and
    /// cargo runs tests in this binary concurrently by default. Any test
    /// that pushes a ring into it and then calls `drain_all` is reading and
    /// draining state shared with every other such test running at the same
    /// time — one test's `drain_all` call can silently drain another's
    /// still-pending ring, or leave stray rings behind for a later test to
    /// see. Hold this lock for the duration of any test that touches the
    /// registry (directly or via `crate::ring::push`) to serialize them
    /// against each other; tests that only exercise a standalone `Ring`
    /// instance never touch `registry()` and don't need it.
    static REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        let _lock = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        drain_all(usize::MAX, |e| {
            if e.ptr == 0xDEAD { found = true; }
        });
        assert!(found, "event from exited thread must be drainable");
    }

    #[test]
    fn drain_all_stops_at_the_cap_leaving_the_rest_for_next_call() {
        let _lock = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ring = Ring::new();
        for i in 0..10u64 {
            assert!(ring.push(ev(i)));
        }
        registry().lock().unwrap_or_else(|p| p.into_inner()).push(std::sync::Arc::new(ring));

        crate::guard::force_enter_permanent();

        let mut first_pass: Vec<u64> = Vec::new();
        drain_all(4, |e| first_pass.push(e.ptr));
        assert_eq!(first_pass, vec![0, 1, 2, 3], "must stop exactly at the cap, in FIFO order");

        let mut second_pass: Vec<u64> = Vec::new();
        drain_all(100, |e| second_pass.push(e.ptr));
        assert_eq!(
            second_pass, vec![4, 5, 6, 7, 8, 9],
            "the remaining events must still be there on the next call — capping must not drop anything"
        );
    }

    #[test]
    fn drain_all_cap_can_stop_mid_ring_across_multiple_producers() {
        let _lock = REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Two separate rings (standing in for two producer threads), each
        // holding more than half the cap — proves the cap is enforced
        // across the *total* drained this call, not reset per-ring, and
        // that a cap hit partway through ring A correctly leaves ring B
        // completely untouched for the next call.
        let ring_a = Ring::new();
        let ring_b = Ring::new();
        for i in 0..6u64 {
            assert!(ring_a.push(ev(100 + i)));
        }
        for i in 0..6u64 {
            assert!(ring_b.push(ev(200 + i)));
        }
        {
            let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
            reg.push(std::sync::Arc::new(ring_a));
            reg.push(std::sync::Arc::new(ring_b));
        }

        crate::guard::force_enter_permanent();

        let mut drained: Vec<u64> = Vec::new();
        drain_all(8, |e| drained.push(e.ptr));
        assert_eq!(drained.len(), 8, "must drain exactly the cap, not more");
        assert_eq!(
            &drained[0..6], &[100, 101, 102, 103, 104, 105],
            "ring A must be fully drained first"
        );
        assert_eq!(
            &drained[6..8], &[200, 201],
            "ring B must be drained only up to the remaining cap budget"
        );

        let mut rest: Vec<u64> = Vec::new();
        drain_all(100, |e| rest.push(e.ptr));
        assert_eq!(rest, vec![202, 203, 204, 205], "ring B's remainder must survive to the next call");
    }
}
