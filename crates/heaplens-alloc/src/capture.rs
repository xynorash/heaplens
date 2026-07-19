use std::sync::OnceLock;
use std::time::Instant;

/// Process-start instant, initialised once on first call to `timestamp_nanos`.
/// `OnceLock::get_or_init` does not allocate.
static START: OnceLock<Instant> = OnceLock::new();

/// Monotonic timestamp in nanoseconds since process start.
/// Uses `Instant` (QPC on Windows) — no allocation, no unsafe.
#[inline]
pub fn timestamp_nanos() -> u64 {
    let start = START.get_or_init(Instant::now);
    Instant::now().duration_since(*start).as_nanos() as u64
}

/// Capture up to 16 raw instruction pointers from the current call stack.
/// Does not resolve symbols (symbol resolution is off-path, in writer.rs) and
/// does not attempt to skip or filter any frames — the raw trace always
/// starts inside the shared instrumentation chain (capture_stack -> record ->
/// the allocator method), and the depth of std-internal frames between that
/// chain and genuine caller code varies by allocation API (confirmed: the
/// zeroed-alloc path used by `vec![0u8; n]` inserts 6+ more non-inlined
/// frames than `Vec::with_capacity`). Locating the real call site is done by
/// the daemon (graph.rs's `effective_site`), using classifications the
/// writer thread ships in the SYMBOLS frame — not by any fixed skip count
/// here. 16 frames is wide enough to reach real user code even on the
/// deepest observed path.
///
/// # Safety precondition
/// Must be called with the per-thread recursion guard set. Any allocation
/// triggered by backtrace internals (first-use frame table init) will be
/// suppressed by the guard and will not recurse into `record`.
///
/// `trace_unsynchronized` is unsafe because it must not be called
/// concurrently from multiple threads without external synchronisation.
/// Here it is safe: called per-thread, one call at a time, under the
/// per-thread guard.
#[inline]
pub fn capture_stack() -> ([u64; 16], u8) {
    let mut stack = [0u64; 16];
    let mut count = 0usize;
    // Serialize against every other caller of `backtrace`'s Windows backend
    // (this function, called from every allocating thread, and
    // `writer::run`'s `resolve` calls on the writer thread) — see
    // `crate::DBGHELP_LOCK`'s doc comment for why this is required, not
    // just defensive. `.lock()`'s `Err` (a poisoned mutex, meaning some
    // other caller panicked while holding it) is treated as "proceed
    // anyway" rather than propagating the panic: a stale/corrupt symbol
    // table is a degraded stack capture, not a reason to crash the whole
    // allocation this call is instrumenting.
    let _guard = crate::DBGHELP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: called single-threaded per-thread, guard is held.
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if count < 16 {
                stack[count] = frame.ip() as u64;
                count += 1;
                true  // continue walking
            } else {
                false // stop
            }
        });
    }
    (stack, count as u8)
}
