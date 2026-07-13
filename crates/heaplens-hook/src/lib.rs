//! Stage 7 capture front-end for injected (uncooperative) processes.
//!
//! Loaded into a target process by `heaplens-injector` and driven via the
//! two exported entry points below — never from `DllMain`, per
//! `docs/stage7-injection-design.md` §4.1 (loader-lock deadlock hazard).
//! `HeapLensHookAttach` installs MinHook trampolines on
//! `HeapAlloc`/`HeapReAlloc`/`HeapFree` and drives the same capture
//! pipeline (`heaplens_alloc::record`, the ring buffer, the writer thread)
//! that the cooperative `#[global_allocator]` producer uses — only the
//! interception mechanism differs. `HeapLensHookDetach` removes the hooks
//! and leaves the target exactly as if this DLL had never loaded.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use minhook::MinHook;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HeapCreate, HeapDestroy, HeapFree, HeapReAlloc};

use heaplens_protocol::EventKind;

// ── Private heap allocator (§4.2: internal bookkeeping never lands on the ─
//    target's own default heap, isolating our footprint from what the
//    target — or its own diagnostics — would see as its heap contents).
//
// Ordering invariant this module depends on: `PRIVATE_HEAP` is set *before*
// hooks are created/enabled in `HeapLensHookAttach`, and cleared *after*
// hooks are disabled/removed in `HeapLensHookDetach`. This guarantees the
// fallback branch below (`heap.is_null()`) — which calls the plain,
// unpatched `HeapAlloc`/`GetProcessHeap` — only ever executes while no hook
// is installed, so it can never recurse into our own detour.

static PRIVATE_HEAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_HEAP_ALLOC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_HEAP_REALLOC: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ORIG_HEAP_FREE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static ATTACHED: AtomicBool = AtomicBool::new(false);

type HeapAllocFn = unsafe extern "system" fn(HANDLE, u32, usize) -> *mut c_void;
type HeapReAllocFn = unsafe extern "system" fn(HANDLE, u32, *const c_void, usize) -> *mut c_void;
type HeapFreeFn = unsafe extern "system" fn(HANDLE, u32, *const c_void) -> i32;

struct PrivateHeapAlloc;

unsafe impl std::alloc::GlobalAlloc for PrivateHeapAlloc {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        let size = layout.size().max(1);
        let heap = PRIVATE_HEAP.load(Ordering::Acquire);
        if heap.is_null() {
            // Bootstrap window (before HeapCreate in attach, or after
            // HeapDestroy in detach): no hook is installed here, so the
            // plain Win32 call below is the real, unpatched function.
            return unsafe { HeapAlloc(GetProcessHeap(), 0, size) as *mut u8 };
        }
        let trampoline = ORIG_HEAP_ALLOC.load(Ordering::Acquire);
        if trampoline.is_null() {
            // Private heap exists but hooks aren't installed yet (mid-attach).
            return unsafe { HeapAlloc(heap as HANDLE, 0, size) as *mut u8 };
        }
        let f: HeapAllocFn = unsafe { std::mem::transmute(trampoline) };
        unsafe { f(heap as HANDLE, 0, size) as *mut u8 }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: std::alloc::Layout) {
        let heap = PRIVATE_HEAP.load(Ordering::Acquire);
        if heap.is_null() {
            unsafe { HeapFree(GetProcessHeap(), 0, ptr as *const c_void) };
            return;
        }
        let trampoline = ORIG_HEAP_FREE.load(Ordering::Acquire);
        if trampoline.is_null() {
            unsafe { HeapFree(heap as HANDLE, 0, ptr as *const c_void) };
            return;
        }
        let f: HeapFreeFn = unsafe { std::mem::transmute(trampoline) };
        unsafe { f(heap as HANDLE, 0, ptr as *const c_void) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new_size: usize) -> *mut u8 {
        let _ = layout;
        let size = new_size.max(1);
        let heap = PRIVATE_HEAP.load(Ordering::Acquire);
        if heap.is_null() {
            return unsafe { HeapReAlloc(GetProcessHeap(), 0, ptr as *const c_void, size) as *mut u8 };
        }
        let trampoline = ORIG_HEAP_REALLOC.load(Ordering::Acquire);
        if trampoline.is_null() {
            return unsafe { HeapReAlloc(heap as HANDLE, 0, ptr as *const c_void, size) as *mut u8 };
        }
        let f: HeapReAllocFn = unsafe { std::mem::transmute(trampoline) };
        unsafe { f(heap as HANDLE, 0, ptr as *const c_void, size) as *mut u8 }
    }
}

