#[doc(hidden)]
pub mod guard;
#[doc(hidden)]
pub mod ring;
#[doc(hidden)]
pub mod capture;
#[doc(hidden)]
pub mod writer;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{Mutex, Once};
use std::sync::atomic::{AtomicBool, Ordering};
use heaplens_protocol::EventKind;
use heaplens_protocol::AllocEvent;

/// Serializes every call into `backtrace`'s Windows backend — both
/// `capture::capture_stack`'s `trace_unsynchronized` (called from every
/// allocating thread, on the hot path) and `writer::run`'s `resolve` calls
/// (called from the single writer thread). `trace_unsynchronized`'s own
/// safety doc already warns it must not be called concurrently from
/// multiple threads "without external synchronisation" — this is that
/// synchronisation, extended to also cover `resolve`, since both
/// ultimately reach `dbghelp.dll`, which Windows documents as not safe for
/// concurrent calls from multiple threads. A single-threaded producer never
/// contends this lock in practice; a multi-threaded one (concurrent
/// allocating threads, or an allocating thread racing the writer thread's
/// own resolve loop) genuinely needs it.
pub(crate) static DBGHELP_LOCK: Mutex<()> = Mutex::new(());

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

impl Default for HeapLensAlloc {
    fn default() -> Self {
        Self::new()
    }
}

static WRITER_ONCE: Once = Once::new();
static WRITER_DEAD: AtomicBool = AtomicBool::new(false);
static WRITER_SHOULD_STOP: AtomicBool = AtomicBool::new(false);
static WRITER_STOPPED: AtomicBool = AtomicBool::new(false);

/// Read by `writer::run`'s loop. Not exposed outside this crate — front-ends
/// request shutdown via `request_writer_stop_and_wait`, they don't poll this
/// directly.
pub(crate) fn writer_should_stop() -> bool {
    WRITER_SHOULD_STOP.load(Ordering::Acquire)
}

/// Set by `writer::run` immediately before it returns.
pub(crate) fn mark_writer_stopped() {
    WRITER_STOPPED.store(true, Ordering::Release);
}

/// Spawn the writer thread exactly once. Safe to call from the hot path for
/// the cooperative `#[global_allocator]` front-end: after the first
/// successful call_once, subsequent calls are a single atomic load (no
/// allocation, no blocking).
///
/// **Not safe to reach lazily from an injected hook callback** (Stage 7,
/// `heaplens-hook`) — confirmed empirically: spawning a thread from inside
/// a MinHook-detoured `RtlAllocateHeap`/`HeapAlloc` call crashes
/// (`STATUS_ACCESS_VIOLATION`), reproducibly, isolated by disabling every
/// other part of `record()` in turn until only the `std::thread::spawn`
/// call remained implicated. Root cause: `CreateThread`'s synchronous
/// `DLL_THREAD_ATTACH` notifications run on the new thread before it's
/// fully initialized, and something in that bootstrap path re-enters the
/// hooked allocation function while the thread isn't in a state that
/// tolerates it. `heaplens-hook`'s `HeapLensHookAttach` therefore calls
/// `ensure_writer_started()` eagerly, from a normal (non-hook) thread
/// context, before enabling any hook — see `docs/stage7-injection-design.md`
/// §4 (this finding postdates and refines the design's original safety
/// analysis, which did not anticipate this specific hazard). By the time
/// any hook callback reaches this function, `WRITER_ONCE` has already
/// fired, so the call below is a single atomic load — no thread is ever
/// spawned from a hook callback.
///
/// The spawn itself allocates ("heaplens-writer" thread name string), but it
/// runs under the recursion guard, so those allocations are suppressed.
#[inline]
fn ensure_writer() {
    WRITER_ONCE.call_once(|| {
        if std::thread::Builder::new()
            .name("heaplens-writer".to_owned())
            .spawn(writer::run)
            .is_err()
        {
            // Spawn failed (e.g., out of threads). Mark the writer dead so
            // record() can skip the ring push rather than filling the ring
            // silently until it overflows.
            WRITER_DEAD.store(true, Ordering::Relaxed);
        }
    });
}

/// Public entry point for capture front-ends that cannot rely on `record`'s
/// lazy spawn — currently `heaplens-hook`, which must start the writer
/// thread from a normal thread context (its `HeapLensHookAttach`, before
/// any hook is enabled) rather than from inside a hook callback. See the
/// safety note on `ensure_writer` above.
pub fn ensure_writer_started() {
    ensure_writer();
}

