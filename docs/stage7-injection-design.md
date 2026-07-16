# Stage 7 — Process Attachment via Injection (Design)

Status: **DESIGN ONLY — not implemented.** This document specifies the
architecture; no code has been written against it. Benchmarks remain frozen
and unaffected by this stage. No branch has been created for this work.

Goal: let a user pick a running Windows process and observe its heap in
HeapLens without recompiling or relinking the target.

Scope for this stage, confirmed: **single target process at a time**,
matching the daemon's current single-connection pipe loop. Multi-target
concurrent monitoring is a real capability a future stage could add, but is
explicitly out of scope here — see §3.4.

---

## 1. Capture front-end

### 1.1 Mechanism: inline hooking via `minhook`, on the `ntdll!Rtl*Heap` layer

**Chosen:** a hook DLL installs inline (trampoline) hooks on
`ntdll!RtlAllocateHeap` / `RtlReAllocateHeap` / `RtlFreeHeap`, using the
`minhook` crate (Rust bindings to the MinHook library).

**Revised from the initial draft, which specified `kernelbase!HeapAlloc` —
confirmed during Step 1 implementation to be the wrong layer on this
system, not a style choice.** Hooking `kernelbase!HeapAlloc`/`HeapReAlloc`/
`HeapFree` directly produced a reproducible `STATUS_ACCESS_VIOLATION` on
the very first call after `MinHook::enable_all_hooks()` succeeded — MinHook
reports success at every step (hook creation, enable), but the generated
trampoline is broken the instant it's invoked. Isolated via a control
probe: hooking a simple, non-allocating export (`GetTickCount`) with the
identical create/enable/detour pattern worked correctly (correct trampoline
call, correct return value, no crash), which rules out a usage mistake and
narrows the failure to something specific about `HeapAlloc`'s prologue
shape on this Windows build. Hooking one layer lower, at
`ntdll!RtlAllocateHeap` (which `kernelbase!HeapAlloc` is a thin wrapper
around), is stable — confirmed via the same control-probe method, then via
the full Step 1 acceptance gate.

**Consequence, not a downside to hide:** `Rtl*Heap` is lower-level and
process-wide — it intercepts *all* heap traffic through that path,
including allocations `HeapAlloc`-layer hooking wouldn't have surfaced
identically, and including the capture pipeline's own ambient heap traffic
(the writer thread's pipe I/O triggers some Windows-internal heap calls as
a side effect — confirmed empirically, see §8 Step 1's acceptance gate,
which matches captured events to the workload by pointer identity rather
than a raw count specifically because of this). This is a more honest
capture surface than the `HeapAlloc` layer would have been, not a worse
one — the same "silent undercount is worse than a visible one" reasoning
below for IAT-vs-inline hooking applies here too. Any real deployment
built on this design must filter/attribute by pointer identity or
equivalent, not assume the raw event stream is 1:1 with "the target's own
calls," because it structurally is not.

**Why not IAT hooking:** IAT hooking only intercepts calls that go through
the target's import table. It misses statically-linked CRTs (every MSVC
`/MT` binary), calls made through function pointers, and delay-loaded
imports. For the actual goal — observing real, uncooperative third-party
binaries — those blind spots aren't a degraded signal, they're a *silent
undercount*: the tool would report a heap graph that's missing an unknown
fraction of allocations, with no way to tell how much. That's worse than
φ's symbol-degradation problem (§2), because the user can't see it happening.
Inline hooking intercepts the function at its entry point regardless of how
control reached it, which is the only mechanism that gives an honest capture
surface on binaries we don't control.

**Why the `Rtl*Heap` layer, not CRT `malloc`:** CRT `malloc` calls into this
same layer internally on Windows. Hooking both would double-count every CRT
allocation. `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` is the
common sink — it catches CRT-backed allocations *and* direct-Win32
allocators that bypass the CRT entirely, and avoids per-CRT-version
`malloc` symbol variance (static vs dynamic CRT, debug vs release CRT
naming). The tradeoff: this observes heap *operations*, not CRT-level
semantics (e.g. `calloc`'s zeroing is invisible as a distinct operation
from a plain alloc). That's acceptable — HeapLens is a memory-topology
tool, not a CRT-semantics tool.

### 1.2 Wire format and transport: unchanged, reused as-is

The existing framed wire protocol (`crates/heaplens-protocol`) already
carries everything needed:

- `Frame::Handshake { pid, name }` (frame type `0x00`) already exists on the
  wire. Today the daemon **decodes and discards** it
  (`ingest.rs`: "pid/process name not currently used"). This stage
  **activates that field** — the daemon starts using the PID it already
  receives, for display ("monitoring: `<name>` [`<pid>`]"), for
  detecting target-exit, and for scoping the session. **No wire schema
  change.**
- `AllocEvent` (frame type `0x01`) and `Symbols` (frame type `0x02`) are
  used by the hook DLL exactly as they are used by every cooperative
  producer today. No new fields, no format change.
- Transport is the same named pipe (`\\.\pipe\heaplens`), and the daemon's
  existing `ingest.rs` accept loop needs **no changes** to accept a
  connection from the injected hook DLL — from the daemon's point of view,
  an injected target is indistinguishable from a cooperative producer. This
  is the payoff of keeping the wire format identical: the daemon, graph,
  φ, and Flutter app are **entirely unchanged** by this stage. All new code
  is upstream of the pipe.

**Confirmed: protocol, daemon (ingest/graph/φ), and Flutter are unchanged.**
The only new components are two new crates (§1.3) that produce the same
bytes a cooperative producer already produces.

### 1.3 New components

