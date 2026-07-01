use std::cell::Cell;

thread_local! {
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
}

/// Returns true if the current thread is already inside `record`.
#[inline]
pub fn is_set() -> bool {
    IN_ALLOC.with(|f| f.get())
}

/// Permanently marks the current thread as inside-allocator.
/// Called once by the writer thread at startup so that none of its own
/// allocations (pipe buffers, symbol resolution, etc.) are ever recorded.
pub fn force_enter_permanent() {
    IN_ALLOC.with(|f| f.set(true));
}

/// RAII guard that sets the per-thread recursion flag on creation and clears
/// it on Drop. Panic-safe: Drop runs during stack unwinding, so the flag is
/// always cleared even if code between `enter()` and drop panics.
pub struct ScopedGuard(());

impl ScopedGuard {
    /// Set the recursion flag. Must only be called after confirming `is_set()`
    /// is false — the guard does not check this itself.
    #[inline]
    pub fn enter() -> Self {
        IN_ALLOC.with(|f| f.set(true));
        ScopedGuard(())
    }
}

impl Drop for ScopedGuard {
    #[inline]
    fn drop(&mut self) {
        IN_ALLOC.with(|f| f.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_clear() {
        // Spawn to get a fresh TLS slot uncontaminated by other tests.
        std::thread::spawn(|| {
            assert!(!is_set(), "guard must start clear on a new thread");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn scoped_guard_sets_and_clears() {
        std::thread::spawn(|| {
            assert!(!is_set());
            let g = ScopedGuard::enter();
            assert!(is_set());
            drop(g);
            assert!(!is_set());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn guard_clears_after_panic() {
        std::thread::spawn(|| {
            let _ = std::panic::catch_unwind(|| {
                let _g = ScopedGuard::enter();
                assert!(is_set());
                panic!("test panic while guard held");
            });
            assert!(!is_set(), "guard must clear even after a panic (Drop ran during unwind)");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn reentrancy_check_via_is_set() {
        std::thread::spawn(|| {
            let _g = ScopedGuard::enter();
            // Simulate what record() does: check first, skip if set.
            if is_set() {
                return; // correct — re-entrant call is a no-op
            }
            panic!("re-entrant call should have been blocked by is_set()");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn force_enter_permanent_stays_set() {
        std::thread::spawn(|| {
            assert!(!is_set());
            force_enter_permanent();
            assert!(is_set());
            // No ScopedGuard here; stays set forever on this thread.
            // (Writer thread relies on this.)
        })
        .join()
        .unwrap();
    }
}
