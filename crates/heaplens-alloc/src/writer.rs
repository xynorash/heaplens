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
        if crate::writer_should_stop() {
            crate::mark_writer_stopped();
            return;
        }
        symbol_cache.clear();

        // ── Connect ─────────────────────────────────────────────────────────
        let mut pipe = loop {
            if crate::writer_should_stop() {
                crate::mark_writer_stopped();
                return;
            }
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
        let mut new_syms: Vec<(u64, String, bool)> = Vec::new();
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
                        let is_machinery = is_machinery_symbol(&name);
                        symbol_cache.insert(addr, name.clone());
                        new_syms.push((addr, name, is_machinery));
                    }
                }

                // Emit SYMBOLS frame for newly-seen addresses.
                if !new_syms.is_empty() {
                    let refs: Vec<(u64, &str, bool)> = new_syms
                        .iter()
                        .map(|(a, n, m)| (*a, n.as_str(), *m))
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
            } else {
                if crate::writer_should_stop() {
                    crate::mark_writer_stopped();
                    return;
                }
                thread::sleep(POLL_SLEEP);
            }
        }
        // Fell through 'send → reconnect. symbol_cache is cleared at the top
        // of the reconnect loop so all addresses are re-resolved and re-sent.
    }
}

/// Prefixes of resolved symbol names that belong to the shared allocation
/// instrumentation chain (this crate, the backtrace walker, and the std/core
/// allocator internals) rather than genuine caller code. These are always
/// fully-qualified crate paths, so a leading-prefix match is correct.
/// Extending or reordering this list does not affect the wire format or the
/// hot path — classification happens here, in the writer, off the
/// producer's critical path, and is shipped once per newly-seen address via
/// the SYMBOLS frame.
const MACHINERY_PREFIXES: &[&str] = &[
    "heaplens_alloc::",
    "backtrace::",
    "alloc::alloc::",
    "alloc::raw_vec::",
    "alloc::vec::from_elem",
    "alloc::vec::spec_from_elem",
    "alloc::slice::",
    "core::alloc::",
    "core::ptr::drop_in_place",
];

/// Compiler-generated `__rust_alloc`/`__rust_dealloc`/`__rust_realloc`/
/// `__rust_no_alloc_shim_is_unstable_v2` shims — unlike the crate-internal
/// machinery above, these are namespaced under the *consuming* binary's own
/// module path (observed: `wire_producer::_::__rust_alloc`, not a bare or
/// `alloc::`-prefixed symbol), so they must be matched as a substring
/// anywhere in the name, not a prefix. Confirmed necessary empirically: a
/// prefix-only check silently misclassified these as real caller code,
/// which made every node's effective site collapse onto this shim (the same
/// failure mode as the original unfiltered-stack[0] bug, one layer out) and
/// produced zero ownership edges against the real wire_producer.exe despite
/// the function-name-matching fix being logically correct.
const MACHINERY_SHIM_SUBSTRINGS: &[&str] = &[
    "__rust_alloc",
    "__rust_dealloc",
    "__rust_realloc",
    "__rust_no_alloc_shim",
];

fn is_machinery_symbol(name: &str) -> bool {
    MACHINERY_PREFIXES.iter().any(|p| name.starts_with(p))
        || MACHINERY_SHIM_SUBSTRINGS.iter().any(|s| name.contains(s))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_crate_qualified_prefixes_as_machinery() {
        assert!(is_machinery_symbol("heaplens_alloc::capture::capture_stack"));
        assert!(is_machinery_symbol("backtrace::backtrace::trace_unsynchronized"));
        assert!(is_machinery_symbol("alloc::alloc::Global::alloc_impl_runtime"));
        assert!(is_machinery_symbol("alloc::raw_vec::RawVecInner::try_allocate_in"));
        assert!(is_machinery_symbol("core::alloc::global::GlobalAlloc::alloc_zeroed"));
        assert!(is_machinery_symbol("core::ptr::drop_in_place<alloc::vec::Vec<u8>>"));
    }

    #[test]
    fn classifies_consumer_namespaced_rust_alloc_shims_as_machinery() {
        // Regression: confirmed via a real wire_producer.exe run that these
        // shims are namespaced under the *consuming* binary's own module
        // path, not under a bare or `alloc::`-prefixed symbol — a
        // prefix-only check silently missed them and made every node's
        // effective site collapse onto this shim, producing zero real
        // ownership edges end-to-end despite correct φ matching logic.
        assert!(is_machinery_symbol("wire_producer::_::__rust_alloc"));
        assert!(is_machinery_symbol("wire_producer::_::__rust_alloc_zeroed"));
        assert!(is_machinery_symbol("some_other_producer::_::__rust_dealloc"));
        assert!(is_machinery_symbol("demo_producer::_::__rust_realloc"));
        assert!(is_machinery_symbol("__rustc[8068f81614cfe5c]::__rust_no_alloc_shim_is_unstable_v2"));
    }

    #[test]
    fn does_not_classify_real_caller_code_as_machinery() {
        assert!(!is_machinery_symbol("wire_producer::nested_alloc"));
        assert!(!is_machinery_symbol("wire_producer::leaf_alloc"));
        assert!(!is_machinery_symbol("wire_producer::main"));
        assert!(!is_machinery_symbol("demo_producer::make_family"));
        assert!(!is_machinery_symbol("my_crate::foo::Bar::do_work"));
        // The check is keyed on "__rust_" + known op, never on bare "alloc" —
        // a function whose NAME happens to contain "alloc" as an English word
        // must not be swallowed into machinery. This is the failure mode a
        // `.contains("alloc")` fix would reintroduce, in the opposite
        // direction from the original bug (real user frames misclassified as
        // machinery instead of the reverse).
        assert!(!is_machinery_symbol("myapp::allocate_buffer"));
        assert!(!is_machinery_symbol("myapp::Allocator::new"));
        assert!(!is_machinery_symbol("myapp::reallocate_pool"));
    }

    #[test]
    fn shim_family_complete_across_all_four_ops_bare_and_namespaced() {
        // All four compiler-generated shim ops, both bare (as they'd appear
        // if a future toolchain ever emits them unnamespaced) and namespaced
        // under an arbitrary consuming binary — the form actually observed.
        for sym in [
            "__rust_alloc",
            "__rust_dealloc",
            "__rust_realloc",
            "__rust_alloc_zeroed", // covered via the "__rust_alloc" substring
            "__rust_no_alloc_shim_is_unstable_v2",
            "demo_producer::_::__rust_alloc",
            "demo_producer::_::__rust_dealloc",
            "demo_producer::_::__rust_realloc",
            "demo_producer::_::__rust_alloc_zeroed",
            "hot_producer::_::__rust_alloc",
        ] {
            assert!(is_machinery_symbol(sym), "missed shim: {sym}");
        }
    }
}
