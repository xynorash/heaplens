//! Stage 7 capture front-end for injected (uncooperative) processes.
//!
//! Loaded into a target process by `heaplens-injector` and driven via the
//! exported entry points below — never from `DllMain`, per
//! `docs/stage7-injection-design.md` §4.1 (loader-lock deadlock hazard).
//! `HeapLensHookAttach`/`HeapLensHookAttachRemote` install MinHook
//! trampolines on `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` and
//! drive the same capture pipeline (`heaplens_alloc::record`, the ring
//! buffer, the writer thread) that the cooperative `#[global_allocator]`
//! producer uses — only the interception mechanism differs. Two attach
//! entry points exist (rather than one) because a raw `CreateRemoteThread`
//! thread and a normal in-process thread have different constraints on how
//! they may safely return — see `HeapLensHookAttachRemote`'s doc comment.
//! `HeapLensHookDetach` (in-process callers) and `HeapLensHookDetachApc`
//! (the `QueueUserAPC`-driven path `heaplens-injector` actually uses, see
//! `WORKER_TID`'s doc comment) both remove the hooks and leave the target
//! exactly as if this DLL had never loaded.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};

use minhook::MinHook;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapAlloc, HeapCreate, HeapDestroy, HeapFree, HeapReAlloc};
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId, SleepEx, TerminateThread};

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

// ── Cross-process detach handshake (§8 Step 2) ─────────────────────────
//
// `heaplens-injector` cannot safely run `HeapLensHookDetach` via a second
// `CreateRemoteThread` call while hooks are active: creating that thread
// at all — regardless of what code it runs — makes the OS deliver
// `DLL_THREAD_ATTACH` to every loaded DLL on it (including `ucrtbase`'s own
// per-thread setup) *before* our code gets control, and that notification's
// own heap traffic reenters our still-active detour on a `CreateRemoteThread`
// -created "raw" thread — the same class of stack-walk hazard documented on
// `HeapLensHookAttach`'s own exit path, just at start instead of end.
// Confirmed empirically: even a `HeapLensHookDetach` body reduced to an
// immediate `return 42` (no spawn, no real work) still crashed identically
// whenever hooks were active at the moment the injector's second
// `CreateRemoteThread` call created the thread — proving the fault is in the
// automatic notification, not anything this module's code does.
//
// The fix avoids creating a second raw thread at all: `attach_impl`'s own
// worker thread (already alive, already a normal, properly CRT-initialized
// Rust thread — safe by construction, unlike a `CreateRemoteThread` thread)
// parks itself in an *alertable* wait instead of an inert one, and exposes
// its OS thread ID here so `heaplens-injector` can `QueueUserAPC` detach
// work directly onto it from outside the process — no new thread, so no
// `DLL_THREAD_ATTACH` hazard. `HeapLensHookDetachApc` below is the queued
// callback; `DETACH_RESULT` is how the injector (which cannot receive a
// return value from a queued APC) reads the outcome back via
// `ReadProcessMemory`, polling until it leaves its `-1` "pending" sentinel.
#[unsafe(no_mangle)]
pub static WORKER_TID: AtomicU32 = AtomicU32::new(0);
#[unsafe(no_mangle)]
pub static DETACH_RESULT: AtomicI32 = AtomicI32::new(-1);

