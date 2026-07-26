//! Targeted repro for a specific, named, still-unconfirmed hazard in
//! ring.rs's own doc comment: a live FLS registration (its exit callback
//! lives inside heaplens_hook.dll) could in principle fire *after* the DLL
//! is unmapped, if a thread that made a hooked allocation is still mid-exit
//! exactly as the process tears down and Windows' informal (not guaranteed)
//! unmap-vs-FLS-callback ordering doesn't cooperate.
//!
//! Not a general stress test — general concurrent load (see
//! injection_target_concurrent.rs) doesn't target this specific window.
//! This maximizes it directly: spawn many short-lived threads, each
//! registering exactly one fresh FLS slot (one hooked allocation) then
//! exiting immediately, *never joined* by main — so their own natural
//! thread-exit (and FLS callback) races the process's own hard, immediate
//! exit (and DLL unmap) as tightly as possible. No explicit detach.

use std::alloc::{GlobalAlloc, Layout, System};
use std::time::Duration;

const THREADS_PER_RUN: usize = 200;

unsafe fn alloc_n(size: usize) -> *mut u8 {
    let layout = Layout::from_size_align(size, 8).unwrap();
    unsafe { System.alloc(layout) }
}

fn main() {
    println!("TARGET_PID={}", std::process::id());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Window for the driving script to attach.
    std::thread::sleep(Duration::from_millis(500));

    for _ in 0..THREADS_PER_RUN {
        // Spawned, never joined -- dropping the JoinHandle detaches it, the
        // thread keeps running independently. That's the point: its exit
        // (and FLS callback) is left to race the process's own teardown.
        std::thread::spawn(|| {
            let p = unsafe { alloc_n(32) };
            std::hint::black_box(p);
        });
    }

    println!("SPAWNED {THREADS_PER_RUN} unjoined threads, hard-exiting immediately, no detach");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Hard, immediate exit -- skips Rust's normal unwind/cleanup, closer to
    // how real process teardown (ExitProcess) actually happens, maximizing
    // concurrency pressure between the spawned threads' own exits and this
    // process's teardown/DLL-unmap sequence.
    std::process::exit(0);
}
