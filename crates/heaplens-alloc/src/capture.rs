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

/// Capture up to 8 raw instruction pointers from the current call stack.
/// Does not resolve symbols (symbol resolution is off-path, in writer.rs).
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
pub fn capture_stack() -> ([u64; 8], u8) {
    let mut stack = [0u64; 8];
    let mut count = 0usize;
    // SAFETY: called single-threaded per-thread, guard is held.
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if count < 8 {
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
