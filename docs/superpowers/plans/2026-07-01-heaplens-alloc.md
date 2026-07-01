# HeapLens Stage 2 — `heaplens-alloc` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the `heaplens-alloc` crate — a `#[global_allocator]` that intercepts every allocation and ships raw `AllocEvent` records off-process over a Windows named pipe, without allocating or locking on the hot path.

**Architecture:** `HeapLensAlloc` wraps `std::alloc::System`; every alloc/dealloc/realloc calls `record()`, which (under a per-thread RAII recursion guard) captures a monotonic timestamp and up to 8 raw stack IPs, then pushes an `AllocEvent` onto a per-thread SPSC lock-free ring. A single background writer thread drains all rings, resolves symbols in-process via `backtrace::resolve`, and flushes HANDSHAKE / SYMBOLS / EVENTS frames to `\\.\pipe\heaplens`.

**Tech Stack:** Rust 2021, `heaplens-protocol` (path), `backtrace = "0.3"`, `windows-sys = "0.59"` (features: Win32_Foundation, Win32_Storage_FileSystem, Win32_System_Pipes), std only otherwise.

## Global Constraints

- No heap allocation on the critical path (`record` and everything it calls): no `Vec`, `String`, `Box`, `format!`, or any heap-allocating construct.
- No locks on the critical path: lock-free ring only for the producer.
- Never panic in the host process. All error paths are silent returns or `abort`.
- The writer thread permanently holds the recursion guard (first thing it does on entry).
- The ring's backing store is allocated via `std::alloc::System` directly, never via the global allocator.
- Windows only. Platform: x86_64-pc-windows-msvc.
- Edition: Rust 2021. No tokio.

---

## File Map

```
crates/heaplens-alloc/
├── Cargo.toml
└── src/
    ├── lib.rs       — HeapLensAlloc, GlobalAlloc impl, record fn, WRITER_ONCE
    ├── guard.rs     — IN_ALLOC thread_local, ScopedGuard (RAII), force_enter_permanent
    ├── ring.rs      — Ring (SPSC, System-backed), RingHandle (TLS Drop), REGISTRY, push/drain_all
    ├── capture.rs   — START OnceLock, timestamp_nanos, capture_stack
    └── writer.rs    — run() — connect, handshake, drain→resolve→flush loop
    
crates/heaplens-alloc/examples/
└── alloc_smoke.rs   — sets #[global_allocator], allocates in a loop, must not crash with no daemon
```

**Interfaces between modules:**
- `guard`: `is_set() -> bool`, `ScopedGuard::enter() -> ScopedGuard`, `force_enter_permanent()`
- `ring`: `push(AllocEvent) -> bool` (producer; TLS-routed, lock-free), `drain_all(FnMut(AllocEvent))` (writer only)
- `capture`: `timestamp_nanos() -> u64`, `capture_stack() -> ([u64; 8], u8)`
- `writer`: `run()` — the thread entry point

---

## Task 1: Workspace + crate scaffold

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Create: `crates/heaplens-alloc/Cargo.toml`
- Create: `crates/heaplens-alloc/src/lib.rs`
- Create: `crates/heaplens-alloc/src/guard.rs`
- Create: `crates/heaplens-alloc/src/ring.rs`
- Create: `crates/heaplens-alloc/src/capture.rs`
- Create: `crates/heaplens-alloc/src/writer.rs`

**Interfaces:**
- Produces: compilable skeleton that `cargo build -p heaplens-alloc` accepts

- [ ] **Step 1: Add crate to workspace**

Edit `Cargo.toml` (workspace root): change `# TODO: crates/heaplens-alloc` to `"crates/heaplens-alloc"`.

```toml
[workspace]
resolver = "2"
members = [
    "crates/heaplens-protocol",
    "crates/heaplens-alloc",
    # TODO: crates/heaplens-daemon
]
```

- [ ] **Step 2: Create `crates/heaplens-alloc/Cargo.toml`**

```toml
[package]
name = "heaplens-alloc"
version = "0.1.0"
edition = "2021"

[dependencies]
heaplens-protocol = { path = "../heaplens-protocol" }
backtrace = "0.3"
windows-sys = { version = "0.59", features = [
    "Win32_Foundation",
    "Win32_Storage_FileSystem",
    "Win32_System_Pipes",
] }
```

- [ ] **Step 3: Create stub source files**

