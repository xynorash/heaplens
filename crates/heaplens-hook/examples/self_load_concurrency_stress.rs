//! Diagnostic-only repro harness for the DPC_WATCHDOG_VIOLATION investigation
//! (2026-07-21). Self-load only (like `self_load_harness.rs`) — loads
//! `heaplens_hook.dll` into *itself* via `LoadLibraryW`, never injects into
//! any other process. This is intentional: this investigation's constraint
//! is "example producers only, or a disposable VM" for cross-process
//! testing, and self-load already exercises the exact same
//! `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` hook path a real
//! injected target would run, just without touching a second process.
//!
//! Spawns many threads (approximating a heavily multi-threaded target like
//! Chrome/Discord/a JVM) all allocating/freeing rapidly and concurrently
//! under the hook, for a fixed duration. Measures each alloc+free pair's
//! wall-clock latency from *outside* the hook (no modification to hook or
//! heaplens-alloc source — this harness only times calls it makes itself),
//! then reports max/p99/outlier-count, first unhooked (baseline) and then
//! hooked, so the added overhead is directly comparable.
//!
//! Not a fix, not a permanent addition — a one-off diagnostic tool for this
//! investigation, per the same "temporary, clearly marked" discipline as
//! `diag_veh.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type AttachFn = unsafe extern "system" fn() -> u32;
type DetachFn = unsafe extern "system" fn() -> u32;

const THREAD_COUNT: usize = 48;
const RUN_DURATION: Duration = Duration::from_secs(8);
const ALLOC_SIZE: usize = 64;

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

/// Runs `THREAD_COUNT` threads, each doing tight alloc/free loops for
/// `RUN_DURATION`, recording every single alloc+free pair's latency in
/// nanoseconds. Returns the flattened latency samples from all threads.
fn run_concurrent_workload() -> Vec<u64> {
    let barrier = Arc::new(Barrier::new(THREAD_COUNT));
    let mut handles = Vec::with_capacity(THREAD_COUNT);

    for _ in 0..THREAD_COUNT {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait(); // start all threads at (approximately) the same instant
            let mut samples: Vec<u64> = Vec::new();
            let deadline = Instant::now() + RUN_DURATION;
            let layout = Layout::from_size_align(ALLOC_SIZE, 8).unwrap();
            while Instant::now() < deadline {
                let t0 = Instant::now();
                let ptr = unsafe { System.alloc(layout) };
                unsafe { System.dealloc(ptr, layout) };
                samples.push(t0.elapsed().as_nanos() as u64);
            }
            samples
        }));
    }

    let mut all: Vec<u64> = Vec::new();
    for h in handles {
        all.extend(h.join().expect("stress thread panicked"));
    }
    all
}

fn summarize(label: &str, mut samples: Vec<u64>) {
    samples.sort_unstable();
    let n = samples.len();
    if n == 0 {
        println!("[{label}] no samples captured");
        return;
    }
    let p50 = samples[n / 2];
    let p99 = samples[(n * 99) / 100];
    let p999 = samples[((n * 999) / 1000).min(n - 1)];
    let max = samples[n - 1];
    let over_1ms = samples.iter().filter(|&&ns| ns > 1_000_000).count();
    let over_10ms = samples.iter().filter(|&&ns| ns > 10_000_000).count();
    let over_100ms = samples.iter().filter(|&&ns| ns > 100_000_000).count();
    let over_1s = samples.iter().filter(|&&ns| ns > 1_000_000_000).count();
    println!(
        "[{label}] n={n} p50={:.3}us p99={:.3}us p999={:.3}us max={:.3}ms | outliers: >1ms={over_1ms} >10ms={over_10ms} >100ms={over_100ms} >1s={over_1s}",
        p50 as f64 / 1_000.0,
        p99 as f64 / 1_000.0,
        p999 as f64 / 1_000.0,
        max as f64 / 1_000_000.0,
    );
}

fn main() {
    println!("self_load_concurrency_stress: {THREAD_COUNT} threads, {RUN_DURATION:?} each, alloc_size={ALLOC_SIZE}");

    // ── Baseline: unhooked, plain System allocator ──────────────────────
    println!("\n=== BASELINE (unhooked) ===");
    let baseline = run_concurrent_workload();
    summarize("baseline", baseline);

    // ── Attach (self-load, in-process, no injection) ────────────────────
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
    std::thread::sleep(Duration::from_millis(300)); // writer thread connect grace period

    println!("\n=== HOOKED (concurrent, {THREAD_COUNT} threads) ===");
    let hooked = run_concurrent_workload();
    summarize("hooked", hooked);

    let rc = unsafe { detach() };
    assert_eq!(rc, 0, "HeapLensHookDetach failed with code {rc}");

    println!("\nself_load_concurrency_stress: done");
}
