//! TEMPORARY diagnostic module for the hook owner-free crash investigation
//! (Nash's observed `0xC0000005` on "case 2": owner freed while children
//! remain live, under real cross-process injection). Not a fix — installs a
//! Vectored Exception Handler that logs whatever fault fires (exception
//! code, faulting address/RIP, access-violation type + faulting memory
//! address, and a best-effort backtrace) to a fixed file next to the DLL,
//! then lets the crash proceed exactly as it would without this handler
//! (`EXCEPTION_CONTINUE_SEARCH` — never claims to have handled anything).
//!
//! Strip this module, its call site in `lib.rs`, and the `backtrace`
//! dependency in `Cargo.toml` once the investigation closes (see the
//! `TEMPORARY` markers in both files).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    EXCEPTION_RECORD,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

const LOG_PATH: &str = "heaplens_hook_veh.log";
const EXCEPTION_ACCESS_VIOLATION: i32 = 0xC0000005u32 as i32;

static INSTALLED: AtomicBool = AtomicBool::new(false);

/// Installs the diagnostic handler once per process (idempotent — safe to
/// call from every `attach_impl` run, including re-attach). Called as early
/// as possible in `attach_impl`, before any hook is installed, so it can
/// catch a fault anywhere in the attach sequence too, not just steady-state
/// capture.
pub fn install() {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return;
    }
    // `1` = call this handler first, ahead of any handler the target
    // process itself may have installed — so we see the fault before
    // anything else has a chance to obscure or partially handle it.
    unsafe {
        AddVectoredExceptionHandler(1, Some(veh_handler));
    }
}

unsafe extern "system" fn veh_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    // Defensive: a VEH can in principle be called with a null pointer for
    // exotic exception types; never dereference blindly.
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let ptrs = unsafe { &*info };
    if ptrs.ExceptionRecord.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let record: &EXCEPTION_RECORD = unsafe { &*ptrs.ExceptionRecord };

    let mut msg = String::new();
    let _ = writeln!(msg, "=== heaplens_hook diag VEH ===");
    let _ = writeln!(msg, "thread_id: {}", unsafe { GetCurrentThreadId() });
    let _ = writeln!(msg, "exception_code: 0x{:08X}", record.ExceptionCode as u32);
    let _ = writeln!(msg, "exception_address: {:?}", record.ExceptionAddress);

    if !ptrs.ContextRecord.is_null() {
        let ctx: &CONTEXT = unsafe { &*ptrs.ContextRecord };
        let _ = writeln!(msg, "rip: 0x{:016X}", ctx.Rip);
        let _ = writeln!(msg, "rsp: 0x{:016X}", ctx.Rsp);
        let _ = writeln!(msg, "rbp: 0x{:016X}", ctx.Rbp);
    }

    if record.ExceptionCode == EXCEPTION_ACCESS_VIOLATION && record.NumberParameters >= 2 {
        let access_type = record.ExceptionInformation[0];
        let faulting_addr = record.ExceptionInformation[1];
        let kind = match access_type {
            0 => "read",
            1 => "write",
            8 => "data-execution-prevention",
            other => {
                let _ = write!(msg, "access_violation_type: unknown({other}) ");
                "unknown"
            }
        };
        let _ = writeln!(msg, "access_violation: {kind} at 0x{faulting_addr:016X}");
    }

    // Best-effort backtrace — this is the same `backtrace` crate and
    // resolution path already exercised on the writer thread during normal
    // operation (see heaplens_alloc's symbol-resolution warm-up), so it is
    // not introducing a fundamentally new risk here. If it itself faults or
    // hangs, the exception-code/address/RIP lines above have already been
    // formatted (though not yet flushed to disk — see the single write
    // below); that is an accepted, understood limitation of a diagnostic
    // handler running inside an already-faulting process.
    let bt = backtrace::Backtrace::new();
    let _ = writeln!(msg, "backtrace:\n{bt:?}");
    let _ = writeln!(msg, "=== end ===\n");

    // Single append write, plain std::fs — matches how the rest of this
    // module already allocates (via the private-heap global allocator,
    // trampoline-backed, never routes through hook_heap_* capture logic).
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(LOG_PATH) {
        let _ = f.write_all(msg.as_bytes());
        let _ = f.flush();
    }

    EXCEPTION_CONTINUE_SEARCH
}