`crates/heaplens-alloc/src/guard.rs`:
```rust
// stub
```

`crates/heaplens-alloc/src/ring.rs`:
```rust
// stub
```

`crates/heaplens-alloc/src/capture.rs`:
```rust
// stub
```

`crates/heaplens-alloc/src/writer.rs`:
```rust
pub fn run() { loop { std::thread::sleep(std::time::Duration::from_secs(1)); } }
```

`crates/heaplens-alloc/src/lib.rs`:
```rust
mod guard;
mod ring;
mod capture;
mod writer;

use std::alloc::{GlobalAlloc, Layout, System};
use heaplens_protocol::EventKind;

/// Global allocator that intercepts all allocations and ships events
/// off-process over a named pipe without blocking.
///
/// Usage:
/// ```no_run
/// use heaplens_alloc::HeapLensAlloc;
/// #[global_allocator]
/// static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();
/// ```
pub struct HeapLensAlloc;

impl HeapLensAlloc {
    pub const fn new() -> Self { HeapLensAlloc }
}

unsafe impl GlobalAlloc for HeapLensAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        System.realloc(ptr, layout, new_size)
    }
}
```

- [ ] **Step 4: Verify compilation**

```
cargo build -p heaplens-alloc
```

Expected: `Compiling heaplens-alloc` then `Finished`.

- [ ] **Step 5: Commit**

```
git add Cargo.toml crates/heaplens-alloc/
git commit -m "feat(alloc): crate scaffold — Cargo.toml and stub modules"
```

---

## Task 2: `guard.rs` — recursion guard

**Files:**
- Modify: `crates/heaplens-alloc/src/guard.rs`

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub fn is_set() -> bool`
  - `pub fn force_enter_permanent()`
  - `pub struct ScopedGuard(());` with `pub fn enter() -> Self` and `impl Drop`

- [ ] **Step 1: Write tests first**

Add to `crates/heaplens-alloc/src/guard.rs`:

```rust
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
}
```

- [ ] **Step 2: Run tests — expect failure**

```
cargo test -p heaplens-alloc guard
```

Expected: compilation error (guard.rs is a stub).

- [ ] **Step 3: Implement `guard.rs`**

Replace `crates/heaplens-alloc/src/guard.rs` with:

```rust
use std::cell::Cell;

thread_local! {
    static IN_ALLOC: Cell<bool> = const { Cell::new(false) };
}

/// Returns true if the current thread is already inside `record`.
#[inline]
pub fn is_set() -> bool {
    IN_ALLOC.with(|f| f.get())
}

/// Permanently marks the current thread as inside-allocator.
/// Called once by the writer thread at startup so that none of its own
/// allocations (pipe buffers, symbol resolution, etc.) are ever recorded.
pub fn force_enter_permanent() {
    IN_ALLOC.with(|f| f.set(true));
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
        IN_ALLOC.with(|f| f.set(true));
        ScopedGuard(())
    }
}

impl Drop for ScopedGuard {
    #[inline]
    fn drop(&mut self) {
        IN_ALLOC.with(|f| f.set(false));
    }
}

#[cfg(test)]
mod tests {
    // ... (same as Step 1)
}
```

- [ ] **Step 4: Run tests — expect all pass**

```
cargo test -p heaplens-alloc guard
```

Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```
git add crates/heaplens-alloc/src/guard.rs
git commit -m "feat(alloc): guard.rs — recursion guard with panic-safe RAII ScopedGuard"
```

---

## Task 3: `ring.rs` — SPSC ring + thread-local registry

**Files:**
- Modify: `crates/heaplens-alloc/src/ring.rs`

**Interfaces:**
- Consumes: `heaplens_protocol::AllocEvent`
- Produces:
  - `pub const CAP: usize`
  - `pub struct Ring` with `pub fn push(&self, ev: AllocEvent) -> bool`, `pub fn pop(&self) -> Option<AllocEvent>`, `pub dropped: AtomicU64`, `pub producer_alive: AtomicBool`
  - `pub fn push(ev: AllocEvent) -> bool` (module-level, TLS-routed)
  - `pub fn drain_all(f: impl FnMut(AllocEvent))` (writer-side)

- [ ] **Step 1: Write tests first**

