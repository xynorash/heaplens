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

### 1.1 Mechanism: inline hooking via `minhook`, on the `HeapAlloc` layer

**Chosen:** a hook DLL installs inline (trampoline) hooks on
`HeapAlloc` / `HeapReAlloc` / `HeapFree` (kernelbase/kernel32), using the
`minhook` crate (Rust bindings to the MinHook library).

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

**Why the `HeapAlloc` layer, not CRT `malloc`:** CRT `malloc` calls
`HeapAlloc` internally on Windows. Hooking both would double-count every CRT
allocation. `HeapAlloc`/`HeapReAlloc`/`HeapFree` is the common sink — it
catches CRT-backed allocations *and* direct-Win32 allocators that bypass the
CRT entirely, and avoids per-CRT-version `malloc` symbol variance (static vs
dynamic CRT, debug vs release CRT naming). The tradeoff: this observes heap
*operations*, not CRT-level semantics (e.g. `calloc`'s zeroing is invisible
as a distinct operation from a plain alloc). That's acceptable — HeapLens is
a memory-topology tool, not a CRT-semantics tool.

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
  (`ring.rs`), the background writer thread, symbol resolution, and pipe
  framing are **reused unmodified in spirit** — only the *entry point*
  differs. Where `heaplens-alloc` hangs off `#[global_allocator]` (a
  compile-time hook available only to a statically-linked producer),
  `heaplens-hook` hangs the same capture call off MinHook-installed
  trampolines on `HeapAlloc`/`HeapReAlloc`/`HeapFree`. This is the precise
  boundary of what's reusable: **the capture pipeline is shared code; the
  interception mechanism is new, because compile-time and inject-time
  interception are fundamentally different hooks into fundamentally
  different points in the allocator's lifecycle.**

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

### 4.1 Attach/detach as two explicit exported entry points, not `DllMain`

`heaplens-hook.dll` exports two functions, `HeapLensHookAttach` and
`HeapLensHookDetach`. Neither runs from `DllMain`. Running MinHook
installation or spawning threads from `DllMain` is a well-known deadlock
hazard (the loader lock is held during `DllMain`, and MinHook / thread
creation / pipe connection all need to run outside it). Instead,
`heaplens-injector` uses the standard two-step injection pattern:

1. `CreateRemoteThread` calling `LoadLibraryW` with the DLL path — loads the
   DLL, runs its (minimal, do-nothing) `DllMain`, returns the loaded module
   handle.
2. A second `CreateRemoteThread` calling `GetProcAddress`-resolved
   `HeapLensHookAttach` in the now-loaded module — this is where MinHook
   initializes, hooks are installed, the private heap and writer thread are
   created, and the pipe connection is opened. All real work happens here,
   safely outside the loader lock.

Detach is symmetric: `heaplens-injector --detach` calls
`HeapLensHookDetach` via `CreateRemoteThread` (disables and removes the
MinHook hooks, stops the writer thread, closes the pipe connection, frees
the private heap), then a final `CreateRemoteThread` calling
`FreeLibraryAndExitThread` to unload the DLL from the target. After detach
completes, the target process is left exactly as if `heaplens-hook.dll` had
never been loaded — no dangling hooks, no leftover threads.

### 4.2 Reentrancy inside the hook

The hook functions run **inside the target's own threads**, at the moment
they call `HeapAlloc`/`HeapFree`. If capturing an event itself allocates —
building the `AllocEvent`, growing a buffer, opening a pipe — that
allocation re-enters the hooked `HeapAlloc`, which re-enters the capture
code: unbounded recursion, crashing the target. This is the same hazard
Stage 2's cooperative allocator solved with a thread-local reentrancy guard
(`heaplens-alloc/src/guard.rs`) — `heaplens-hook` uses the identical
pattern, now load-bearing in a context where a crash means crashing
*someone else's process*, not our own test binary:

- A thread-local guard flag: if already inside the hook on this thread,
  call straight through to the real `HeapAlloc` (via the MinHook trampoline)
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
than silently sitting on a stale graph.

### 4.5 Reversibility summary

| Scenario | Target impact |
|---|---|
| Clean detach (§4.1) | None — hooks fully removed, DLL unloaded |
| Daemon dies mid-session | None — hook stays installed, drops events silently, target unaffected |
| HeapLens app closed (via launcher teardown) | Same as daemon dying, from the target's perspective — the launcher's Job Object teardown kills the daemon, not the target; a follow-up detach is not guaranteed. **This is a gap** (§4.6). |
| Target exits first | Pipe closes, daemon treats as normal disconnect, no target-side impact possible since the target is already gone |

### 4.6 Known gap: launcher teardown does not guarantee detach

The existing launcher (`heaplens-launcher`) kills the daemon via its Job
Object the moment the Flutter app closes. If a target is injected at that
moment, the daemon dies without getting a chance to send the
`--detach` sequence, leaving the hook installed in the target (harmless per
§4.3/§4.2, but not clean). Two options, to be decided at implementation
time, not in this design:
- Launcher-side: before closing, if a target is attached, wait for an
  explicit detach round-trip (adds a shutdown-ordering dependency the
  launcher doesn't have today).
- Accept the gap: an installed-but-inert hook is safe (per §4.2/§4.3) even
  if not tidy, and document it as "closing HeapLens while a target is
  attached leaves the hook resident and harmless in that process until it
  exits; explicitly detach first for a clean unload."

Recommendation: accept the gap for this stage and document it — the target
is provably safe either way, and building shutdown ordering into the
launcher is real scope for a corner case. Flag for revisit if the mémoire
defense wants a stronger guarantee here.

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

### 5.2 Proposed detail-surfacing additions (independent, scoped, optional)

These are small, independently-shippable UI improvements — not gated on
injection landing first, and not required for injection to work:

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

- **Wire protocol (`heaplens-protocol`):** No schema change.
  `Frame::Handshake`'s `pid` field goes from decoded-and-discarded to
  decoded-and-used. This is a semantic activation, not a format change.
- **Daemon (`heaplens-daemon`):** `ingest.rs` needs no changes to accept an
  injected session (a hook DLL connection is indistinguishable from a
  cooperative producer's). New: WS control messages (`ListProcesses`,
  `AttachTarget`, `DetachTarget`), process enumeration, spawning/waiting on
  `heaplens-injector`, session-lifecycle bookkeeping (now-active PID,
  target-exit detection via the now-used Handshake pid), graph-clear on
  target switch (§3.4).
- **φ / graph (`graph.rs`):** No changes. Its dependency on symbolizable
  frames is unchanged and is now understood to be inherent to the injection
  use case, not just a build-configuration concern (§2).
- **Flutter app:** No changes required for injection to function at the
  wire level. New: a process-picker screen/dialog and the WS calls it
  makes. The detail-surfacing proposals (§5.2) are separate, optional work.
- **Launcher (`heaplens-launcher`):** No changes required for injection to
  function. Known gap at teardown (§4.6), explicitly not fixed in this
  stage.
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
- Fixing the launcher-teardown detach gap (§4.6) — documented, accepted for
  this stage.
- The detail-surfacing UI proposals (§5.2) — designed here for completeness
  since the prompt asked for them, but they are independent work, not a
  precondition for or dependency of injection landing.