- **`heaplens-hook`** (new crate, `cdylib`) — the DLL that gets injected.
  Statically links `minhook` and reuses the existing `heaplens-alloc`
  capture pipeline as a library: the lock-free per-thread ring buffer
  (`ring.rs`), the record/capture/writer functions, symbol resolution, and
  pipe framing are the same code, now exposed as `pub` for this reuse.
  Where `heaplens-alloc` hangs off `#[global_allocator]` (a compile-time
  hook available only to a statically-linked producer), `heaplens-hook`
  hangs the same `record()` call off MinHook-installed trampolines on
  `RtlAllocateHeap`/`RtlReAllocateHeap`/`RtlFreeHeap` (§1.1). The capture
  logic itself — ring push, event construction, stack capture, wire framing
  — is unmodified.

  **Revised from the initial draft's "reused unmodified in spirit" —
  confirmed during Step 1 that the *lifecycle*, not just the entry point,
  differs, and the difference is load-bearing, not stylistic.**
  `heaplens-alloc`'s cooperative model spins up the writer thread and
  primes symbol resolution *lazily*, triggered by the first captured
  event — safe there because record() runs on an ordinary thread making an
  ordinary allocation. Inside an injected hook, that same lazy trigger runs
  *from inside the hook callback itself*, and two distinct, empirically
  confirmed hazards follow from that: spawning a thread from inside the
  hook reenters the not-yet-stable hooked path during the new thread's
  `DLL_THREAD_ATTACH` bootstrap (crash), and the writer thread's first
  `backtrace::resolve` call — which loads and initializes `dbghelp.dll` —
  hits the same class of hazard one layer later (also a crash, isolated by
  disabling one thing at a time until each specific cause was found). Both
  are fixed the same way: force them to completion *eagerly*, before any
  hook is installed. `attach_impl` (the shared core both exported attach
  entry points drive — see §4.1) therefore performs, in this exact order:
  1. Create the private heap (§4.2).
  2. Start the writer thread (`heaplens_alloc::ensure_writer_started`).
  3. Warm `backtrace::resolve`'s one-time init
     (`heaplens_alloc::warm_up_symbol_resolution`).
  4. **Only then** install and enable the MinHook hooks.

  This ordering is a hard requirement, not an optimization — reverting to
  lazy initialization "to simplify" would reintroduce a target-crashing bug
  invisible to any test run in a warm process (a test harness that already
  triggered dbghelp/thread-pool warmup elsewhere wouldn't reproduce it). A
  fourth hazard, symmetric to the first, governs detach and is covered in
  §4.1.

  **The precise, now-corrected boundary of what's reusable:** the capture
  *logic* (ring, record, writer body, wire framing) is shared code,
  unmodified. The *lifecycle* — when each piece is allowed to initialize
  relative to when hooks are live — is new and specific to injection, not
  a stylistic difference from the cooperative allocator's lifecycle.

- **`heaplens-injector`** (new crate, small synchronous binary) — does the
  actual `OpenProcess` / `VirtualAllocEx` / `WriteProcessMemory` /
  `CreateRemoteThread` sequence to load `heaplens-hook.dll` into the target
  and to call its exported attach/detach functions (§4.1). Runs as a
  short-lived child process spawned by the daemon (§3.2), one invocation per
  attach and one per detach. Kept separate from the daemon binary
  deliberately: injection is synchronous, Win32-heavy, and process-scoped
  code that has no reason to live inside the daemon's async tokio runtime,
  and keeping it a standalone tool makes it independently testable (it can
  be pointed at any test target PID from a terminal) exactly like the
  existing producer examples.

### 1.4 Two capture semantics adaptations (both are honest limitations, not bugs)

- **Timestamps.** `AllocEvent.ts_nanos` is documented as monotonic since
  *process start*. An injected hook doesn't observe process start — it
  attaches at some arbitrary point in the target's lifetime. `heaplens-hook`
  rebases its clock to *injection time*, not process start. This must be
  stated in the daemon/Flutter-facing documentation: for injected sessions,
  "time since attach," not "time since process start." No wire or daemon
  change is needed — the daemon already treats `ts_nanos` as an opaque
  relative counter, not a wall-clock value.
- **Pre-attach allocations are invisible.** Injection happens into a
  process that has already been running and allocating. Those earlier
  allocations produced no `AllocEvent`, so when one of them is later freed,
  the hook observes a `HeapFree` for a pointer it never saw allocated.
  **Design decision: an unknown-pointer free is silently dropped — not
  forwarded as a `Dealloc` event, not surfaced as an error.** Forwarding it
  would create a dealloc-for-nonexistent-node in the daemon's graph, which
  has no defined meaning. The injected view is therefore "allocations from
  attach-time forward," not a full heap snapshot — document this plainly as
  a known characteristic of the capture mode, both in this doc and in the
  eventual Flutter UI (a one-line note when an injected session starts).
- **Realloc of a pre-attach pointer is the asymmetric case, and it's already
  handled — confirmed, not assumed.** A pointer allocated before attach can
  later be *reallocated* (not just freed) after attach. `HeapReAlloc` on
  such a pointer produces a `Realloc` event whose `old_ptr` the daemon never
  tracked. Verified directly against the current code
  (`heaplens-daemon/src/graph.rs:130-154`, `Graph::on_realloc`): when
  `old_ptr` is not found in `by_ptr`, the function does **not** drop the
  event — it falls through to `self.on_alloc(...)` and registers `new_ptr`
  as a brand-new node, exactly as if it had been a fresh allocation. This is
  already the correct behavior for the injection case and needs no daemon
  change. It is deliberately asymmetric with `on_dealloc`
  (`graph.rs:92-96`, `Graph::on_dealloc`), which `return`s immediately on an
  unknown `ptr` and drops the event: a realloc of an unknown pointer still
  carries a real, current size and address worth tracking going forward
  (there's a live allocation *right now*, we just missed its origin), while
  a free of an unknown pointer has nothing left to track — the thing is
  gone. Record this as an intentional, already-correct asymmetry, not an
  oversight to fix.

---

## 2. The stack-capture and φ problem, stated honestly

This is the same debug-info envelope finding from the release-packaging
work this session, now **inherent to the injection use case rather than an
avoidable build configuration**: we don't control how the target was built,
and most real-world binaries a user would want to inject into are
release-optimized and shipped without debug info (no PDB, or a PDB the user
doesn't have).

Confirmed via this session's codebase exploration: `infer_ownership`
(`graph.rs`) depends on `effective_site_name`, which depends on
`resolver.name_for(addr)` — a string produced by `backtrace::resolve` and
shipped over the `Symbols` frame. φ's granularity is deliberately the
*function name*, not the instruction address (exact-IP matching was tried
earlier in the project and rejected — see `HeapLens_Build_Spec.md` — because
different allocation statements in the same function resolve to different
IPs). When `backtrace::resolve` cannot produce a real function name — no
debug info available for that module — `effective_site_name` falls back to
a raw `0x{addr:x}` string. Two allocations from two different call sites
never coincidentally collide on a raw hex address, so **φ silently produces
zero ownership edges** against such a target. This is not a crash and not a
detectable "error" state from φ's point of view; it degrades to reporting
every node as its own root.

**What still works against a symbol-stripped injected target:**
- Allocation capture itself (size, pointer, timestamp, raw stack depth) —
  unaffected; this doesn't require symbol names.
- The memory-map / node-count / size-over-time view — unaffected, since it
  doesn't depend on ownership edges.
- Leak-by-growth detection (a node's size growing without a matching free,
  or `hot_cluster_threshold`-based structural flags) — unaffected; both are
  purely count/size-based, not name-based.
- Orphan detection (`tau_ms`) — unaffected; purely time-based.

**What degrades:**
- The ownership force-graph and its edges — this is φ's entire output, and
  it degrades toward a graph of disconnected roots as symbol availability
  drops. Partial symbol availability (some modules ship debug info, some
  don't — common when injecting into a target that statically links a
  symbol-stripped third-party library) produces a **partially-connected**
  graph: real edges where both the allocating and owning frames resolve,
  missing edges where either doesn't. This is honest and worth stating
  plainly rather than papering over: φ's edges are only as trustworthy as
  the weakest symbol table in the call chain.

**For the mémoire:** this is worth a short, direct paragraph in whichever
chapter discusses φ's limitations — the technique's precondition
(symbolizable frames) that was established for the *shipped* build in this
session's earlier work turns out to be not just a packaging concern but a
structural boundary of what injection-based profiling can promise. A
profiler that requires debug info to show ownership, applied to a target
that has none, is expected to show none — and that is a correct, legible
degradation, not a bug to chase.

---

## 3. Process picker UX and orchestration

### 3.1 Where the picker lives: Flutter, not the launcher

The launcher (`heaplens-launcher`) runs with `windows_subsystem = "windows"`
(no console) and today does exactly one thing: start the daemon, wait for
its port, start the Flutter app, tear both down together. It has no
process-enumeration facility and no UI. Building a process picker there
would mean building a second, separate UI toolkit inside a component
designed to be invisible. The Flutter app already has UI, already holds the
WebSocket connection to the daemon, and already renders daemon state — it's
the natural home for "here is a list of running processes, pick one."

**New Flutter screen** (name: process picker / attach dialog) requests a
process list from the daemon and, on selection, sends an attach request.
**New daemon responsibilities**, both over the existing WebSocket control
channel (the one that already carries `NodeDto`/`GraphMessage` JSON — a
small addition to that message enum, not a new channel):
- `ListProcesses` request → daemon enumerates running processes
  (`CreateToolhelp32Snapshot` + `Process32First/Next`) and returns
  `{ pid, name, arch }` tuples. Daemon does the enumeration (not Flutter)
  because it's already the process with Win32 access; Flutter stays a pure
  UI/rendering layer, consistent with its role everywhere else in the
  system.
- `AttachTarget { pid }` request → daemon validates (§3.3), clears the
  current graph (§3.4), spawns `heaplens-injector.exe <pid> --attach`, and
  reports success/failure back to Flutter.
- `DetachTarget` request (or automatic on target exit) → daemon spawns
  `heaplens-injector.exe <pid> --detach` (§4.1), closes out the session.

This is a small, additive change to the existing WS message enum — not a
new transport, not a new protocol.

### 3.2 Orchestration: the daemon spawns the injector

The daemon owns the pipe session lifecycle already (it's the thing deciding
when a producer is connected/disconnected), so it's the natural owner of
"when do we attach/detach a target," even though it doesn't do the Win32
injection syscalls itself — it delegates that to `heaplens-injector` as a
child process, the same pattern the launcher already uses for spawning the
daemon and Flutter app (spawn, wait, report). This keeps injection's Win32
surface (`OpenProcess`, `CreateRemoteThread`, etc.) out of the daemon's
async runtime and in a small, independently-runnable, independently-testable
tool.

### 3.3 Validation before touching the target

`heaplens-injector` performs these checks, in order, before any
`WriteProcessMemory`/`CreateRemoteThread` call, and reports a specific
failure reason for each:

- **Architecture match.** An x64 injector cannot inject into an x86 target
  (different address space layout, calling convention, and the hook DLL
  itself is architecture-specific). Checked via `IsWow64Process2` against
  the target. Reject early with "target is a 32-bit process; this build of
  HeapLens is 64-bit and cannot attach" — surfaced verbatim to the Flutter
  UI as the attach-failure message.
- **Access.** `OpenProcess` requested with the minimal rights actually
  needed (`PROCESS_CREATE_THREAD | PROCESS_VM_OPERATION | PROCESS_VM_WRITE |
  PROCESS_VM_READ | PROCESS_QUERY_INFORMATION`), not
  `PROCESS_ALL_ACCESS`. Failure (protected process, elevated process while
  HeapLens itself isn't, antimalware-protected process) is reported as
  "access denied — try running HeapLens as Administrator." **No automatic
  elevation, no token manipulation, no privilege escalation attempt** —
  this is a deliberate security boundary: if the user needs elevation, they
  re-launch HeapLens elevated themselves.
- **Target still running.** Re-checked immediately before injection (PID
  reuse race is small but real on Windows) via a live handle held from the
  successful `OpenProcess` call through to the `CreateRemoteThread` call —
  if the process exited in between, the handle operations themselves fail
  and are reported as "target exited before attach completed," not treated
  as an internal error.

### 3.4 Clean single-target transition (the scope decision's actual constraint)

Per the confirmed single-target scope: selecting a new target while one is
already attached must be an explicit, ordered transition, not a race:

1. Daemon spawns `heaplens-injector.exe <old_pid> --detach` (§4.1) and waits
   for it to complete.
2. Daemon waits for the existing pipe connection to close (the hook DLL's
   writer thread closes it as part of clean detach).
3. Daemon **clears the graph** — all nodes, all edges. A new target process
   is a new, unrelated address space; merging its nodes into the previous
   target's topology would produce a graph that mixes two programs'
   allocations into one nonsensical structure. This is the correctness-
   critical step of the transition, not a cosmetic reset.
4. Daemon spawns `heaplens-injector.exe <new_pid> --attach`.

This sequencing lives entirely in the daemon (it already serializes pipe
connections one at a time, so this is a natural extension of logic it
already has), not in `heaplens-injector`, which stays a stateless one-shot
tool per invocation.

---

## 4. Safety and teardown

### 4.1 Attach/detach as explicit exported entry points, not `DllMain`

`heaplens-hook.dll` exports `HeapLensHookAttach`/`HeapLensHookAttachRemote`
(attach) and `HeapLensHookDetach`/`HeapLensHookDetachApc` (detach). None run
from `DllMain`. Running MinHook installation or spawning threads from
`DllMain` is a well-known deadlock hazard (the loader lock is held during
`DllMain`, and MinHook / thread creation / pipe connection all need to run
outside it). Instead, `heaplens-injector` uses the standard two-step
injection pattern:

1. `CreateRemoteThread` calling `LoadLibraryW` with the DLL path — loads the
   DLL, runs its (minimal, do-nothing) `DllMain`, returns the loaded module
   handle.
2. A second `CreateRemoteThread` calling `GetProcAddress`-resolved
   `HeapLensHookAttachRemote` in the now-loaded module — this is where
   MinHook initializes, hooks are installed, the private heap and writer
   thread are created, and the pipe connection is opened. All real work
   happens here, safely outside the loader lock.

**`attach_impl`'s internal ordering is a hard requirement, confirmed
empirically (§1.3), not an implementation detail:** private heap, then
start the writer thread, then warm `backtrace::resolve`'s one-time init,
then — only then — install and enable the MinHook hooks. Each of the first
three must complete on this normal thread *before* any hook exists for a
callback to reenter.

**Two attach entry points, not one — a fifth hazard, found only under real
cross-process injection (self-load cannot surface it):** a normal `return`
from a `CreateRemoteThread` start routine makes the OS call `ExitThread`,
which synchronously delivers `DLL_THREAD_DETACH` to every loaded DLL on
that thread *before it dies* — unconditionally. With hooks now active
(attach just succeeded), that notification's own heap traffic reenters the
still-active detour on a thread `CreateRemoteThread` created directly
(never touched by the CRT's own thread-init path), corrupting its stack
(`STATUS_STACK_BUFFER_OVERRUN`) — not a generic access violation, a `/GS`
stack-cookie mismatch, consistent with a real overrun rather than merely
"unsafe to walk." Guarding what our own code does downstream (the
recursion guard, §4.2) cannot fix a fault in the OS's own unavoidable
teardown sequence.

The fix: `HeapLensHookAttachRemote` never returns normally. After the
shared `attach_and_wait()` core completes (spawns a normal, properly
CRT-initialized worker thread to do the real attach work — see the
existing rationale below — and blocks for its result), it marks itself
permanently guarded and calls `TerminateThread(GetCurrentThread(), result)`
on itself. `TerminateThread` is documented to skip `DLL_THREAD_DETACH`
notification entirely when the target is the calling thread — the property
needed here. This is safe specifically because the call is **synchronous
self-termination issued only after every prior statement on this thread's
own call stack has already returned** (not an external interrupt arriving
mid-operation): `attach_and_wait()`'s `rx.recv()` has fully returned (and
its `Receiver`'s `Drop` — which runs as part of that function's own,
uninterrupted return — has already completed), and
`force_enter_permanent()`'s thread-local write is a `const`-initialized
fast-TLS store with no allocation. No Windows heap critical section or
Rust lock is entered anywhere on this thread's path between `rx.recv()`
returning and `TerminateThread` firing, so there is nothing left locked.
This was also verified empirically, not just argued: the target's *own*
threads keep allocating successfully — including on the private heap this
same DLL uses internally — for the rest of a real attach/workload/detach
cycle after `TerminateThread` fires (Step 2's acceptance gate captures 50
allocations, 11 reallocations, and 51 frees after this point); a locked
private-heap critical section would have hung every one of those.

A **separate** entry point (`HeapLensHookAttach`, unconditional-`return`,
no `TerminateThread`) exists for in-process callers — Step 1's self-load
harness calls `HeapLensHookAttach` directly on its own long-lived main
thread, where `TerminateThread`-ing the caller would kill that thread (and
its still-pending workload) outright. This was confirmed the hard way: an
earlier version of this fix applied `TerminateThread` inside the single
shared `HeapLensHookAttach`, and it silently broke `hook_self_load_wire`'s
gate (the harness process hung — its main thread was terminated mid-flow
before it could run its own workload). The two entry points share one
internal `attach_and_wait()` core and differ only in how the calling thread
is allowed to return afterward.

**Detach's ordering is the reverse of attach's, and equally load-bearing —
confirmed empirically as a sixth, distinct hazard, symmetric to the fifth
above.** Even with hooks correctly disabled and both attach-time hazards
fixed, a target still crashed if the writer thread was left running through
process/DLL teardown: a thread *exiting* naturally also triggers
`DLL_THREAD_DETACH` notifications and TLS-destructor cleanup on that
thread, which itself performs heap operations — if hooks are still active
at that moment, those exit-time heap calls route through the detour during
the exact window the thread is mid-teardown. `detach_impl` therefore
performs, in this exact order:

1. `MinHook::disable_all_hooks` — un-redirects the three hooked functions
   (but does not yet free MinHook's trampolines, so the private-heap
   allocator, still in use by the writer thread until it actually stops,
   keeps working correctly through this window).
2. Signal the writer thread to stop and wait, bounded by a timeout, for it
   to actually exit (`heaplens_alloc::request_writer_stop_and_wait`). Its
   own exit-time heap traffic now goes through the real, unhooked
   functions, not a detour — this is *why* step 1 must happen first, not
   an independent nicety.
3. Only once the writer is confirmed stopped: `MinHook::uninitialize`
   (frees the trampolines) and destroy the private heap.

If the writer does not stop within the timeout, detach does **not**
proceed to step 3 — freeing MinHook's trampolines or the private heap out
from under a thread that may still be executing code depending on either
is worse than leaving the hook installed.

**Detach never runs via a second `CreateRemoteThread` call — a seventh
hazard, the sharpest one found in this stage.** The intuitive design
(mirror attach: `CreateRemoteThread` a second time, targeting a
`HeapLensHookDetach` export) was tried and crashes reliably, and the reason
is structural, not fixable by changing what that export's body does:
*creating* a new thread at all, while hooks are still globally active,
makes the OS deliver `DLL_THREAD_ATTACH` to every loaded DLL on it
*before any of that thread's own code ever runs* — the same reentrancy
class as the fifth hazard, but at thread *start* instead of thread *end*,
and entirely outside our control since it fires automatically ahead of the
requested start routine. This was proven, not assumed: reducing
`HeapLensHookDetach`'s body to a bare `return 42;` (no spawn, no real
work at all) reproduced the identical crash whenever hooks were active at
the moment the injector's second `CreateRemoteThread` call created the
thread — isolating the fault to the thread's mere creation, not anything
this module's code does once it has control.

The fix avoids creating a second raw thread for detach entirely, reusing
`attach_impl`'s own worker thread — already alive, already a normal,
properly CRT-initialized Rust thread, safe by construction:

1. That worker thread waits alertably (`SleepEx(INFINITE, TRUE)`) instead
   of an inert `park()`, and publishes its own OS thread ID to an exported
   `WORKER_TID: AtomicU32` static as its first action.
2. `heaplens-injector` resolves `WORKER_TID`'s remote address the same way
   it resolves exported functions (local `LoadLibraryW` + `GetProcAddress`,
   offset applied to the target's own base), reads the TID via
   `ReadProcessMemory`, resets an exported `DETACH_RESULT: AtomicI32`
   static to its `-1` "pending" sentinel via `WriteProcessMemory` (guards
   against a stale value from a prior attach/detach cycle in the same
   process), opens the worker thread with `OpenThread(THREAD_SET_CONTEXT,
   ...)`, and calls `QueueUserAPC` with `HeapLensHookDetachApc`'s resolved
   remote address — no new thread is created, so there is no
   `DLL_THREAD_ATTACH` notification to reenter anything.
3. The queued APC runs `detach_impl()` on the worker thread once it next
   enters its alertable wait, then stores the `u32` result (cast to `i32`)
   into `DETACH_RESULT` via `Ordering::Release`. A queued APC has no return
   channel back to the process that queued it, hence the exported static.
4. The injector polls `DETACH_RESULT` via `ReadProcessMemory` (20ms
   interval, 5s bound) until it observes a value other than `-1` — an
   **explicit completion signal**, not a fixed-duration timing guess: the
   loop only stops once it has actually observed the transition. The
   `-1` sentinel never collides with a real `detach_impl` return value (0
   or 6, the only codes it produces).

   The `Ordering::Release` store on the worker thread and the
   `ReadProcessMemory`-based poll on the injector side are not
   synchronized by Rust's abstract memory model (that model only governs
   same-process atomics) — the actual guarantee here is architectural:
   `ReadProcessMemory` is a kernel transition, which cannot return content
   older than the last write actually committed to that physical page, and
   a naturally-aligned 32-bit store/load is atomic on x86/x64 (no torn
   reads). `Ordering::Release` on the store side is still the correct,
   intentional choice — it is what prevents the compiler/CPU from
   reordering the result store to before `detach_impl`'s real teardown
   work in program order on the worker thread, so once *any* observer
   (including a cross-process poll) sees the new value, everything that
   happened-before it on that thread — hooks disabled, writer stopped,
   trampolines freed — is also complete.

`HeapLensHookDetach` (unconditional direct call, no thread spawn) remains
for in-process callers — Step 1's self-load harness calls it directly on
its own thread, which needs neither the APC handshake nor a new thread.
This function used to spawn a worker thread by analogy with attach before
this hazard was understood; that was itself a live bug (spawning *any*
thread while hooks are active hits the same reentrancy class, regardless of
whether it is `CreateRemoteThread`-created or an ordinary in-process
`std::thread::spawn`), and it crashed self-load's own detach path
intermittently until reverted to running `detach_impl` directly on the
caller's thread — no new thread, no notification, no hazard.

`heaplens-injector --detach`, once the APC handshake reports success,
issues one final `CreateRemoteThread` calling `FreeLibrary` to unload the
DLL — safe by this point because hooks are already fully disabled and
uninitialized, matching the same "no thread creation while hooks are live"
condition attach's own raw thread satisfied by construction. After detach
completes, the target process is left exactly as if `heaplens-hook.dll` had
never been loaded — no dangling hooks, no leftover threads holding any
lock this module ever used.

### 4.2 Reentrancy inside the hook

The hook functions run **inside the target's own threads**, at the moment
they call `RtlAllocateHeap`/`RtlFreeHeap`/`RtlReAllocateHeap` (§1.1). If
capturing an event itself allocates — building the `AllocEvent`, growing a
buffer, opening a pipe — that allocation re-enters the hooked function,
which re-enters the capture code: unbounded recursion, crashing the
target. This is the same hazard Stage 2's cooperative allocator solved
with a thread-local reentrancy guard (`heaplens-alloc/src/guard.rs`) —
`heaplens-hook` uses the identical pattern, now load-bearing in a context
where a crash means crashing *someone else's process*, not our own test
binary:

- A thread-local guard flag: if already inside the hook on this thread,
  call straight through to the real function (via the MinHook trampoline)
  and do not attempt to capture.
- All of the hook's own memory needs (event staging buffer, ring buffer
  storage) come from a **private heap** created once at attach time via
  `HeapCreate` — a heap MinHook never touches — never from the process's
  default heap that the hook is watching. This is the mechanism, not just
  the guard flag, that makes the guard's fast-path safe: even if the guard
  were somehow bypassed, the hook's internal allocations physically cannot
  reach the hooked functions.

### 4.3 Daemon dies while a target is injected

The hook DLL's writer thread must never block the target's allocation path
waiting on the pipe. This reuses the existing property of
`heaplens-alloc`'s lock-free ring buffer: when the ring is full or the pipe
write fails (daemon gone), events are dropped, not blocked on. The hook
stays installed and harmless — allocations continue to work normally in the
target, capture is just silently lost until (if ever) a daemon reconnects.
**The target is never aware the daemon died** — this is the core safety
property: injection failure modes degrade to "we stop observing," never to
"the target misbehaves."

### 4.4 Target exits mid-session

No special handling needed beyond what already exists: the target's process
teardown closes its end of the named pipe, which the daemon's existing
`ingest.rs` loop already treats as a normal disconnection (it already
handles a cooperative producer exiting mid-run). The daemon additionally
uses the now-activated PID (§1.2) to mark the session as ended (rather than
"awaiting the next connection," which is its current behavior for a
disconnect) and to prompt Flutter to show "target process exited" rather
than silently sitting on a stale graph — surfaced as an unprompted
`ControlResponse::TargetExited { pid }` broadcast (Step 4), not just a
server-side log line.

**The pid on that disconnect event is load-bearing, not decorative — a real
race, found and fixed during Step 4's review.** A target *switch*
(`TargetCmd::Attach` while one is already live) detaches the old target,
then immediately attaches the new one, all within one `tokio::select!`
branch's execution. The old target's pipe-close is detected *asynchronously*
by `ingest.rs`, on its own task, and is not synchronized with
`injector::detach()`'s completion — that disconnect event can arrive
*after* the graph loop has already moved `attached_pid` on to the new
target. Without checking which pid the disconnect is actually for, that
stale event would be indistinguishable from the new target exiting:
`attached_pid` would be wrongly cleared and `TargetExited` broadcast for a
target that had just successfully attached and was still running. Fixed by
carrying the pid on `GraphMsg::TargetDisconnected` and gating the
clear/broadcast on it matching the currently-tracked target
(`msg::is_current_target_exit`, unit-tested directly — the race itself
isn't reliably reproducible in a fast deterministic test, so the decision
rule is pinned down instead). A plain, non-switch `DetachTarget` was
already safe from this by construction (it clears `attached_pid`
synchronously *before* the pipe even starts closing), so only the switch
path needed the fix — but both now go through the same pid-checked path.

### 4.5 Reversibility summary

| Scenario | Target impact |
|---|---|
| Clean detach (§4.1) | None — hooks fully removed, DLL unloaded |
| Daemon dies mid-session | None — hook stays installed, drops events silently, target unaffected |
| HeapLens app closed (via launcher teardown) | Handled — launcher requests a bounded-timeout detach before tearing down the daemon (§4.6). No longer an accepted gap. |
| Target exits first | Pipe closes, daemon treats as normal disconnect, no target-side impact possible since the target is already gone |

### 4.6 Launcher teardown must request detach first (in scope, not deferred)

**Revised from the initial draft, which proposed accepting this as a gap —
that recommendation is wrong and is withdrawn.** "Harmless to the target's
execution" (§4.2/§4.3 prove the target keeps running correctly with an
abandoned hook) is not the same question as "acceptable to leave behind."
Closing HeapLens while attached would otherwise leave live trampolines
installed in a process the user doesn't own or control, invisible to that
process's own user, removed only whenever that process happens to exit on
its own. That is a persistent, silent modification to third-party software
— a real responsibility problem even though it is provably safe, and not
something to wave off with a safety argument that answers a different
question.

**In-scope fix:** the launcher already performs an ordered shutdown (it
explicitly stops the daemon after the Flutter app exits, ahead of relying
solely on the Job Object, per the existing `main.rs` teardown logic). This
extends that same ordered sequence with one more step, before the existing
daemon-stop:

1. Flutter app exits (as today).
2. **New:** launcher sends the daemon a `Shutdown` control message (small
   addition to the same WS control channel used for `AttachTarget`/
   `DetachTarget`, §3.1) if a target is currently attached; the daemon runs
   the normal detach sequence (§3.4 steps 1–2, spawn
   `heaplens-injector.exe <pid> --detach`, wait for the pipe to close) in
   response.
3. This wait is **bounded** — a short timeout (implementation detail, on
   the order of a few seconds; exact value is an implementation-time
   tuning choice, not a design commitment here). If the timeout elapses
   (hung target, hung injector, anything), the launcher proceeds to its
   existing teardown (kill daemon via Job Object) exactly as before. The
   fallback on timeout is **the current behavior**, not a worse one — this
   change can only improve the common case, never regress the timeout
   case.
4. Daemon and Flutter processes stop as today (unchanged).

This is a small, additive change: one new control message, one bounded
wait inserted into a shutdown sequence the launcher already performs in
order. It does not change the launcher's process-spawning or Job Object
logic. Promoted from §7's "out of scope" to in-scope for this stage — see
the implementation plan (§8) for where it lands in build order.

---

## 5. The relevance/detail problem (independent of injection)

### 5.1 Root cause of "no lines between nodes"

Investigated directly against the current codebase. **Conclusion: this was
the release debug-info φ collapse (§2's underlying mechanism), not a
renderer defect.**

- `graph_canvas.dart`'s edge painter (`_paintEdges`) draws a line for every
  `(self, target)` pair in `node.edges`, with a fixed low-alpha stroke and
  no threshold, opacity gate, or conditional visibility logic beyond both
  endpoints needing to exist in the force-sim layout. If the daemon reports
  an edge, Flutter draws it — there is no code path where a real edge is
  silently suppressed.
- `NodeDto.edges` mirrors `infer_ownership`'s output directly. If φ produces
  zero edges (missing debug info → raw hex fallback names → no name-set
  intersections, per §2), the Flutter renderer has nothing to draw, and "no
  lines" is the correct, honest rendering of "the daemon reported no
  ownership."

This traces to the same class of bug already fixed this session
(`[profile.release] debug = true`). If "no lines" is observed again against
the current `dist/` build (which already carries that fix and is under
verification via the Gate 1/2 visual run sheet), it would represent a
genuine new regression worth re-opening systematic debugging on — but there
is no evidence of that; the mechanism fully explains the historical
observation without invoking a renderer bug.

### 5.2 Proposed detail-surfacing additions — moved out of Stage 7 entirely

**Revised: these are not part of Stage 7 in any form.** They are recorded
here because the original prompt asked for them, but they are Flutter-only
rendering work, fully decoupled from injection, and must not be bundled
into the injection branch — bundling them would entangle injection's
acceptance testing with unrelated rendering changes, muddying what a test
failure means. This is a **separate, later, named task**: "Stage 7b —
graph detail surfacing" (or whatever sequence number is current when it's
picked up), tracked independently and not started as part of this design's
implementation plan (§8).

**One exception, and it is conditional, not pulled forward as scheduled
work:** the edge-opacity one-liner below (`0x33` → a higher default) may be
worth doing sooner *only if* Nash's visual-gate observations (the
already-in-flight φ re-verification, unrelated to this design) report edges
being hard to see at the current alpha. That would be a one-line tuning
change triggered by that observation, not by this design — it does not
belong to Stage 7's scope or build order either way.