The ring tests exercise the `Ring` struct directly (not through TLS) to avoid interaction with the global allocator. Add a `#[cfg(test)]` block at the bottom of `ring.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use heaplens_protocol::{AllocEvent, EventKind};

    fn ev(n: u64) -> AllocEvent {
        AllocEvent::new(EventKind::Alloc, n, 0, 64, 8, n, [0u64; 8], 0)
    }

    #[test]
    fn push_pop_roundtrip() {
        let ring = Ring::new();
        assert!(ring.push(ev(1)));
        assert!(ring.push(ev(2)));
        assert_eq!(ring.pop().unwrap().ptr, 1);
        assert_eq!(ring.pop().unwrap().ptr, 2);
        assert!(ring.pop().is_none());
    }

    #[test]
    fn fifo_order() {
        let ring = Ring::new();
        for i in 0..10u64 {
            ring.push(ev(i));
        }
        for i in 0..10u64 {
            assert_eq!(ring.pop().unwrap().ptr, i);
        }
    }

    #[test]
    fn full_ring_returns_false_and_increments_dropped() {
        let ring = Ring::new();
        // Fill to capacity-1 (ring has CAP slots but can hold CAP-1 items)
        let mut pushed = 0usize;
        for i in 0..CAP as u64 {
            if ring.push(ev(i)) { pushed += 1; } else { break; }
        }
        assert_eq!(pushed, CAP - 1, "ring holds exactly CAP-1 events when full");
        // Next push must fail and increment dropped
        let before = ring.dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert!(!ring.push(ev(99999)));
        let after = ring.dropped.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(after, before + 1);
    }

    #[test]
    fn registry_drain_after_thread_exit() {
        // A spawned thread pushes an event; after the thread exits, drain_all
        // must still return that event (the ring stays alive via Arc in registry).
        let handle = std::thread::spawn(|| {
            // Trigger THREAD_RING init + push by calling the module-level push.
            // Note: this will lock the registry briefly, which is safe here.
            crate::ring::push(ev(0xDEAD));
        });
        handle.join().unwrap();

        // Give TLS destructor time to run and mark producer_alive = false.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let mut found = false;
        drain_all(|e| {
            if e.ptr == 0xDEAD { found = true; }
        });
        assert!(found, "event from exited thread must be drainable");
    }
}
```

- [ ] **Step 2: Run tests — expect compile failure**

```
cargo test -p heaplens-alloc ring
```

Expected: compile error (ring.rs is still a stub).

- [ ] **Step 3: Implement `ring.rs`**

Replace `crates/heaplens-alloc/src/ring.rs` with:

