//! Multi-threaded companion to `owner_freed_while_children_live.rs` for the
//! `hook_owner_free_no_crash.rs` exit-without-detach regression coverage.
//!
//! The original investigation's repro was single-threaded (all allocations
//! on `main`). Per `guard.rs`'s TLS-fix doc comment, the old `thread_local!`
//! defect specifically crashed *any thread other than the one that called
//! `LoadLibraryW`* — a target's own worker threads are exactly that
//! population, and a single-threaded repro can't exercise it. This target
//! closes that gap: several worker threads, each continuously making
//! hooked allocations, still running when the process exits without an
//! explicit detach. Short duration (seconds, not minutes) — this is
//! regression-test coverage, not a long-duration soak
//! (`injection_target_concurrent.rs` already covers that separately).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const THREAD_COUNT: usize = 8;
const RUN_MILLIS: u64 = 1500;

unsafe fn alloc_n(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.alloc(layout) }
}

unsafe fn free_n(ptr: *mut u8, size: usize) {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.dealloc(ptr, layout) };
}

fn main() {
    println!("TARGET_PID={}", std::process::id());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving script to attach.
    std::thread::sleep(Duration::from_millis(500));

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(THREAD_COUNT);
    for _ in 0..THREAD_COUNT {
        let stop = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let p = unsafe { alloc_n(64) };
                unsafe { free_n(p, 64) };
            }
        }));
    }

    println!("WORKLOAD_RUNNING {THREAD_COUNT} threads");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    std::thread::sleep(Duration::from_millis(RUN_MILLIS));

    // Deliberately do NOT stop/join the worker threads and do NOT call
    // detach — the point is to exit with worker threads still actively
    // making hooked allocations, exactly the scenario this regression test
    // exists to cover. `stop` is never set; the threads are abandoned when
    // the process exits.
    println!("WORKLOAD_DONE");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Small window for the driving script to observe WORKLOAD_DONE before
    // this process exits on its own (no detach) — mirrors
    // owner_freed_while_children_live.rs's structure.
    std::thread::sleep(Duration::from_millis(200));

    println!("exit_without_detach_multithreaded: exiting now, no detach, {THREAD_COUNT} threads still live");
    let _ = std::io::Write::flush(&mut std::io::stdout());
}
