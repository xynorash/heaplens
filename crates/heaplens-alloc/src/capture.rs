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
///
/// # `DBGHELP_LOCK` uses `try_lock`, never `lock` — confirmed deadlock fix
///
/// A blocking `.lock()` here can hang forever if `DBGHELP_LOCK` is
/// orphaned: confirmed via WinDbg (exact symbol-resolved stack trace,
/// 2026-07-26 investigation) that `RtlExitUserProcess` abruptly terminates
/// a process's other threads, without running their cleanup, when the
/// process exits with hooks still active (no explicit
/// `heaplens-injector --detach`). If one of those threads was caught
/// holding `DBGHELP_LOCK` — inside this exact function — at that instant,
/// the lock is orphaned permanently: nothing will ever release it. The
/// sole surviving thread then reliably deadlocked here, reached via its
/// own FLS-cleanup-triggered heap free routing back through the still
/// -active hook (`hook_heap_free` -> `record` -> `capture_stack` ->
/// `DBGHELP_LOCK.lock()`), confirmed reproducing 7/8 times in the same
/// investigation's batch runs. `try_lock` cannot hang: if the lock is
/// held (orphaned or merely contended — this branch does not need to
/// know which), this call skips symbolization for this one capture and
/// returns an empty stack instead of blocking. A single degraded capture
/// is harmless — every consumer downstream (the daemon's phi inference,
/// the target-diagnostics banner) already tolerates unresolved/hex
/// -fallback symbols as a normal case. A permanent hang is not harmless.
/// The overwhelmingly common case (lock uncontended) behaves identically
/// to the previous `.lock()` call: `try_lock` succeeds immediately, same
/// as `lock` would have, with no added cost.
#[inline]
pub fn capture_stack() -> ([u64; 16], u8) {
    let mut stack = [0u64; 16];
    let mut count = 0usize;
    // Serialize against every other caller of `backtrace`'s Windows backend
    // (this function, called from every allocating thread, and
    // `writer::run`'s `resolve` calls on the writer thread) — see
    // `crate::DBGHELP_LOCK`'s doc comment for why this is required, not
    // just defensive. A poisoned mutex (some other caller panicked while
    // holding it) is treated the same as an uncontended lock — proceed
    // anyway, a stale/corrupt symbol table is a degraded capture, not a
    // reason to crash the allocation this call is instrumenting. A held
    // (contended or orphaned) lock returns an empty, unresolved capture
    // rather than blocking — see this function's doc comment above.
    let guard = match crate::DBGHELP_LOCK.try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    };
    let Some(_guard) = guard else {
        return (stack, 0);
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// `DBGHELP_LOCK` is a single process-wide static — cargo runs this
    /// binary's tests in parallel by default, so a test that deliberately
    /// holds it (to prove `capture_stack` doesn't block) would otherwise
    /// race a concurrent test that expects it free, producing a spurious
    /// empty capture there. Serializes just these two tests against each
    /// other; same pattern as `ring.rs`'s `REGISTRY_TEST_LOCK`.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The confirmed deadlock fix's core guarantee: `capture_stack` must
    /// never block, even when `DBGHELP_LOCK` is held elsewhere. Holds the
    /// lock on a background thread (standing in for the orphaned-lock
    /// scenario — from `capture_stack`'s point of view, an orphaned lock
    /// and a merely-busy one are indistinguishable, and don't need to be
    /// distinguished) and confirms a concurrent `capture_stack` call
    /// returns promptly with an empty, unresolved capture instead of
    /// waiting for the lock to free up.
    #[test]
    fn capture_stack_does_not_block_when_dbghelp_lock_is_held() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let held = crate::DBGHELP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let result = capture_stack();
            let _ = tx.send(result);
        });

        // Generous bound for scheduling jitter — the whole point is this
        // must return almost immediately (try_lock, no waiting), not that
        // it merely returns eventually.
        let (stack, count) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("capture_stack blocked instead of returning promptly while DBGHELP_LOCK was held");

        assert_eq!(count, 0, "a held lock must produce an empty capture, not a real one");
        assert_eq!(stack, [0u64; 16], "a held lock must not partially fill the stack array");

        drop(held);
        handle.join().unwrap();
    }

    /// The overwhelmingly common case — lock uncontended — must behave
    /// exactly as the previous blocking `.lock()` call did: succeed
    /// immediately and produce a real, non-empty capture.
    #[test]
    fn capture_stack_captures_normally_when_dbghelp_lock_is_free() {
        let _serial = TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let (stack, count) = capture_stack();
        assert!(count > 0, "an uncontended lock must still produce a real capture");
        assert_ne!(stack[0], 0, "the first captured frame must be a real, non-null address");
    }
}