```rust
use std::alloc::{handle_alloc_error, Layout, System};
use std::alloc::GlobalAlloc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use heaplens_protocol::AllocEvent;

/// Ring capacity — must be a power of two.
pub const CAP: usize = 65_536;

/// SPSC lock-free ring buffer. Backing store is allocated via `System`
/// directly (invariant §12.5). The producer owns `tail`; the consumer owns `head`.
pub struct Ring {
    slots: *mut AllocEvent,
    head: AtomicUsize,
    tail: AtomicUsize,
    pub dropped: AtomicU64,
    pub producer_alive: AtomicBool,
}

// SAFETY: The SPSC protocol guarantees that only the producer writes `tail`
// and only the consumer writes `head`. No two threads touch the same slot
// simultaneously. Raw pointer is therefore safe to send/share across threads.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new() -> Self {
        let layout = Layout::array::<AllocEvent>(CAP).expect("ring layout");
        // SAFETY: layout is non-zero. We check for null (OOM) below.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            handle_alloc_error(layout);
        }
        Ring {
            slots: ptr as *mut AllocEvent,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            dropped: AtomicU64::new(0),
            producer_alive: AtomicBool::new(true),
        }
    }

    /// Producer side (hot path). Returns false if ring is full (event dropped).
    /// Never allocates, never blocks.
    #[inline]
    pub fn push(&self, ev: AllocEvent) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) & (CAP - 1);
        if next == self.head.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: `tail < CAP`; slots is a valid array of CAP elements.
        // The producer owns slot[tail]; the consumer won't read it until
        // the tail store (Release) below makes the write visible.
        unsafe { self.slots.add(tail).write(ev) };
        self.tail.store(next, Ordering::Release);
        true
    }

    /// Consumer side (writer thread only). Returns None when ring is empty.
    pub fn pop(&self) -> Option<AllocEvent> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `head < CAP`; the producer wrote slot[head] before
        // advancing tail (Release), and we observed that advance (Acquire).
        let ev = unsafe { self.slots.add(head).read() };
        self.head.store((head + 1) & (CAP - 1), Ordering::Release);
        Some(ev)
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        let layout = Layout::array::<AllocEvent>(CAP).expect("ring layout");
        // SAFETY: `self.slots` was allocated with this exact layout.
        unsafe { System.dealloc(self.slots as *mut u8, layout) };
    }
}

// ── Registry ────────────────────────────────────────────────────────────────

/// All live rings. The writer drains these; producers register at TLS init.
/// A Mutex here is acceptable — producers never touch the registry on the
/// hot path; registration happens once per thread at first `push`.
static REGISTRY: OnceLock<Mutex<Vec<Arc<Ring>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Arc<Ring>>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// RAII handle held in TLS. On thread exit (Drop), marks the ring
/// producer-dead so the writer knows it can remove the ring after draining.
struct RingHandle(Arc<Ring>);

impl Drop for RingHandle {
    fn drop(&mut self) {
        // Signal writer: producer is gone; remaining events are still readable.
        // Note: the Arc in REGISTRY keeps the Ring alive until drain_all removes it.
        self.0.producer_alive.store(false, Ordering::Release);
    }
}

thread_local! {
    // Initialised lazily on first push(). Init allocates via the global
    // allocator (Arc::new, Vec::push), but the recursion guard is always
    // set before push() is called from record(), so those allocations are
    // suppressed by the guard and never re-enter record().
    //
    // Windows note: the Drop destructor runs on thread exit for threads
    // created via std::thread. Main-thread TLS destructors may NOT run on
    // Windows (no DllMain THREAD_DETACH for the main thread); the writer's
    // periodic drain picks up main-thread events instead.
    static THREAD_RING: RingHandle = {
        let ring = Arc::new(Ring::new());
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::clone(&ring));
        RingHandle(ring)
    };
}

/// Push an event onto the current thread's ring. Lock-free; never allocates
/// after TLS is initialised (first call per thread has one-time init cost).
#[inline]
pub fn push(ev: AllocEvent) -> bool {
    THREAD_RING.with(|h| h.0.push(ev))
}

/// Drain all rings, calling `f` for each event. Removes dead rings that have
/// been fully drained. Called only from the writer thread.
pub fn drain_all(mut f: impl FnMut(AllocEvent)) {
    let mut reg = registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut i = 0;
    while i < reg.len() {
        while let Some(ev) = reg[i].pop() {
            f(ev);
        }
        if !reg[i].producer_alive.load(Ordering::Acquire) {
            reg.swap_remove(i); // dead + empty: reclaim slot
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    // ... (from Step 1)
}
```

- [ ] **Step 4: Run tests — expect all pass**

```
cargo test -p heaplens-alloc ring
```

Expected: 4 tests pass.

- [ ] **Step 5: Commit**

```
git add crates/heaplens-alloc/src/ring.rs
git commit -m "feat(alloc): ring.rs — SPSC ring (System-backed) + thread-local registry"
```

---

## Task 4: `capture.rs` — timestamp + stack capture

**Files:**
- Modify: `crates/heaplens-alloc/src/capture.rs`

**Interfaces:**
- Consumes: `backtrace::trace_unsynchronized`
- Produces:
  - `pub fn timestamp_nanos() -> u64`
  - `pub fn capture_stack() -> ([u64; 8], u8)`

No standalone tests needed — capture is exercised by the integration tests and alloc_smoke example. The key invariant (no allocation) is guaranteed by only using stack-local data, `OnceLock<Instant>`, and `trace_unsynchronized`.

- [ ] **Step 1: Implement `capture.rs`**

Replace `crates/heaplens-alloc/src/capture.rs` with:

