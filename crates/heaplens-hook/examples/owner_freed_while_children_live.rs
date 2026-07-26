//! Stage 7 hook-crash repro target (docs/stage7-injection-design.md, hook
//! owner-free crash diagnosis). A plain, separate process — no dependency
//! on heaplens-hook or heaplens-alloc — built for real cross-process
//! injection via `heaplens-injector --attach`, mirroring
//! injection_target.rs's structure (TARGET_PID marker, sleep windows for
//! the driving script to attach/detach).
//!
//! Workload pattern ("case 2" as reconstructed): an owner allocation is
//! made first, then several children whose allocation call stack differs
//! from the owner's (so phi's ownership inference has something to link),
//! then the owner alone is freed while the children remain live — followed
//! by continued heap churn (realloc/alloc/free) so that any hook-side
//! reentrancy or bookkeeping bug tied to freeing an "owner" node while
//! children are still tracked gets more than one chance to fire.
//!
//! No fix is attempted here — this is a repro target only, driven under
//! the temporary diagnostic VEH added to heaplens-hook for this
//! investigation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::time::Duration;

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

#[inline(never)]
unsafe fn make_owner() -> *mut u8 {
    unsafe { alloc_n(96) }
}

#[inline(never)]
unsafe fn make_children() -> Vec<*mut u8> {
    (0..8).map(|_| unsafe { alloc_n(64) }).collect()
}

fn main() {
    println!("TARGET_PID={}", std::process::id());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving script to spawn `heaplens-injector --attach`.
    std::thread::sleep(Duration::from_millis(800));

    // ── Owner allocated first, so phi's ownership inference (which only
    // considers already-live nodes) can assign it as the children's
    // effective owner. ──────────────────────────────────────────────────
    let owner = unsafe { make_owner() };
    println!("ALLOC owner ptr={owner:?} size=96");

    let children = unsafe { make_children() };
    for c in &children {
        println!("ALLOC child ptr={c:?} size=64");
    }

    std::thread::sleep(Duration::from_millis(300));

    // ── The operation under test: free the owner while children remain
    // live. If there is a hook-side bug tied specifically to this
    // transition (as opposed to ordinary alloc/free traffic), this is
    // where it would fire. ──────────────────────────────────────────────
    unsafe { free_n(owner, 96) };
    println!("FREE owner ptr={owner:?}");

    // ── Continued churn after the owner-free, so a reentrancy/bookkeeping
    // bug gets repeated chances rather than just one. ───────────────────
    let mut churn_ptrs: Vec<(*mut u8, usize)> = Vec::new();
    for round in 0..20 {
        let p = unsafe { alloc_n(48) };
        churn_ptrs.push((p, 48));
        println!("ALLOC churn round={round} ptr={p:?} size=48");

        if let Some((last_ptr, last_size)) = churn_ptrs.pop() {
            let new_size = last_size * 2;
            let new_ptr = unsafe { realloc_n(last_ptr, last_size, new_size) };
            println!("REALLOC churn round={round} old={last_ptr:?} new={new_ptr:?} size={new_size}");
            churn_ptrs.push((new_ptr, new_size));
        }

        std::thread::sleep(Duration::from_millis(30));
    }
    for (p, size) in churn_ptrs {
        unsafe { free_n(p, size) };
        println!("FREE churn ptr={p:?}");
    }

    // ── Children freed last, well after the owner — confirms the process
    // survives past the owner-free/churn window before we declare success. ─
    for c in &children {
        unsafe { free_n(*c, 64) };
        println!("FREE child ptr={c:?}");
    }

    std::thread::sleep(Duration::from_millis(300));
    println!("WORKLOAD_DONE");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving script to spawn `heaplens-injector --detach`.
    std::thread::sleep(Duration::from_millis(2000));

    println!("owner_freed_while_children_live: done");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}