/// Signals the writer thread to stop and waits (bounded by `timeout`) for
/// it to actually do so. Returns `true` if it stopped in time.
///
/// **Required by `heaplens-hook`'s `HeapLensHookDetach` — and required to
/// be called *after* hooks are disabled, not before.** Confirmed
/// empirically as a fourth, distinct hazard in the same family as the two
/// documented on `ensure_writer` and `warm_up_symbol_resolution`, this one
/// the mirror image of the writer-thread-*creation* hazard: a thread
/// *exiting* naturally also triggers `DLL_THREAD_DETACH` notifications and
/// TLS-destructor cleanup on that thread, which itself performs heap
/// operations. With hooks still active at that moment, those exit-time
/// heap calls route through the detour during the exact window a thread is
/// mid-teardown — isolated via the same disable-one-thing-at-a-time method
/// as the other three hazards, and via diagnostic prints confirming the
/// writer thread returned cleanly from `writer::run` immediately before the
/// crash. The fix: `heaplens-hook`'s `HeapLensHookDetach` calls
/// `MinHook::disable_all_hooks` *before* calling this function, so the
/// writer thread's own exit-time heap traffic goes through the real,
/// unhooked functions. See `docs/stage7-injection-design.md` §4.
pub fn request_writer_stop_and_wait(timeout: std::time::Duration) -> bool {
    WRITER_SHOULD_STOP.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + timeout;
    while !WRITER_STOPPED.load(Ordering::Acquire) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

/// Proactively tears down the per-thread ring-registration mechanism
/// itself, ahead of any chance this module gets unloaded. **Required by
/// `heaplens-hook`'s `HeapLensHookDetach`/`HeapLensHookDetachApc`** — see
/// `ring::shutdown`'s doc comment for the full account of the crash this
/// closes (a stale per-thread ring registration whose exit callback lives
/// inside this DLL, invoked after the DLL may have already been unloaded).
/// Safe to call regardless of hook state; does not touch MinHook or the
/// private heap.
pub fn shutdown_ring_storage() {
    ring::shutdown();
}

/// Forces `backtrace::resolve`'s one-time lazy initialization (on Windows,
/// this loads and initializes `dbghelp.dll` — `SymInitialize` and friends)
/// to happen now, synchronously, on the calling (normal) thread.
///
/// **Required before `heaplens-hook` enables any hook.** Confirmed
/// empirically, the same way as the writer-thread-spawn hazard documented
/// on `ensure_writer`: with the writer thread spawned eagerly (fixing that
/// first hazard), the capture pipeline still crashed
/// (`STATUS_ACCESS_VIOLATION`) the first time the writer thread's symbol
/// resolution loop (`writer::run`'s `backtrace::resolve` call) ran with
/// hooks already live — isolated via the same disable-one-thing-at-a-time
/// method, and via diagnostic prints showing the crash follows immediately
/// after the writer thread's first `backtrace::resolve` call. Root cause is
/// the same *class* of hazard as the thread-spawn issue, one layer later:
/// `dbghelp.dll`'s first load/`SymInitialize` does its own heavyweight,
/// first-time OS-level setup (module enumeration, internal allocations),
/// and doing that for the first time while `RtlAllocateHeap` is hooked
/// process-wide is unsafe, whether it happens synchronously inside a hook
/// callback (the earlier hazard) or asynchronously on a background thread
/// racing against active hook traffic (this one). The general rule this
/// establishes for `heaplens-hook`: **any first-time, heavyweight
/// OS/runtime infrastructure initialization the capture pipeline depends
/// on must be forced to completion before `MinHook::enable_all_hooks`**,
/// never left lazy. See `docs/stage7-injection-design.md` §4.
pub fn warm_up_symbol_resolution() {
    // Resolve this very function's own address — always valid, always
    // resolvable, and its result is intentionally discarded. The only goal
    // is forcing whatever one-time setup `backtrace::resolve` performs to
    // run now, on this thread, before any hook exists to race against it.
    let addr = warm_up_symbol_resolution as *const () as *mut std::ffi::c_void;
    let _guard = DBGHELP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    backtrace::resolve(addr, |_sym| {});
}

/// Record one allocation event. Hot path.
///
/// Invariants enforced here:
/// 1. Guard checked first — re-entrant calls return immediately.
/// 2. Guard set via RAII ScopedGuard — cleared even on panic.
/// 3. No allocation, no locking.
/// 4. ring::push failure (full ring) is a silent drop.
///
/// Public so that other capture front-ends (e.g. `heaplens-hook`'s injected
/// MinHook trampolines, which cannot use `#[global_allocator]`) can drive
/// the same capture pipeline — ring, writer thread, symbol resolution, wire
/// framing — from a different interception mechanism. See
/// `docs/stage7-injection-design.md` §1.3.
#[inline]
pub fn record(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, align: u32) {
    // 1. Re-entrancy check — must be the very first thing.
    if guard::is_set() { return; }

    // Early-exit if the writer thread failed to spawn; no consumer exists.
    if WRITER_DEAD.load(Ordering::Relaxed) { return; }

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