```rust
use std::sync::OnceLock;
use std::time::Instant;

/// Process-start instant, initialised once on first call to `timestamp_nanos`.
/// `OnceLock::get_or_init` does not allocate.
static START: OnceLock<Instant> = OnceLock::new();

/// Monotonic timestamp in nanoseconds since process start.
/// Uses `Instant` (QPC on Windows) — no allocation, no unsafe.
#[inline]
pub fn timestamp_nanos() -> u64 {
    let start = START.get_or_init(Instant::now);
    Instant::now().duration_since(*start).as_nanos() as u64
}

/// Capture up to 8 raw instruction pointers from the current call stack.
/// Does not resolve symbols (symbol resolution is off-path, in writer.rs).
///
/// # Safety precondition
/// Must be called with the per-thread recursion guard set. Any allocation
/// triggered by backtrace internals (first-use frame table init) will be
/// suppressed by the guard and will not recurse into `record`.
///
/// `trace_unsynchronized` is unsafe because it must not be called
/// concurrently from multiple threads without external synchronisation.
/// Here it is safe: called per-thread, one call at a time, under the
/// per-thread guard.
#[inline]
pub fn capture_stack() -> ([u64; 8], u8) {
    let mut stack = [0u64; 8];
    let mut count = 0usize;
    // SAFETY: called single-threaded per-thread, guard is held.
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if count < 8 {
                stack[count] = frame.ip() as u64;
                count += 1;
                true  // continue walking
            } else {
                false // stop
            }
        });
    }
    (stack, count as u8)
}
```

- [ ] **Step 2: Verify compilation**

```
cargo build -p heaplens-alloc
```

Expected: no errors.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-alloc/src/capture.rs
git commit -m "feat(alloc): capture.rs — monotonic timestamp and raw stack capture"
```

---

## Task 5: `lib.rs` — `HeapLensAlloc` + `record` fn

**Files:**
- Modify: `crates/heaplens-alloc/src/lib.rs`

**Interfaces:**
- Consumes: `guard::{is_set, ScopedGuard}`, `ring::push`, `capture::{timestamp_nanos, capture_stack}`, `writer::run`
- Produces: `pub struct HeapLensAlloc` with `pub const fn new() -> Self` and `unsafe impl GlobalAlloc`

- [ ] **Step 1: Implement `lib.rs`**

Replace `crates/heaplens-alloc/src/lib.rs` with the full implementation:

```rust
mod guard;
mod ring;
mod capture;
mod writer;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Once;
use heaplens_protocol::EventKind;
use heaplens_protocol::AllocEvent;

/// A `#[global_allocator]` that intercepts every (de/re)allocation and ships
/// raw `AllocEvent` records off-process via a named pipe, without blocking
/// or allocating on the hot path.
///
/// # Usage
/// ```no_run
/// use heaplens_alloc::HeapLensAlloc;
/// #[global_allocator]
/// static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();
/// ```
///
/// Activate before the program's first allocation. The background writer thread
/// is spawned lazily on first record; it permanently holds the recursion guard
/// so none of its own allocations are ever recorded.
pub struct HeapLensAlloc;

impl HeapLensAlloc {
    pub const fn new() -> Self { HeapLensAlloc }
}

static WRITER_ONCE: Once = Once::new();

/// Spawn the writer thread exactly once. Safe to call from the hot path:
/// after the first successful call_once, subsequent calls are a single
/// atomic load (no allocation, no blocking).
///
/// The spawn itself allocates ("heaplens-writer" thread name string), but it
/// runs under the recursion guard, so those allocations are suppressed.
#[inline]
fn ensure_writer() {
    WRITER_ONCE.call_once(|| {
        // Errors here are unrecoverable but must not panic the host.
        // If spawn fails (e.g., out of threads), events silently pile up
        // in the ring until it fills, after which they are dropped.
        let _ = std::thread::Builder::new()
            .name("heaplens-writer".to_owned())
            .spawn(writer::run);
    });
}

/// Record one allocation event. Hot path.
///
/// Invariants enforced here:
/// 1. Guard checked first — re-entrant calls return immediately.
/// 2. Guard set via RAII ScopedGuard — cleared even on panic.
/// 3. No allocation, no locking.
/// 4. ring::push failure (full ring) is a silent drop.
#[inline]
fn record(kind: EventKind, ptr: u64, old_ptr: u64, size: u64, align: u32) {
    // 1. Re-entrancy check — must be the very first thing.
    if guard::is_set() { return; }

    // 2. Acquire guard (panic-safe RAII).
    let _g = guard::ScopedGuard::enter();

    // 3. Ensure writer thread is running. Safe under guard: any allocations
    //    inside call_once are suppressed by the guard.
    ensure_writer();

    // 4. Timestamp (no alloc).
    let ts = capture::timestamp_nanos();

    // 5. Raw stack capture (no alloc; unsafe justified in capture.rs).
    let (stack, stack_len) = capture::capture_stack();

    // 6. Construct event (no alloc; enforces _pad = [0,0] via AllocEvent::new).
    let ev = AllocEvent::new(kind, ptr, old_ptr, size, align, ts, stack, stack_len);

    // 7. Push to per-thread ring (lock-free, no alloc after TLS init).
    //    Returns false if full — event is silently dropped.
    ring::push(ev);

    // _g drops here, clearing the guard even if earlier steps panicked.
}

