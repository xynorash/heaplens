//! Regression test for the 2026-07-21 root-cause investigation
//! (`self_load_concurrency_stress.rs`'s finding): every allocation in this
//! harness's scripted workload happens on a **spawned** thread, never the
//! main thread — the main thread only attaches, spawns, joins, and
//! detaches. `self_load_harness.rs` (the original Step 1 acceptance gate)
//! never exercised this: its entire scripted workload runs on the process's
//! main thread, the same thread that calls `LoadLibraryW`. That gap is
//! exactly why the bug this file guards against was never caught until a
//! dedicated concurrency stress harness went looking for it — see
//! `guard.rs`/`ring.rs`'s doc comments for the confirmed mechanism
//! (`thread_local!`/plain `FlsAlloc` usage unsafe from any thread other
//! than the one that loaded this DLL) and the fix (raw `TlsAlloc` for the
//! reentrancy guard, `FlsAlloc` with an explicit pre-unload `shutdown` for
//! the ring registry).
//!
//! Deliberately small and fast (a handful of operations, not a multi-second
//! stress run) — this is the *targeted*, deterministic regression test for
//! the specific mechanism; `self_load_concurrency_stress.rs` remains the
//! broader stress/repro tool for the same underlying class of bug.

use std::alloc::{GlobalAlloc, Layout, System};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type AttachFn = unsafe extern "system" fn() -> u32;
type DetachFn = unsafe extern "system" fn() -> u32;

const SIZES: [usize; 4] = [32, 64, 128, 256];
const ALLOC_COUNT: usize = 20;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .and_then(|p| p.parent())
        .expect("cannot determine target profile dir from harness exe path")
        .join("heaplens_hook.dll")
}

unsafe fn alloc_n(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.alloc(layout) }
}

unsafe fn free_n(ptr: *mut u8, size: usize) {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.dealloc(ptr, layout) };
}

fn main() {
    let path = dll_path();
    let wpath = to_wide(path.to_str().expect("dll path is valid UTF-8"));

    let hmodule = unsafe { LoadLibraryW(wpath.as_ptr()) };
    assert!(!hmodule.is_null(), "LoadLibraryW failed for {path:?}");

    let attach: AttachFn = unsafe {
        let proc = GetProcAddress(hmodule, b"HeapLensHookAttach\0".as_ptr());
        std::mem::transmute(proc.expect("GetProcAddress(HeapLensHookAttach) failed"))
    };
    let detach: DetachFn = unsafe {
        let proc = GetProcAddress(hmodule, b"HeapLensHookDetach\0".as_ptr());
        std::mem::transmute(proc.expect("GetProcAddress(HeapLensHookDetach) failed"))
    };

    // Attach happens on the main thread — same as every other self-load
    // harness. The point of divergence from self_load_harness.rs is next.
    let rc = unsafe { attach() };
    assert_eq!(rc, 0, "HeapLensHookAttach failed with code {rc}");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // The entire scripted workload runs on a SPAWNED thread — this is the
    // exact shape that crashed reliably before the fix (confirmed via
    // reduction down to a single allocation on a single spawned thread).
    let worker = std::thread::spawn(|| {
        let mut ptrs: [*mut u8; ALLOC_COUNT] = [std::ptr::null_mut(); ALLOC_COUNT];
        let mut sizes: [usize; ALLOC_COUNT] = [0; ALLOC_COUNT];
        for i in 0..ALLOC_COUNT {
            let size = SIZES[i % SIZES.len()];
            ptrs[i] = unsafe { alloc_n(size) };
            sizes[i] = size;
            println!("ALLOC ptr={:?} size={size}", ptrs[i]);
        }
        for i in 0..ALLOC_COUNT {
            unsafe { free_n(ptrs[i], sizes[i]) };
            println!("FREE ptr={:?}", ptrs[i]);
        }
    });
    worker.join().expect("spawned workload thread panicked");

    std::thread::sleep(std::time::Duration::from_millis(300));

    let rc = unsafe { detach() };
    assert_eq!(rc, 0, "HeapLensHookDetach failed with code {rc}");

    std::thread::sleep(std::time::Duration::from_millis(300));

    println!("self_load_spawned_thread: done");
}
