//! Step 1 acceptance-gate harness (docs/stage7-injection-design.md §8,
//! Step 1). Loads `heaplens_hook.dll` into *itself* via `LoadLibraryW` — no
//! cross-process injection yet, that is Step 2 — then drives a scripted,
//! precisely-countable allocation workload through the now-hooked
//! `HeapAlloc`/`HeapReAlloc`/`HeapFree`, detaches, and runs a second
//! workload that must produce zero captured events.
//!
//! This binary does not link `heaplens_hook` as a Rust library (it's a
//! cdylib, loaded dynamically like any injection target would load it) and
//! does not use `heaplens-alloc` as its global allocator — its allocations
//! go through the plain `std::alloc::System` (Win32 `HeapAlloc` family),
//! exactly like an uncooperative target's would.

use std::alloc::{GlobalAlloc, Layout, System};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type AttachFn = unsafe extern "system" fn() -> u32;
type DetachFn = unsafe extern "system" fn() -> u32;

const SIZES: [usize; 4] = [32, 64, 128, 256];
const ALLOC_COUNT: usize = 50;
const REALLOC_COUNT: usize = 10;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn dll_path() -> std::path::PathBuf {
    // This harness builds to target/{profile}/examples/self_load_harness.exe;
    // heaplens_hook.dll (the package's cdylib target) builds one level up,
    // to target/{profile}/heaplens_hook.dll.
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

unsafe fn realloc_n(ptr: *mut u8, old_size: usize, new_size: usize) -> *mut u8 {
    let old_layout = Layout::from_size_align(old_size, 8).unwrap();
    unsafe { System.realloc(ptr, old_layout, new_size) }
}

unsafe fn free_n(ptr: *mut u8, size: usize) {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.dealloc(ptr, layout) };
}

fn main() {
    // Pre-attach allocation — never observed by the hook. Reallocated below,
    // after attach, to exercise the realloc-of-unknown-pointer fallback
    // (docs/stage7-injection-design.md §1.4, `Graph::on_realloc`).
    let mut pre_attach_ptr = unsafe { alloc_n(96) };

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

    let rc = unsafe { attach() };
    assert_eq!(rc, 0, "HeapLensHookAttach failed with code {rc}");

    // Let the writer thread connect + handshake before generating events.
    std::thread::sleep(std::time::Duration::from_millis(300));

    // ── Scripted, precisely-countable workload (captured) ───────────────
    // Every operation's exact pointer/size is printed to stdout in a
    // simple, parseable form. The capture mechanism hooks HeapAlloc/
    // HeapReAlloc/HeapFree *process-wide* — it cannot distinguish this
    // workload's own calls from Windows' own internal heap traffic
    // triggered as a side effect of the writer thread's pipe I/O (observed
    // empirically: extra captured events with sizes never requested here).
    // The daemon-side test filters to exactly these pointers rather than
    // asserting a brittle global total, so ambient OS-level noise doesn't
    // make the gate flaky. See docs/stage7-injection-design.md §8 Step 1.

    // Exactly ALLOC_COUNT Alloc events, sizes cycling through SIZES.
    let mut ptrs: [*mut u8; ALLOC_COUNT] = [std::ptr::null_mut(); ALLOC_COUNT];
    let mut sizes: [usize; ALLOC_COUNT] = [0; ALLOC_COUNT];
    for i in 0..ALLOC_COUNT {
        let size = SIZES[i % SIZES.len()];
        ptrs[i] = unsafe { alloc_n(size) };
        sizes[i] = size;
        println!("ALLOC ptr={:?} size={size}", ptrs[i]);
    }

    // Exactly REALLOC_COUNT Realloc events on tracked pointers.
    for (i, sizes_i) in sizes.iter_mut().enumerate().take(REALLOC_COUNT) {
        let old_ptr = ptrs[i];
        let new_size = *sizes_i * 2;
        ptrs[i] = unsafe { realloc_n(ptrs[i], *sizes_i, new_size) };
        *sizes_i = new_size;
        println!("REALLOC old={old_ptr:?} new={:?} size={new_size}", ptrs[i]);
    }

    // Exactly 1 Realloc event on the untracked pre-attach pointer.
    let untracked_old = pre_attach_ptr;
    pre_attach_ptr = unsafe { realloc_n(pre_attach_ptr, 96, 192) };
    println!("REALLOC_UNTRACKED old={untracked_old:?} new={pre_attach_ptr:?} size=192");

    // Exactly ALLOC_COUNT + 1 Dealloc events.
    for i in 0..ALLOC_COUNT {
        unsafe { free_n(ptrs[i], sizes[i]) };
        println!("FREE ptr={:?}", ptrs[i]);
    }
    unsafe { free_n(pre_attach_ptr, 192) };
    println!("FREE ptr={pre_attach_ptr:?}");

    // Let the writer thread flush the batch before detaching.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let rc = unsafe { detach() };
    assert_eq!(rc, 0, "HeapLensHookDetach failed with code {rc}");

    // ── Post-detach workload — must produce zero captured events ────────
    // Pointers printed with a distinct marker so the test can positively
    // assert they were never captured (proving hooks were actually
    // removed) rather than relying on their absence from the main workload
    // sets, which coincidental pointer reuse could make ambiguous.
    let mut post_ptrs: [*mut u8; 5] = [std::ptr::null_mut(); 5];
    for p in post_ptrs.iter_mut() {
        *p = unsafe { alloc_n(64) };
        println!("POST_DETACH_ALLOC ptr={p:?} size=64");
    }
    for p in post_ptrs {
        unsafe { free_n(p, 64) };
        println!("POST_DETACH_FREE ptr={p:?}");
    }

    // Give the daemon a moment to receive the final captured batch.
    std::thread::sleep(std::time::Duration::from_millis(300));

    println!("self_load_harness: done");
}