unsafe impl GlobalAlloc for HeapLensAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        record(EventKind::Alloc, ptr as u64, 0, layout.size() as u64, layout.align() as u32);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        record(EventKind::Dealloc, ptr as u64, 0, layout.size() as u64, layout.align() as u32);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = System.realloc(ptr, layout, new_size);
        record(
            EventKind::Realloc,
            new_ptr as u64,
            ptr as u64,
            new_size as u64,
            layout.align() as u32,
        );
        new_ptr
    }
}
```

- [ ] **Step 2: Build and verify no errors**

```
cargo build -p heaplens-alloc
```

Expected: `Finished`.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-alloc/src/lib.rs
git commit -m "feat(alloc): lib.rs — HeapLensAlloc GlobalAlloc + panic-safe record fn"
```

---

## Task 6: `writer.rs` — background writer thread

**Files:**
- Modify: `crates/heaplens-alloc/src/writer.rs`

**Interfaces:**
- Consumes: `guard::force_enter_permanent`, `ring::drain_all`, `heaplens_protocol::{encode_handshake, encode_events, encode_symbols}`
- Produces: `pub fn run()` — the writer thread entry point

The writer holds the recursion guard permanently, so all allocations it does (HashMap, Vec, String for symbol names) are never recorded. It connects to `\\.\pipe\heaplens` via `std::fs::OpenOptions`, retrying every 100 ms until the daemon is up. On pipe error, it discards the connection and re-enters the retry loop. It never panics.

- [ ] **Step 1: Implement `writer.rs`**

Replace `crates/heaplens-alloc/src/writer.rs` with:

```rust
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::thread;
use std::time::{Duration, Instant};

use heaplens_protocol::{AllocEvent, encode_events, encode_handshake, encode_symbols};

const PIPE_PATH: &str = r"\\.\pipe\heaplens";
const BATCH_CAP: usize = 64;
const FLUSH_INTERVAL: Duration = Duration::from_millis(1);
const RETRY_SLEEP: Duration = Duration::from_millis(100);
const POLL_SLEEP: Duration = Duration::from_micros(100);

/// The writer thread entry point. Spawned once via `WRITER_ONCE` in lib.rs.
///
/// The guard is permanently held from the top of this function so that none
/// of the writer's own allocations (HashMap, Vec, String) are recorded.
pub fn run() {
    // Invariant §12.4: writer thread permanently holds the recursion guard.
    crate::guard::force_enter_permanent();

    let mut symbol_cache: HashMap<u64, String> = HashMap::new();

    loop {
        // ── Connect ─────────────────────────────────────────────────────────
        let mut pipe = loop {
            match OpenOptions::new().read(true).write(true).open(PIPE_PATH) {
                Ok(f) => break BufWriter::new(f),
                Err(_) => thread::sleep(RETRY_SLEEP),
            }
        };

        // ── Handshake ────────────────────────────────────────────────────────
        let pid = std::process::id() as u64;
        let name = process_name();
        let handshake = encode_handshake(pid, &name);
        if pipe.write_all(&handshake).is_err() || pipe.flush().is_err() {
            continue; // reconnect
        }

        // ── Event loop ───────────────────────────────────────────────────────
        let mut batch: Vec<AllocEvent> = Vec::with_capacity(BATCH_CAP);
        let mut new_syms: Vec<(u64, String)> = Vec::new();
        let mut last_flush = Instant::now();

        'send: loop {
            // Drain all rings into batch.
            crate::ring::drain_all(|ev| batch.push(ev));

            let should_flush =
                batch.len() >= BATCH_CAP || last_flush.elapsed() >= FLUSH_INTERVAL;

            if should_flush && !batch.is_empty() {
                // Resolve new instruction pointers (off critical path).
                for ev in &batch {
                    for i in 0..ev.stack_len as usize {
                        let addr = ev.stack[i];
                        if addr == 0 || symbol_cache.contains_key(&addr) {
                            continue;
                        }
                        let mut name = format!("0x{addr:x}");
                        backtrace::resolve(addr as *mut _, |sym| {
                            if let Some(n) = sym.name() {
                                name = n.to_string();
                            }
                        });
                        symbol_cache.insert(addr, name.clone());
                        new_syms.push((addr, name));
                    }
                }

                // Emit SYMBOLS frame for newly-seen addresses.
                if !new_syms.is_empty() {
                    let refs: Vec<(u64, &str)> = new_syms
                        .iter()
                        .map(|(a, n)| (*a, n.as_str()))
                        .collect();
                    if pipe.write_all(&encode_symbols(&refs)).is_err() {
                        new_syms.clear();
                        batch.clear();
                        break 'send; // reconnect
                    }
                    new_syms.clear();
                }

                // Emit EVENTS frame.
                if pipe.write_all(&encode_events(&batch)).is_err()
                    || pipe.flush().is_err()
                {
                    batch.clear();
                    break 'send; // reconnect
                }

                batch.clear();
                last_flush = Instant::now();
            } else if batch.is_empty() {
                thread::sleep(POLL_SLEEP);
            }
        }
        // Fell through 'send → reconnect. symbol_cache is preserved across
        // reconnects so we don't re-emit already-sent symbol definitions.
    }
}

fn process_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| {
            p.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}
```