// A size no real caller would ever request, used to prove the trampoline
// actually works end to end (real call -> detour invoked -> real function
// executed -> detour returns the correct result) before trusting the hook
// with a real target. MinHook reporting "enable succeeded" is not
// sufficient evidence on its own — that is exactly the failure mode found
// hooking the kernelbase!HeapAlloc layer (§1.1): every install/enable step
// reported success while the generated trampoline was broken, and the
// first real call after enable crashed. See `HeapLensHookAttach`'s canary
// check, run once per attach, before any real capture is trusted.
const CANARY_SIZE: usize = 0xC0FFEE;
static CANARY_HIT: AtomicBool = AtomicBool::new(false);

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
    if dwbytes == CANARY_SIZE {
        // The attach-time canary (below): proves this detour actually ran
        // and the trampoline actually produced a real allocation. Not a
        // real event — never recorded, so it never reaches the wire.
        CANARY_HIT.store(true, Ordering::Release);
        return ptr;
    }
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
//
// **Both exported functions are thin shims — real work never runs directly
// on the thread `CreateRemoteThread` created.** Confirmed empirically as a
// fifth, distinct hazard (Step 2): calling `HeapLensHookAttach`'s real body
// directly via `CreateRemoteThread` crashed (`STATUS_STACK_BUFFER_OVERRUN`,
// the same fastfail signature observed for the writer-thread hazards)
// even though the identical code, called via `GetProcAddress` + a direct
// function call *within* the same process (Step 1's self-load harness),
// worked correctly. Root cause: Rust's runtime assumes threads are created
// through its own path (ultimately the CRT's `_beginthreadex`), which
// performs per-thread setup — a properly sized/guarded stack, TLS
// bookkeeping — that a bare `CreateRemoteThread` thread never receives.
// Running MinHook calls, private-heap creation, and the writer/canary
// logic directly on such a thread is exactly the kind of substantial,
// assumption-laden work that hazard breaks. The fix is the standard
// mitigation: the exported function does only `std::thread::spawn` (a
// *real*, properly-initialized Rust thread) and blocks on `join()` for the
// result — all the real logic runs on that spawned thread, never on the
// raw `CreateRemoteThread` thread itself.

/// Returns 0 on success, a nonzero failure code otherwise.
///
/// **Confirmed empirically as a sixth, distinct hazard** — the spawned
/// worker thread's *own natural exit* crashed even after `attach_impl`'s
/// entire body completed successfully (traced via diagnostics: every step
/// up to and including a successful canary check printed, yet the
/// injector still reported an access violation). Root cause: a thread
/// exiting triggers `DLL_THREAD_DETACH`/TLS cleanup on that thread, the
/// same hazard class as `ensure_writer`/`request_writer_stop_and_wait` —
/// except there the fix was "disable hooks before the thread is allowed to
/// exit." That fix does not apply here: a *successful* attach must leave
/// hooks active on return, so the worker thread cannot disable them before
/// exiting without undoing the whole point of attaching. The only
/// remaining option is to never let this thread exit at all: it sends its
/// result back over a channel, then waits forever rather than returning.
/// One permanently-parked thread per attach is an acceptable, one-time
/// cost, cleaned up naturally when the DLL is eventually unloaded.
///
/// It waits *alertably* (`SleepEx(INFINITE, TRUE)`), not via
/// `std::thread::park()` — see `WORKER_TID`'s doc comment above: this same
/// thread is later reused, via `QueueUserAPC`, to run detach's real work,
/// specifically to avoid creating a second `CreateRemoteThread`-spawned raw
/// thread while hooks are active.
/// Shared core: spawns the worker thread, records its TID (for detach's
/// later `QueueUserAPC`, see `WORKER_TID`'s doc comment), and blocks for its
/// result. Common to both exported entry points below — they differ only in
/// how the *calling* thread is allowed to return afterward.
fn attach_and_wait() -> u32 {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().spawn(move || {
        WORKER_TID.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        let rc = attach_impl();
        let _ = tx.send(rc);
        loop {
            unsafe { SleepEx(u32::MAX, 1) };
        }
    });
    if spawned.is_err() {
        return u32::MAX;
    }
    rx.recv().unwrap_or(u32::MAX)
}

/// Direct, in-process entry point: call this from a normal thread (e.g. a
/// self-load harness's own `main`) that is safe to simply return from
/// afterward. **Must not be used as a `CreateRemoteThread` start routine**
/// — see `HeapLensHookAttachRemote` below for that case; calling *this* one
/// via `CreateRemoteThread` would leave hooks active and then let that raw
/// thread return normally, reintroducing the ninth hazard `Remote` exists
/// to avoid.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookAttach() -> u32 {
    attach_and_wait()
}

