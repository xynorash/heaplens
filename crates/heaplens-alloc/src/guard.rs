//! Per-thread reentrancy flag for `record()`.
//!
//! # Why this isn't a plain `thread_local!`
//!
//! It used to be exactly that — a `thread_local! { static IN_ALLOC:
//! Cell<bool> = ...; }`. That crashed reliably (confirmed via the
//! `self_load_concurrency_stress` regression harness, 2026-07-21 root-cause
//! investigation): any thread other than the one that called
//! `LoadLibraryW` on this DLL segfaulted on its *first* access to that
//! thread-local, deterministically, every run — even reduced to nothing
//! but a single read with everything else in `record()` stubbed out.
//!
//! Root cause: rustc's `thread_local!` on `x86_64-pc-windows-msvc` can
//! implement a thread-local using a genuine PE `.tls`-section variable,
//! accessed directly via the FS/GS segment register ("static"/"implicit"
//! TLS) — the fastest option, and the one the compiler prefers when it
//! can. That mechanism depends on the OS loader having already registered
//! this module's TLS index in *every* thread's TEB — which is only
//! guaranteed for a module that's linked into the process at startup.
//! `heaplens-hook` is never that: it is always loaded via `LoadLibraryW`
//! at runtime (self-load harnesses and real injected targets alike), so
//! any thread the loader didn't specifically retrofit — in practice, every
//! thread except the one that called `LoadLibraryW` — reads a TLS slot
//! that was never set up for this module, which is exactly what
//! segfaulted here.
//!
//! An initial attempt to fix this by giving the thread-local's value type
//! a `Drop` impl (hoping to force rustc onto its other, non-`.tls`-section
//! implementation) did **not** resolve the crash — confirmed by rerunning
//! the identical isolated reduction and observing the same segfault. Do
//! not reintroduce that approach without re-verifying it against the
//! harness first; whatever rustc chooses for a `Drop`-having thread-local
//! on this target, it wasn't clean of the same hazard.
//!
//! The fix that *is* confirmed to work is bypassing `thread_local!`
//! entirely on Windows and driving the raw Win32 dynamic-TLS API directly
//! (`TlsAlloc`/`TlsGetValue`/`TlsSetValue`) — a plain slot-index-based
//! mechanism with no dependency on module linkage or which thread loaded
//! what. This is Microsoft's own documented answer for "a DLL that may be
//! loaded via `LoadLibrary` needs per-thread state" — see
//! `TlsAlloc`'s documentation. Validated by rerunning the full
//! `self_load_concurrency_stress` harness 10 consecutive times, hooked,
//! all clean (see the root-cause investigation report for the run log).

#[cfg(windows)]
mod imp {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Threading::{TlsAlloc, TlsGetValue, TlsSetValue};

    /// Process-wide TLS slot index, allocated once. `OnceLock` itself is a
    /// plain heap-independent static (no TLS, no allocation after the one
    /// `TlsAlloc` call) — safe to read from any thread regardless of when
    /// or how this module was loaded.
    static TLS_INDEX: OnceLock<u32> = OnceLock::new();

    #[inline]
    fn index() -> u32 {
        *TLS_INDEX.get_or_init(|| unsafe { TlsAlloc() })
    }

    /// `TlsGetValue` returns null for a slot that was never set on this
    /// thread — indistinguishable from "explicitly set to null", which is
    /// exactly the "not in alloc" state we want as the default, so no
    /// separate first-access initialization is needed.
    #[inline]
    pub fn is_set() -> bool {
        unsafe { !TlsGetValue(index()).is_null() }
    }

    #[inline]
    pub fn set(val: bool) {
        let v = if val { 1usize as *mut core::ffi::c_void } else { core::ptr::null_mut() };
        unsafe {
            TlsSetValue(index(), v);
        }
    }
}

/// Non-Windows fallback. This project targets Windows only (named pipes,
/// `RtlAllocateHeap` hooking, etc.) — this branch exists so `cargo check`
/// on a non-Windows dev machine doesn't fail outright, not because it's
/// expected to run for real anywhere.
#[cfg(not(windows))]
mod imp {
    use std::cell::Cell;

    thread_local! {
        static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
    }

    #[inline]
    pub fn is_set() -> bool {
        IN_ALLOC.with(|f| f.get())
    }

    #[inline]
    pub fn set(val: bool) {
        IN_ALLOC.with(|f| f.set(val));
    }
}

/// Returns true if the current thread is already inside `record`.
#[inline]
pub fn is_set() -> bool {
    imp::is_set()
}

/// Permanently marks the current thread as inside-allocator.
/// Called once by the writer thread at startup so that none of its own
/// allocations (pipe buffers, symbol resolution, etc.) are ever recorded.
pub fn force_enter_permanent() {
    imp::set(true);
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
        debug_assert!(!is_set(), "ScopedGuard::enter called on a permanently-guarded thread");
        imp::set(true);
        ScopedGuard(())
    }
}

impl Drop for ScopedGuard {
    #[inline]
    fn drop(&mut self) {
        imp::set(false);
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

    #[test]
    fn distinct_threads_get_independent_slots() {
        // The TlsAlloc-based implementation shares one process-wide slot
        // *index*, but TlsGetValue/TlsSetValue are inherently per-thread —
        // this test pins down that setting the flag on one thread never
        // leaks into another's view of it.
        let t1 = std::thread::spawn(|| {
            assert!(!is_set());
            force_enter_permanent();
            assert!(is_set());
        });
        t1.join().unwrap();

        std::thread::spawn(|| {
            assert!(!is_set(), "a fresh thread must not see another thread's flag");
        })
        .join()
        .unwrap();
    }
}