- [ ] **Step 2: Build — expect no errors**

```
cargo build -p heaplens-alloc
```

Expected: `Finished`.

- [ ] **Step 3: Commit**

```
git add crates/heaplens-alloc/src/writer.rs
git commit -m "feat(alloc): writer.rs — drain→resolve→flush loop with reconnect"
```

---

## Task 7: Integration tests + `alloc_smoke` example

**Files:**
- Create: `crates/heaplens-alloc/tests/guard_tests.rs`
- Create: `crates/heaplens-alloc/tests/ring_tests.rs`
- Create: `crates/heaplens-alloc/tests/multithread_stress.rs`
- Create: `crates/heaplens-alloc/examples/alloc_smoke.rs`

**Note on test structure:** Integration tests in `tests/` compile as separate binaries and do NOT set `#[global_allocator]` unless explicitly declared in the test file itself. Tests that set `HeapLensAlloc` as global allocator are isolated to their own file to avoid interference.

- [ ] **Step 1: Create `tests/guard_tests.rs`**

These mirror the `#[cfg(test)]` tests in `guard.rs` but run as an integration test binary to guarantee isolation from unit test TLS state.

```rust
// tests/guard_tests.rs
use heaplens_alloc::guard::{force_enter_permanent, is_set, ScopedGuard};

#[test]
fn guard_basics_integration() {
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
fn guard_panic_safe_integration() {
    std::thread::spawn(|| {
        let _ = std::panic::catch_unwind(|| {
            let _g = ScopedGuard::enter();
            panic!("intentional");
        });
        assert!(!is_set());
    })
    .join()
    .unwrap();
}
```

But wait: `guard`, `ring`, `capture` are private modules in `lib.rs`. Integration tests cannot access them directly unless they are re-exported. Add re-exports to `lib.rs`:

```rust
// In lib.rs, add after the `mod` declarations:
#[doc(hidden)]
pub mod guard;
#[doc(hidden)]
pub mod ring;
```

(Alternatively, add `pub(crate)` visibility and use `#[cfg(test)]` integration. But integration tests need `pub`. Add `pub` with `#[doc(hidden)]` to keep them out of the public API surface.)

- [ ] **Step 2: Re-export internal modules for tests**

Edit `crates/heaplens-alloc/src/lib.rs` — change `mod guard;` and `mod ring;` to `pub mod guard;` and `pub mod ring;`:

```rust
pub mod guard;
pub mod ring;
mod capture;
mod writer;
```

- [ ] **Step 3: Create `tests/ring_integration.rs`**

```rust
// tests/ring_integration.rs
use heaplens_alloc::ring;
use heaplens_protocol::{AllocEvent, EventKind};

fn ev(n: u64) -> AllocEvent {
    AllocEvent::new(EventKind::Alloc, n, 0, 64, 8, n, [0u64; 8], 0)
}

#[test]
fn thread_ring_drainable_after_exit() {
    let handle = std::thread::spawn(|| {
        ring::push(ev(0xBEEF_CAFE));
    });
    handle.join().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(20));

    let mut found = false;
    ring::drain_all(|e| {
        if e.ptr == 0xBEEF_CAFE {
            found = true;
        }
    });
    assert!(found, "event from exited thread must remain drainable");
}
```

