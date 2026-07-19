//! Stage 7 Step 2 acceptance-gate target (docs/stage7-injection-design.md
//! §8, Step 2). A plain, separate process — built with debug info (dev
//! profile default), no dependency on `heaplens-hook` or `heaplens-alloc`
//! — that an external `heaplens-injector` process attaches to via real
//! `CreateRemoteThread`/`LoadLibraryW` injection. This is the first real
//! cross-process test: Step 1 proved the capture pipeline in isolation via
//! self-load; this proves the injector actually gets a hook DLL into
//! *another* process and that capture still works from there.
//!
//! Prints synchronization markers to stdout so the driving test can inject
//! and detach at the right points without guessing timing, plus the same
//! per-operation ALLOC/REALLOC/FREE/POST_DETACH_* markers Step 1's harness
//! uses, for the same pointer-identity-matching reason (see
//! self_load_harness.rs's module doc).

use std::alloc::{GlobalAlloc, Layout, System};

const SIZES: [usize; 4] = [32, 64, 128, 256];
const ALLOC_COUNT: usize = 50;
const REALLOC_COUNT: usize = 10;

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
    println!("TARGET_PID={}", std::process::id());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving test to spawn `heaplens-injector --attach`.
    std::thread::sleep(std::time::Duration::from_millis(800));

    let mut pre_attach_ptr = unsafe { alloc_n(96) };

    // ── Scripted, precisely-countable workload (captured, if attach landed
    // in time — same shape as self_load_harness.rs) ─────────────────────
    let mut ptrs: [*mut u8; ALLOC_COUNT] = [std::ptr::null_mut(); ALLOC_COUNT];
    let mut sizes: [usize; ALLOC_COUNT] = [0; ALLOC_COUNT];
    for i in 0..ALLOC_COUNT {
        let size = SIZES[i % SIZES.len()];
        ptrs[i] = unsafe { alloc_n(size) };
        sizes[i] = size;
        println!("ALLOC ptr={:?} size={size}", ptrs[i]);
    }

    for (i, sizes_i) in sizes.iter_mut().enumerate().take(REALLOC_COUNT) {
        let old_ptr = ptrs[i];
        let new_size = *sizes_i * 2;
        ptrs[i] = unsafe { realloc_n(ptrs[i], *sizes_i, new_size) };
        *sizes_i = new_size;
        println!("REALLOC old={old_ptr:?} new={:?} size={new_size}", ptrs[i]);
    }

    let untracked_old = pre_attach_ptr;
    pre_attach_ptr = unsafe { realloc_n(pre_attach_ptr, 96, 192) };
    println!("REALLOC_UNTRACKED old={untracked_old:?} new={pre_attach_ptr:?} size=192");

    for i in 0..ALLOC_COUNT {
        unsafe { free_n(ptrs[i], sizes[i]) };
        println!("FREE ptr={:?}", ptrs[i]);
    }
    unsafe { free_n(pre_attach_ptr, 192) };
    println!("FREE ptr={pre_attach_ptr:?}");

    // Flush window before signaling the driving test to detach.
    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("WORKLOAD_DONE");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving test to spawn `heaplens-injector --detach`.
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // ── Post-detach workload — must produce zero captured events ────────
    let mut post_ptrs: [*mut u8; 5] = [std::ptr::null_mut(); 5];
    for p in post_ptrs.iter_mut() {
        *p = unsafe { alloc_n(64) };
        println!("POST_DETACH_ALLOC ptr={p:?} size=64");
    }
    for p in post_ptrs {
        unsafe { free_n(p, 64) };
        println!("POST_DETACH_FREE ptr={p:?}");
    }

    std::thread::sleep(std::time::Duration::from_millis(300));
    println!("injection_target: done");
}
