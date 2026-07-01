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
        symbol_cache.clear();

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
        // Fell through 'send → reconnect. symbol_cache is cleared at the top
        // of the reconnect loop so all addresses are re-resolved and re-sent.
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