#[global_allocator]
static ALLOC: PrivateHeapAlloc = PrivateHeapAlloc;

// ── Hook detours ────────────────────────────────────────────────────────
//
// Each detour calls straight through to the trampoline (the real function,
// MinHook-relocated — never re-enters our own patched code), then drives
// the shared capture pipeline via `heaplens_alloc::record`, which checks
// the per-thread reentrancy guard as its very first action.

unsafe extern "system" fn hook_heap_alloc(hheap: HANDLE, dwflags: u32, dwbytes: usize) -> *mut c_void {
    let trampoline = ORIG_HEAP_ALLOC.load(Ordering::Acquire);
    let real: HeapAllocFn = unsafe { std::mem::transmute(trampoline) };
    let ptr = unsafe { real(hheap, dwflags, dwbytes) };
    if !ptr.is_null() {
        heaplens_alloc::record(EventKind::Alloc, ptr as u64, 0, dwbytes as u64, 0);
    }
    ptr
}

unsafe extern "system" fn hook_heap_realloc(
    hheap: HANDLE,
    dwflags: u32,
    lpmem: *const c_void,
    dwbytes: usize,
) -> *mut c_void {
    let trampoline = ORIG_HEAP_REALLOC.load(Ordering::Acquire);
    let real: HeapReAllocFn = unsafe { std::mem::transmute(trampoline) };
    let new_ptr = unsafe { real(hheap, dwflags, lpmem, dwbytes) };
    if !new_ptr.is_null() {
        heaplens_alloc::record(EventKind::Realloc, new_ptr as u64, lpmem as u64, dwbytes as u64, 0);
    }
    new_ptr
}

unsafe extern "system" fn hook_heap_free(hheap: HANDLE, dwflags: u32, lpmem: *const c_void) -> i32 {
    let trampoline = ORIG_HEAP_FREE.load(Ordering::Acquire);
    let real: HeapFreeFn = unsafe { std::mem::transmute(trampoline) };
    let result = unsafe { real(hheap, dwflags, lpmem) };
    if result != 0 {
        // §1.4: an unknown-pointer free (pre-attach allocation) is handled
        // downstream by `Graph::on_dealloc` returning early — nothing to
        // special-case here.
        heaplens_alloc::record(EventKind::Dealloc, lpmem as u64, 0, 0, 0);
    }
    result
}

// ── Exported attach/detach entry points (§4.1) ─────────────────────────
//
// Called via a second `CreateRemoteThread`, after `LoadLibraryW` has
// already loaded this DLL and returned — never from `DllMain`, so none of
// this runs under the loader lock.

/// Returns 0 on success, a nonzero failure code otherwise.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookAttach() -> u32 {
    if ATTACHED.swap(true, Ordering::AcqRel) {
        return 0; // already attached — idempotent
    }

    // 1. Private heap first (§4.2), using the real, not-yet-hooked HeapCreate.
    let heap = unsafe { HeapCreate(0, 0, 0) };
    if heap.is_null() {
        ATTACHED.store(false, Ordering::Release);
        return 1;
    }
    PRIVATE_HEAP.store(heap as *mut c_void, Ordering::Release);

    // 2. Force every piece of heavyweight, first-time infrastructure setup
    //    the capture pipeline depends on to complete now, from this normal
    //    thread context — before any hook is installed. Two hazards
    //    confirmed empirically, same class, same fix pattern:
    //    (a) spawning the writer thread lazily from inside a hook callback
    //        (the cooperative allocator's approach, safe there) crashes
    //        here, because CreateThread's synchronous DLL_THREAD_ATTACH
    //        bootstrap on the new thread reenters the not-yet-stable hooked
    //        allocation path;
    //    (b) even with the writer thread spawned eagerly, its first
    //        `backtrace::resolve` call (dbghelp.dll load + SymInitialize)
    //        still crashed once hooks were live, for the same underlying
    //        reason one layer later.
    //    See the safety notes on `heaplens_alloc::ensure_writer` and
    //    `heaplens_alloc::warm_up_symbol_resolution`. By the time any hook
    //    callback below fires, both are already warm, so nothing on the
    //    hot path performs first-time OS-level initialization.
    heaplens_alloc::warm_up_symbol_resolution();
    heaplens_alloc::ensure_writer_started();
    // Give the writer thread a brief head start to reach its own connect
    // loop before hooks go live, reducing (not eliminating — the ring
    // buffer tolerates this) the chance its first real batch races hook
    // installation.
    std::thread::sleep(std::time::Duration::from_millis(50));

    // 3. Install hooks. Order matters: PRIVATE_HEAP is already set, so any
    //    allocation `heaplens_alloc::record`/the writer thread perform from
    //    here on lands on the private heap via the trampoline once it's
    //    populated, or via the still-unhooked plain HeapAlloc until then.
    let alloc_orig = match unsafe {
        MinHook::create_hook_api("ntdll.dll", "RtlAllocateHeap", hook_heap_alloc as *mut c_void)
    } {
        Ok(p) => p,
        Err(_) => {
            ATTACHED.store(false, Ordering::Release);
            return 2;
        }
    };
    ORIG_HEAP_ALLOC.store(alloc_orig, Ordering::Release);

    let realloc_orig = match unsafe {
        MinHook::create_hook_api("ntdll.dll", "RtlReAllocateHeap", hook_heap_realloc as *mut c_void)
    } {
        Ok(p) => p,
        Err(_) => {
            ATTACHED.store(false, Ordering::Release);
            return 3;
        }
    };
    ORIG_HEAP_REALLOC.store(realloc_orig, Ordering::Release);

    let free_orig = match unsafe {
        MinHook::create_hook_api("ntdll.dll", "RtlFreeHeap", hook_heap_free as *mut c_void)
    } {
        Ok(p) => p,
        Err(_) => {
            ATTACHED.store(false, Ordering::Release);
            return 4;
        }
    };
    ORIG_HEAP_FREE.store(free_orig, Ordering::Release);

    if unsafe { MinHook::enable_all_hooks() }.is_err() {
        ATTACHED.store(false, Ordering::Release);
        return 5;
    }

    0
}