These remain small, independently-shippable UI improvements when their
time comes — not gated on injection landing first, and not required for
injection to work:

- **Always-visible compact node label.** Currently, per-node detail
  (`node_detail.dart`) is a selection-driven panel — you have to click a
  node to see its symbol/size. Add a small always-on label near each node
  (short symbol name, current size) rendered directly on the canvas,
  toggleable via a density setting for graphs with many nodes (labels for
  every node in a 500-node graph would be unreadable clutter — this needs
  an opt-in or an auto-hide-below-threshold rule, e.g. show labels only
  when node count is under some small N, or only for hovered/nearby nodes).
- **Edge direction and emphasis.** Current edges are undirected-looking
  lines (a plain stroke, no arrowhead) even though ownership is directional
  (owner → child). Add a small arrowhead or a taper (thicker at the owner
  end) so ownership direction is visually legible without clicking through
  to the detail panel. Also consider a slightly higher default opacity —
  the current `0x33FFFFFF` (~20% white) is easy to miss against a busy
  graph; this is a one-line tuning change, not a structural one.
- **Side list of ownership relations.** For graphs too dense to read
  visually, add an optional side panel listing `owner → child` pairs as
  scrollable text (symbol names, not just node ids), filterable/searchable.
  This gives an exact, unambiguous view of φ's output independent of the
  force-sim layout, useful both for demoing φ's correctness (mémoire
  defense) and for real debugging sessions where the graph is too tangled
  to read visually.