- [ ] **Step 4: Create `tests/multithread_stress.rs`**

```rust
// tests/multithread_stress.rs
//
// Many threads allocating concurrently. Uses HeapLensAlloc as global allocator.
// Asserts: no deadlock (test completes), bounded memory (dropped counter works),
// and no host process crash.

use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

#[test]
fn concurrent_allocs_no_deadlock() {
    const THREADS: usize = 8;
    const ITERS: usize = 10_000;

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                for i in 0..ITERS {
                    let v: Vec<u8> = vec![i as u8; 64];
                    std::hint::black_box(v);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread must not panic");
    }
    // If we reach here without deadlock or abort, the test passes.
}
```

- [ ] **Step 5: Create `examples/alloc_smoke.rs`**

```rust
// crates/heaplens-alloc/examples/alloc_smoke.rs
//
// Smoke test: run with no daemon. The writer retries the pipe connection in
// the background. The host process must not crash, hang, or abort.
// Events are produced into the ring and dropped when it fills.

use heaplens_alloc::HeapLensAlloc;

#[global_allocator]
static GLOBAL: HeapLensAlloc = HeapLensAlloc::new();

fn main() {
    let rounds = 1_000;
    for i in 0..rounds {
        // Allocate and immediately drop a variety of sizes.
        let _s: String = format!("hello-{i}");
        let _v: Vec<u64> = (0..16).collect();
        let _b: Box<[u8; 128]> = Box::new([i as u8; 128]);
    }
    println!("alloc_smoke: {rounds} rounds completed — no crash, no hang.");
}
```

- [ ] **Step 6: Run all tests and the smoke example**

```
cargo test -p heaplens-alloc
cargo run --example alloc_smoke -p heaplens-alloc
```

Expected:
- Tests: all pass (the multithread stress test may take a few seconds).
- Smoke example: prints the completion message and exits cleanly within a few seconds. The writer thread keeps retrying the pipe in the background (no daemon), but this does not block the main thread.

- [ ] **Step 7: Commit**

```
git add crates/heaplens-alloc/tests/ crates/heaplens-alloc/examples/ crates/heaplens-alloc/src/lib.rs
git commit -m "test(alloc): integration tests, multithread stress, alloc_smoke example"
```

---

## Task 8: Full suite + clippy clean

**Files:**
- Modify: any source files that clippy flags

- [ ] **Step 1: Run the full test suite**

```
cargo test -p heaplens-alloc
```

Expected: all tests pass.

- [ ] **Step 2: Run clippy**

```
cargo clippy -p heaplens-alloc -- -D warnings
```

Common issues to fix if they appear:
- `clippy::new_without_default` on `Ring`: add `impl Default for Ring { fn default() -> Self { Ring::new() } }`
- `clippy::missing_safety_doc` on `unsafe impl GlobalAlloc`: add `# Safety` doc section to `HeapLensAlloc`
- `clippy::cast_possible_truncation`: `count as u8` in capture.rs — use `.min(u8::MAX as usize) as u8` or add explicit bound (`count < 8` guarantees fit)
- `clippy::module_name_repetitions`: rename if clippy flags e.g. `ring::Ring` — suppress with `#[allow(clippy::module_name_repetitions)]`

- [ ] **Step 3: Fix any clippy issues, then re-run both**

```
cargo test -p heaplens-alloc && cargo clippy -p heaplens-alloc -- -D warnings
```

Expected: clean.

- [ ] **Step 4: Commit if fixes were needed**

```
git add -p
git commit -m "fix(alloc): clippy clean"
```

---

## Acceptance checklist

- [ ] `cargo build -p heaplens-alloc` — zero errors
- [ ] `cargo test -p heaplens-alloc` — all tests pass
- [ ] `cargo clippy -p heaplens-alloc -- -D warnings` — zero warnings
- [ ] `cargo run --example alloc_smoke -p heaplens-alloc` — prints completion, exits cleanly, no crash
- [ ] `record` and every function it calls transitively: no `Vec`, `String`, `Box`, `format!`
- [ ] Ring backing store: allocated via `System.alloc_zeroed`, not via `HeapLensAlloc`
- [ ] Writer thread: first line is `guard::force_enter_permanent()`
- [ ] No Stage 3+ logic (no graph, no anomaly, no WebSocket, no SQLite)