/// `CreateRemoteThread`-safe entry point: identical work to
/// `HeapLensHookAttach`, but the calling thread never returns normally
/// afterward.
///
/// A ninth, distinct hazard, found after the eighth's fix
/// (`force_enter_permanent`) failed to resolve an identical crash on this
/// path: diagnostics showed every one of this thread's own post-return heap
/// frees correctly suppressed by the guard (no `capture_stack` ever
/// attempted) — yet the process still crashed with the same
/// `STATUS_STACK_BUFFER_OVERRUN` signature. So the fault is not in anything
/// `record()` does; it is in the mere act of *returning* from this function
/// at all. A normal return from a `CreateRemoteThread` start routine makes
/// the OS call `ExitThread`, which synchronously delivers
/// `DLL_THREAD_DETACH` to every loaded DLL on this thread before it
/// actually dies — unconditionally, regardless of what our own code does or
/// guards. That notification runs deep inside `ntdll`'s own thread-shutdown
/// path, a calling context this thread (created by raw `CreateRemoteThread`,
/// never touched by the CRT's own thread-init path) is not equipped for;
/// something in that path performs a heap operation that reenters our still
/// -active detour at a point with insufficient stack margin, corrupting it
/// (the fault code is a `/GS` stack-cookie mismatch, not a generic access
/// violation — consistent with an actual overrun, not just "unsafe to
/// walk"). Guarding what *our* code does downstream cannot fix a fault that
/// happens in the OS's own unavoidable teardown sequence.
///
/// The fix is architectural, not another point patch: never let this thread
/// reach that teardown sequence at all. `TerminateThread` on the calling
/// thread itself is explicitly documented to skip `DLL_THREAD_DETACH`
/// notification entirely (unlike a normal return or `ExitThread`) —
/// normally a liability (leaked per-thread cleanup) but exactly the
/// property needed here: this thread has done nothing but spawn the
/// worker, wait for its result, and mark itself permanently guarded, so it
/// owns no resources whose cleanup we need. Its exit code is set from
/// `dwExitCode` exactly as a normal return would set it, so
/// `heaplens-injector`'s `GetExitCodeThread` read is unaffected.
///
/// This must be a **separate** export from `HeapLensHookAttach`, not a
/// flag/branch inside it: self-load's harness calls `HeapLensHookAttach`
/// directly, in-process, on its own long-lived main thread — unconditional
/// `TerminateThread` there kills that thread (and its still-pending
/// workload) outright, which is exactly what broke `hook_self_load_wire`'s
/// gate the first time this fix was applied to the shared function.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookAttachRemote() -> u32 {
    let result = attach_and_wait();
    heaplens_alloc::guard::force_enter_permanent();
    // SAFETY / DO NOT REORDER: this call is only lock-free because nothing
    // between `attach_and_wait()` returning and this line touches a lock or
    // the heap — see docs/stage7-injection-design.md §4.1 ("fifth hazard")
    // for the full proof. `attach_and_wait()`'s `rx.recv()` has already
    // *returned* (its `Receiver::drop` already ran, uninterrupted, as part
    // of that normal return) and `force_enter_permanent()` is a
    // `const`-initialized fast-TLS store with no allocation. If you add ANY
    // code between the two lines above and this `TerminateThread` call —
    // including something that looks allocation-free — re-verify the proof
    // before assuming it still holds: `TerminateThread` skips all normal
    // cleanup (no unwind, no Drop, no lock release), so anything left
    // locked here stays locked for the rest of the target's process
    // lifetime.
    unsafe { TerminateThread(GetCurrentThread(), result) };
    // Per Win32 docs, TerminateThread does not return when the target is
    // the calling thread. This is unreachable in practice; parking keeps
    // the (never-taken) fallback well-defined rather than returning
    // through the now-abandoned normal path.
    loop {
        std::thread::park();
    }
}