None of these require daemon/protocol changes — `NodeDto` already carries
everything needed (symbol name via existing fields, size, edges); these are
Flutter-only rendering and layout work.

---

## 6. Summary of protocol/architecture impact

- **Wire protocol (`heaplens-protocol`):** the pipe/event protocol
  (`Frame`/`AllocEvent`, the daemon-facing hook→daemon transport) has no
  schema change. `Frame::Handshake`'s `pid` field goes from
  decoded-and-discarded to decoded-and-used — a semantic activation, not a
  format change. This "no schema change" claim is scoped to that pipe/event
  surface specifically, **not** the WS surface: Step 3 added a genuinely
  new `heaplens-protocol::control` module (`ControlRequest`/
  `ControlResponse`/`ProcessInfo`) for the daemon↔client WS control
  channel — a different transport from the pipe/event one, tagged JSON
  alongside the pre-existing `GraphMessage` on the same WS connection (no
  new connection/port). The distinction matters: the capture pipeline's own
  wire format is untouched by injection; the UI-facing control surface
  necessarily grew to carry attach/detach/process-list requests that did
  not previously exist.
- **Daemon (`heaplens-daemon`):** `ingest.rs` needs no protocol changes to
  accept an injected session (a hook DLL connection is indistinguishable
  from a cooperative producer's) — it does now forward the Handshake pid as
  `GraphMsg::TargetConnected`/detect disconnect as `TargetDisconnected`,
  both consumed by the graph-loop task. New: WS control messages
  (`ListProcesses`, `AttachTarget`, `DetachTarget`), process enumeration
  (`procs.rs`, real per-process `IsWow64Process2` arch probing — not
  stubbed), spawning/waiting on `heaplens-injector` (`injector.rs`),
  session-lifecycle bookkeeping (now-active PID, target-exit detection via
  the now-used Handshake pid), graph-clear on target switch (§3.4).
- **φ / graph (`graph.rs`):** No changes. Its dependency on symbolizable
  frames is unchanged and is now understood to be inherent to the injection
  use case, not just a build-configuration concern (§2).
- **Flutter app:** No changes required for injection to function at the
  wire level. New: a process-picker screen/dialog and the WS calls it
  makes. The detail-surfacing proposals (§5.2) are **not** part of this
  stage — moved to a separate, later task.
- **Launcher (`heaplens-launcher`):** New: a bounded-timeout detach request
  inserted into the existing ordered shutdown sequence, before the current
  daemon-stop step (§4.6). This is now in-scope, not a deferred gap.
- **New crates:** `heaplens-injector` (binary), `heaplens-hook` (cdylib).
  New dependency: `minhook`.

---

## 7. Explicitly out of scope for this stage

- Concurrent multi-target monitoring (confirmed scope decision, §0/3.4) —
  would require a per-connection task model in `ingest.rs`, a
  source-process discriminator in the graph/`NodeDto`, and PID
  filtering/color-coding in Flutter.
- Auto-elevation or any privilege escalation beyond a normal `OpenProcess`
  call (§3.3) — a deliberate security boundary, not a deferred feature.
- The detail-surfacing UI proposals (§5.2) — designed here for completeness
  since the original prompt asked for them, but explicitly deferred to a
  separate, later, independently-tracked task ("Stage 7b"), not bundled
  into this stage's branch or acceptance testing.

**No longer out of scope, revised from the initial draft:** the
launcher-teardown detach fix (§4.6) was originally proposed as an accepted
gap. That recommendation was wrong — "safe to leave behind" is not
"acceptable to leave behind" for a persistent modification to a process
HeapLens doesn't own — and it is now in-scope, specified in §4.6, and
included in the build order (§8).

---

## 8. Staged implementation plan

Each step below has its own acceptance gate. **Do not proceed to the next
step until the current step's gate passes.** This mirrors the pattern
already used for prior stages (M5 canvas-render, φ ownership fix): land and
verify one layer before building the next on top of it, so a failure is
always attributable to the step that just landed, not to an accumulation of
unverified layers.

### Step 1 — `heaplens-hook` (cdylib), tested by manual `LoadLibrary`, no injection yet

Build the hook DLL in isolation first, loaded the *easy* way — a small test
harness process that statically links nothing but calls
`LoadLibraryW("heaplens_hook.dll")` on itself, then calls the exported
`HeapLensHookAttach` directly (in-process, not via `CreateRemoteThread`).
This isolates "does the hook itself work" from "does injection work,"
which are separable concerns and should not be debugged simultaneously.

Covers: MinHook installation on `RtlAllocateHeap`/`RtlReAllocateHeap`/
`RtlFreeHeap` (§1.1), thread-local reentrancy guard, private heap for
internal allocations, ring buffer + writer thread (reused from
`heaplens-alloc`), eager writer-start/symbol-resolution warm-up before
hooks go live (§1.3), pipe connection, `Handshake` with real `pid`, event
framing, clock rebased to attach-time, `HeapLensHookDetach` disabling
hooks before stopping the writer and cleanly uninstalling everything
(§4.1).

**Done — complete, per the design as revised above (`dev/phase_7`
commit `b3a22c2`).** One methodology refinement versus this section's
original plan: the gate matches captured events to the workload by
*pointer identity*, not a raw global count as first specified — the
`Rtl*Heap` hook layer (§1.1) intercepts ambient, expected Windows-internal
heap traffic alongside the workload's own calls, so a raw count is not, in
fact, exact-comparable; identity matching isolates workload correctness
from that noise. See §1.1 for why this layer was chosen over the
originally-planned `HeapAlloc` layer.

**Acceptance gate — must assert correctness, not just "it ran."** A gate
that only confirms the DLL loads and doesn't crash would pass with a
silently broken capture path, and step 6 would inherit exactly the
ambiguity §7's step 6 ordering rule was designed to eliminate. This gate
holds capture to the same bar Stage 2's allocator was held to:

1. With `heaplens-daemon` running standalone (as it does today for
   cooperative producers), the test harness self-attaches and runs a
   **known, scripted workload with predetermined shape** — a fixed number
   of allocs of specific sizes, a fixed number of reallocs (including at
   least one on an untracked/pre-attach-simulated pointer to exercise the
   §1.4 fallback), a fixed number of frees.
2. **The gate asserts, not just observes:** the daemon-side event count for
   the session equals the workload's known event count exactly; each
   captured `AllocEvent.size` matches the corresponding workload
   allocation's requested size exactly; no event is missing and no
   duplicate/phantom event appears. This is a pass/fail numeric comparison
   against the scripted workload, the same rigor as Stage 2's allocator
   tests — "the daemon received *something*" does not pass this gate.
3. Then self-detaches; a second, equally scripted allocation burst
   performed *after* detach must produce **zero** captured events at the
   daemon, confirmed by the same count assertion (zero, not "fewer" or
   "looks quiet") — proving hooks are fully removed, not just quiesced.

Only a gate written this way — fixed workload in, fixed count/size
assertion out, both before and after detach — proves the capture pipeline
itself is correct in isolation, before injection introduces its own
variables in Step 2.

### Step 2 — `heaplens-injector`, tested against a cooperative, separate target

Build the injector: architecture check (`IsWow64Process2`), `OpenProcess`
with minimal rights, the two-step `CreateRemoteThread` sequence
(`LoadLibraryW` then `HeapLensHookAttachRemote`), and the detach handshake
(`QueueUserAPC`-driven `HeapLensHookDetachApc`, then `FreeLibrary` once it
reports success — see §4.1 for why detach does not mirror attach's
`CreateRemoteThread` pattern).

**Acceptance gate:** run a plain, separate, debug-info-carrying test
process (`injection_target.exe` — not Step 1's self-load harness, which
tests a different code path; not a stripped/no-debug-info target, that is
Step 6's deliberate later sequencing) as an *already-running* process, and
have `heaplens-injector <pid> --attach` inject into it externally. Confirm
identical results to Step 1's gate (event capture correct, clean detach)
but now via real cross-process injection. Additionally verify the
validation paths from §3.3 (nonexistent/inaccessible PID, malformed PID)
each produce the specified error message rather than crashing the injector
or the target. This is the step where loader-lock safety (§4.1) and the
private-heap reentrancy guard (§4.2) get their real test — they cannot be
meaningfully verified until injection is happening into a separate process.

**Done — complete (`dev/phase_7`).** Real cross-process injection surfaced
three further hazards self-load could not (§4.1's fifth, sixth, and seventh
hazards) — validating this step's own separation rationale from Step 1.
Test: `crates/heaplens-daemon/tests/hook_injection_wire.rs`, mirroring Step
1's pointer-identity matching methodology but driving real attach/detach via
`heaplens-injector.exe` subprocess calls timed against the target's own
`TARGET_PID=`/`WORKLOAD_DONE` stdout markers.

### Step 3 — daemon WS control messages + process enumeration

Add `ListProcesses`, `AttachTarget`, `DetachTarget` as an inbound WS
control-request channel (`heaplens-protocol::control::ControlRequest`/
`ControlResponse`, a new tagged-JSON pair alongside the existing
`GraphMessage`, on the *same* WS connection — no new transport).
`Shutdown` (§4.6) is deliberately **not** added here — it belongs to the
launcher's own teardown sequence, which Step 5 owns; bundling it into Step
3 would mean testing it before Step 5's dedicated gate exists for it.
Implement `CreateToolhelp32Snapshot`-based enumeration (`procs.rs`), the
daemon-side orchestration of spawning `heaplens-injector` (§3.2,
`injector.rs` — a short-lived, synchronous child process per attach/detach
call, not a long-lived process to track), the single-target transition
sequence (§3.4, including graph-clear), and target-exit detection via the
now-activated Handshake `pid` (§4.4, `GraphMsg::TargetConnected`/
`TargetDisconnected`).

All three control requests and the target-switch transition are owned by
the same single-threaded graph-loop task in `main.rs` that already owns
graph/resolver/storm-tracker state (not a separate actor) — the natural
place to clear that state on a switch, since no message round-trip to
another task is needed to do it.

**Acceptance gate:** without any Flutter UI yet, drive these WS messages
directly against the daemon's existing WS endpoint, against a real,
separate, cooperative target (`injection_target.exe`, Step 2's target, run
as an already-running process — not Step 1's self-load harness, which has
no separate pid to attach to). Confirm: process list includes the target
with correct pid/name/arch; `AttachTarget` results in events flowing and
appearing in the daemon's graph, driven purely through the WS control
channel (no direct `heaplens-injector` CLI call from the test); sending
`AttachTarget` for a second target while one is attached correctly runs the
full detach-then-clear-then-attach sequence (§3.4), with an observed empty
snapshot between the two targets' node sets as positive evidence of a
clear, not a merge; `DetachTarget` cleanly ends the session.

**Done — complete (`dev/phase_7`).** Test:
`crates/heaplens-daemon/tests/control_target_switch.rs`, run against the
real `heaplens-daemon.exe` binary (not an in-process mini graph loop —
Step 3's actual new code, process spawning and the switch transition, only
lives in `main.rs`'s real graph loop). One regression caught and fixed
during this step: two pre-existing tests
(`hook_self_load_wire.rs`/`cross_process_wire.rs`) had their own local
`GraphMsg` collector loops with a `_ => break` wildcard that had, until
this step, never needed to match anything beyond `Events`/`Symbols` — once
`ingest.rs` started forwarding the newly-activated Handshake as
`GraphMsg::TargetConnected`, that wildcard caught it and silently ended
event collection at the first message of every run (before any real
events arrived), which made both gates report zero captured events. Fixed
by adding explicit ignore arms for the two new `GraphMsg` variants in both
files, ahead of their `_ => break` catch-alls.

### Step 4 — Flutter process picker

Add the picker screen/dialog: request `ListProcesses`, render the list,
send `AttachTarget` on selection, surface attach-failure messages from §3.3
verbatim, show "target process exited" per §4.4, show the attach-time-clock
note per §1.4.

**Acceptance gate:** end-to-end from the running Flutter app — open the
picker, see the Step 1 harness (or any known test target) in the list,
select it, watch the graph populate with real nodes as the target
allocates. This is the first point where the whole chain (hook → injector
→ daemon → WS → Flutter) is exercised together.

### Step 5 — launcher teardown fix (§4.6)

Add the bounded-timeout `Shutdown`-then-detach step to the launcher's
existing ordered shutdown sequence.

**Acceptance gate — deliberately partial at this step; do not mark step 5
"done" on this alone.** Step 5's real-world scenario (attach a target,
close HeapLens, confirm the hook is actually gone from that target) needs
a working injection path to test honestly, and that path is still being
proven out in Step 6. Claiming step 5 complete from an isolated test alone
would repeat the exact mistake the review caught in the original §4.6
draft — asserting a safety property before the mechanism that proves it
exists. So step 5's gate here is scoped to what *can* be verified without
a real target:

- **Timeout-logic test, isolated:** stub or mock the detach round-trip
  (the daemon side of the `Shutdown` message can be faked to either
  respond promptly or never respond) and confirm the launcher waits for a
  prompt response, and separately confirm it proceeds to its existing
  teardown once the bounded timeout elapses on a non-responding stub —
  without hanging indefinitely. This proves the launcher's timeout/fallback
  *logic* is correct in isolation.
- **Explicitly not proven here:** that a real hook is actually removed from
  a real target as a result of this sequence. That property is real-target-
  dependent and is deferred to Step 6.

**The "no hook left resident" property is completed as an explicit
checklist item in Step 6**, not claimed here: once Step 6's real injected
target exists, close HeapLens with that target attached and confirm (via
the target's own continued execution, plus a way to check whether MinHook's
hooks are still installed — e.g. the Step 1 harness logging its own hook
state) that the hook was cleanly removed within the bounded timeout before
the daemon exited. Only once *that* checklist item passes is the full §4.6
guarantee actually proven end to end — step 5's isolated gate is necessary
but not sufficient on its own.

### Step 6 — end-to-end injection test, sequenced to avoid an ambiguous read

**This sequencing matters and must not be skipped or reordered.** Two
different failure modes look identical on screen — "injection captured
nothing because injection is broken" and "injection captured allocations
correctly but φ correctly shows zero ownership edges because the target
has no debug info" — and testing against a stripped binary first would
make it impossible to tell which one occurred.

1. **First target: a program built by this project, with debug info
   present**, e.g. one of the existing `heaplens-alloc` example producers
   *built without* linking `heaplens-alloc` as its global allocator (so it
   has no cooperative capture path — injection is the only way it gets
   observed) but *with* `debug = true` (matching the release-profile fix
   already in the workspace `Cargo.toml`). Attach and confirm: allocations
   are captured, sizes are correct, **and φ produces real, meaningful
   ownership edges** (a star or chain shape, matching the visual-gate
   expectations already established for cooperative producers). This
   proves the capture path is correct, independent of the φ-degradation
   question — if this step doesn't produce edges, the bug is in injection
   or capture, not in φ's expected degradation behavior.
2. **Second target: a genuinely stripped, release-optimized third-party
   binary** (no debug info available) that the project didn't build.
   Attach and confirm: allocations are still captured (sizes, counts, the
   memory-map view, leak-by-growth detection — everything §2 lists as
   unaffected), while ownership edges degrade toward zero/roots, matching
   §2's predicted, honest degradation. This step is the actual thesis
   demonstration: "here is a real program we don't control, here is what
   the tool can and cannot tell you about it, and here is why."

3. **Checklist item carried over from Step 5 (§4.6's real proof):** with
   target 1 (the debug-info-carrying target from item 1, still running)
   attached, close the HeapLens app window and confirm the hook is fully
   removed from that target within the bounded timeout — the same check
   described at the end of Step 5, now run for real. Step 5's isolated
   timeout-logic test is necessary but was explicitly not sufficient; this
   is the item that actually closes it out. Do not consider §4.6 done, or
   step 5 done, until this passes.

Only after item 1, item 2, and item 3 are all observed and distinguished
can Stage 7 be called functionally complete. Do not run step 2 before step
1 passes.