/// Returns 0 on success (including "was not attached" — idempotent), a
/// nonzero failure code otherwise. Leaves the target exactly as if this DLL
/// had never been loaded: hooks fully removed, private heap destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookDetach() -> u32 {
    if !ATTACHED.swap(false, Ordering::AcqRel) {
        return 0; // not attached — idempotent no-op
    }

    // Disable interception FIRST — before the writer thread is asked to
    // stop, not after. Confirmed empirically as a required ordering, the
    // mirror image of the writer-thread-*creation* hazard documented on
    // `heaplens_alloc::ensure_writer`: a thread *exiting* naturally also
    // triggers DLL_THREAD_DETACH and TLS-destructor cleanup on that thread,
    // which itself performs heap operations. If the hook is still active
    // when the writer thread terminates (the ordering this replaces), those
    // cleanup-time heap calls route through our detour during the exact
    // window a thread is mid-teardown — the same class of hazard as
    // hooking a thread's DLL_THREAD_ATTACH bootstrap, just at the other
    // end of the thread's life. Disabling first means the writer thread's
    // own exit-time heap traffic goes through the real, unhooked functions.
    // `MinHook::disable_all_hooks` un-redirects `HeapAlloc`/`HeapReAlloc`/
    // `HeapFree` but does not yet free the trampolines — `ORIG_HEAP_*` stay
    // valid, so `PrivateHeapAlloc` (still in use by the writer thread until
    // it actually stops) keeps working correctly through this window.
    let _ = unsafe { MinHook::disable_all_hooks() };

    // Now it's safe to let the writer thread exit — its own DLL_THREAD_DETACH
    // traffic no longer passes through any detour. Confirmed empirically as
    // a required step in its own right (not just reordered): leaving the
    // writer running through process/DLL teardown crashes even after every
    // other capture-path hazard is fixed. See the safety note on
    // `heaplens_alloc::request_writer_stop_and_wait`. The timeout's
    // fallback mirrors the launcher's own bounded-detach pattern (§4.6):
    // never let a hung wait block teardown indefinitely.
    let stopped = heaplens_alloc::request_writer_stop_and_wait(std::time::Duration::from_secs(2));
    if !stopped {
        // Hooks are already disabled (safe either way at this point), but
        // do NOT free MinHook's trampolines or the private heap: the
        // writer thread may still be executing code that depends on both.
        // Leave ATTACHED false — a stuck writer thread means this DLL
        // instance cannot cleanly detach; the caller should not retry into
        // the same, already-compromised state.
        return 6;
    }

    // Writer confirmed stopped. Now safe to fully uninitialize MinHook
    // (frees its trampolines) and destroy the private heap.
    MinHook::uninitialize();

    ORIG_HEAP_ALLOC.store(std::ptr::null_mut(), Ordering::Release);
    ORIG_HEAP_REALLOC.store(std::ptr::null_mut(), Ordering::Release);
    ORIG_HEAP_FREE.store(std::ptr::null_mut(), Ordering::Release);

    let heap = PRIVATE_HEAP.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if !heap.is_null() {
        unsafe { HeapDestroy(heap as HANDLE) };
    }

    0
}