fn attach_impl() -> u32 {
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

    // 4. Canary: prove the trampoline actually works before trusting it
    //    with a real target. A broken trampoline must never stay resident
    //    — fail clean and detach rather than leave a corrupting hook
    //    installed. Uses the real (now-hooked) process heap directly, not
    //    the private heap, so this genuinely exercises the detour path a
    //    real caller would take.
    CANARY_HIT.store(false, Ordering::Release);
    let canary_ptr = unsafe { HeapAlloc(GetProcessHeap(), 0, CANARY_SIZE) };
    let canary_ok = !canary_ptr.is_null() && CANARY_HIT.load(Ordering::Acquire);
    if !canary_ptr.is_null() {
        unsafe { HeapFree(GetProcessHeap(), 0, canary_ptr as *const c_void) };
    }
    if !canary_ok {
        let _ = unsafe { MinHook::disable_all_hooks() };
        let _ = heaplens_alloc::request_writer_stop_and_wait(std::time::Duration::from_secs(2));
        MinHook::uninitialize();
        ORIG_HEAP_ALLOC.store(std::ptr::null_mut(), Ordering::Release);
        ORIG_HEAP_REALLOC.store(std::ptr::null_mut(), Ordering::Release);
        ORIG_HEAP_FREE.store(std::ptr::null_mut(), Ordering::Release);
        let heap = PRIVATE_HEAP.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !heap.is_null() {
            unsafe { HeapDestroy(heap as HANDLE) };
        }
        ATTACHED.store(false, Ordering::Release);
        return 7;
    }

    0
}

/// Returns 0 on success (including "was not attached" — idempotent), a
/// nonzero failure code otherwise. Leaves the target exactly as if this DLL
/// had never been loaded: hooks fully removed, private heap destroyed.
///
/// Unlike `HeapLensHookAttach`, this runs `detach_impl` directly on the
/// calling thread rather than spawning a worker — spawning a *new* thread
/// is itself unsafe while hooks are still globally active (a tenth,
/// distinct hazard, the in-process sibling of the one documented on
/// `HeapLensHookAttachRemote`/`WORKER_TID`: the new thread's own automatic
/// `DLL_THREAD_ATTACH` notification reenters the still-active detour before
/// `detach_impl` ever gets a chance to disable it). Confirmed empirically:
/// this function used to spawn a worker (by analogy with `HeapLensHookAttach`
/// — wrongly; `HeapLensHookAttach` needs a spawned thread to avoid running
/// substantial work on a *raw `CreateRemoteThread` thread*, an unrelated
/// concern), and self-load's harness — a direct, in-process, same-thread
/// caller — crashed reliably at exactly that spawn, before `detach_impl`'s
/// own first line ever printed. Running `detach_impl` directly on the
/// caller's thread has no such issue: no new thread is created, so there is
/// no notification to reenter anything. `heaplens-injector` never calls
/// this function via `CreateRemoteThread` for a live (hooked) target for
/// the same reason a spawn is unsafe here — see `HeapLensHookDetachApc`.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookDetach() -> u32 {
    detach_impl()
}

fn detach_impl() -> u32 {
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

    // Writer confirmed stopped — no other thread is concurrently pushing
    // to or draining any ring now. Tear down the per-thread ring-storage
    // mechanism itself before there's any chance this DLL gets unloaded;
    // see `heaplens_alloc::shutdown_ring_storage`'s doc comment for the
    // crash this closes (a stale FLS registration whose callback lives in
    // this DLL, invoked after the DLL may already be unloaded).
    heaplens_alloc::shutdown_ring_storage();

    // Now safe to fully uninitialize MinHook (frees its trampolines) and
    // destroy the private heap.
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

/// Queued via `QueueUserAPC` onto `attach_impl`'s worker thread (see
/// `WORKER_TID`'s doc comment) — the primary path `heaplens-injector` uses
/// to drive a real detach while hooks are active, avoiding the
/// `DLL_THREAD_ATTACH`-on-a-fresh-`CreateRemoteThread`-thread hazard that a
/// second `CreateRemoteThread` call would trigger. Matches `PAPCFUNC`'s
/// required signature (`unsafe extern "system" fn(usize)`, no return value)
/// — the result is published via `DETACH_RESULT` instead, since a queued
/// APC has no return channel back to the process that queued it.
///
/// `HeapLensHookDetach` (above) is kept for direct, in-process callers
/// (e.g. a future self-load-style test) where hooks being active at a raw
/// thread's creation is not a concern because no new thread is involved.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn HeapLensHookDetachApc(_param: usize) {
    let rc = detach_impl();
    DETACH_RESULT.store(rc as i32, Ordering::Release);
}
