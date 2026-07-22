//! Stage 7 Step 2 (docs/stage7-injection-design.md §8): a standalone,
//! synchronous tool that injects `heaplens_hook.dll` into a target process
//! and drives its exported `HeapLensHookAttachRemote`/detach entry points,
//! then unloads the DLL. No Rust-level dependency on
//! `heaplens-hook` — it locates the DLL and resolves its exports the same
//! way any injector would target an arbitrary DLL: `LoadLibraryW` +
//! `GetProcAddress`, `CreateRemoteThread` to run code in the target.
//!
//! Usage: `heaplens-injector <pid> --attach|--detach`

use std::ffi::c_void;
use std::path::{Path, PathBuf};

mod safety;

use windows_sys::Win32::Foundation::{CloseHandle, FreeLibrary, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_UNKNOWN};
use windows_sys::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeThread, IsWow64Process2, OpenProcess, OpenThread, QueueUserAPC,
    WaitForSingleObject, LPTHREAD_START_ROUTINE, PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION,
    PROCESS_VM_OPERATION, PROCESS_VM_READ, PROCESS_VM_WRITE, THREAD_SET_CONTEXT,
};

const DLL_NAME: &str = "heaplens_hook.dll";
const REMOTE_THREAD_TIMEOUT_MS: u32 = 10_000;

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn dll_path() -> PathBuf {
    // Ships next to heaplens-injector.exe in dist/HeapLens/, matching every
    // other component pair in this project (daemon/launcher/examples all
    // resolve paths relative to their own exe directory).
    let exe = std::env::current_exe().expect("current_exe");
    exe.parent()
        .expect("exe has a parent dir")
        .join(DLL_NAME)
}

/// Every failure path returns a message meant to be shown to the user
/// verbatim (per §3.3) — specific enough to say what went wrong and, where
/// applicable, what to do about it.
fn fail(msg: impl Into<String>) -> ! {
    eprintln!("heaplens-injector: {}", msg.into());
    std::process::exit(1);
}

struct RemoteProcess {
    handle: HANDLE,
    pid: u32,
}

impl RemoteProcess {
    /// Opens the target with the minimal rights actually needed, after
    /// confirming architecture match. Never requests `PROCESS_ALL_ACCESS`
    /// and never attempts elevation — if `OpenProcess` fails, that is
    /// reported as an access-denied condition, not retried with broader
    /// rights (§3.3).
    fn open(pid: u32) -> Self {
        // Architecture check first, via a query-only handle, before any
        // attempt to write to or execute in the target (§3.3). x64 cannot
        // inject into an x86 target — different address space layout,
        // calling convention, and the hook DLL itself is architecture-
        // specific.
        let probe = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid) };
        if probe.is_null() {
            fail(format!(
                "cannot open process {pid} to check its architecture — access denied, or the \
                 process does not exist. If the target requires elevation, re-launch HeapLens \
                 as Administrator; this tool does not attempt to elevate itself."
            ));
        }
        let mut process_machine = 0u16;
        let mut native_machine = 0u16;
        let ok = unsafe { IsWow64Process2(probe, &mut process_machine, &mut native_machine) };
        if ok == 0 {
            unsafe { CloseHandle(probe) };
            fail(format!("IsWow64Process2 failed for process {pid} — cannot verify architecture match"));
        }
        // process_machine != IMAGE_FILE_MACHINE_UNKNOWN means the target is
        // running under WOW64 emulation (i.e. it is a 32-bit process on a
        // 64-bit system) — not native at native_machine's architecture.
        if process_machine != IMAGE_FILE_MACHINE_UNKNOWN || native_machine != IMAGE_FILE_MACHINE_AMD64 {
            unsafe { CloseHandle(probe) };
            fail(format!(
                "target process {pid} is a 32-bit process; this build of HeapLens is 64-bit \
                 and cannot attach"
            ));
        }
        unsafe { CloseHandle(probe) };

        let rights = PROCESS_CREATE_THREAD | PROCESS_VM_OPERATION | PROCESS_VM_WRITE | PROCESS_VM_READ | PROCESS_QUERY_INFORMATION;
        let handle = unsafe { OpenProcess(rights, 0, pid) };
        if handle.is_null() {
            fail(format!(
                "access denied opening process {pid} — try running HeapLens as Administrator"
            ));
        }
        RemoteProcess { handle, pid }
    }

    /// Runs `start_addr` as a thread in the target, waits (bounded), and
    /// returns its exit code. `start_addr` must already be a valid address
    /// *in the target's address space* (either a system-DLL export shared
    /// at the same base across processes, like `LoadLibraryW`, or an
    /// address computed via `local_offset_to_remote`).
    fn run_remote_thread(&self, start_addr: *const c_void, param: *mut c_void) -> Result<u32, String> {
        let start: LPTHREAD_START_ROUTINE = Some(unsafe { std::mem::transmute::<*const c_void, unsafe extern "system" fn(*mut c_void) -> u32>(start_addr) });
        let mut tid = 0u32;
        let thread = unsafe {
            CreateRemoteThread(self.handle, std::ptr::null(), 0, start, param, 0, &mut tid)
        };
        if thread.is_null() {
            return Err(format!("CreateRemoteThread failed in process {}", self.pid));
        }
        let wait = unsafe { WaitForSingleObject(thread, REMOTE_THREAD_TIMEOUT_MS) };
        if wait != 0 {
            unsafe { CloseHandle(thread) };
            return Err(format!(
                "remote thread in process {} did not complete within {REMOTE_THREAD_TIMEOUT_MS}ms — \
                 target may have exited or hung",
                self.pid
            ));
        }
        let mut exit_code = 0u32;
        let ok = unsafe { GetExitCodeThread(thread, &mut exit_code) };
        unsafe { CloseHandle(thread) };
        if ok == 0 {
            return Err(format!("GetExitCodeThread failed in process {}", self.pid));
        }
        Ok(exit_code)
    }

    /// Finds `heaplens_hook.dll`'s loaded base address in the target, if
    /// present. Used both to confirm a `LoadLibraryW` actually landed (its
    /// own thread exit code truncates to 32 bits on x64 and can't be
    /// trusted as a full pointer) and, on detach, to find the module to
    /// unload.
    fn find_module_base(&self) -> Option<*mut u8> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, self.pid) };
        if snap.is_null() {
            return None;
        }
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut found = None;
        let mut ok = unsafe { Module32FirstW(snap, &mut entry) };
        while ok != 0 {
            let name_len = entry.szModule.iter().position(|&c| c == 0).unwrap_or(entry.szModule.len());
            let name = String::from_utf16_lossy(&entry.szModule[..name_len]);
            if name.eq_ignore_ascii_case(DLL_NAME) {
                found = Some(entry.modBaseAddr);
                break;
            }
            ok = unsafe { Module32NextW(snap, &mut entry) };
        }
        unsafe { CloseHandle(snap) };
        found
    }
}

impl Drop for RemoteProcess {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

/// Loads `path` locally (in this injector's own process) purely to resolve
/// `proc_name`'s offset from the DLL's base — a DLL's internal layout
/// (RVA offsets) is identical across processes loading the same file, so
/// `local_addr - local_base` applied to the target's own load base gives
/// the correct address in the target, without needing the DLL to already
/// be loaded remotely to inspect it.
fn resolve_offset(path: &Path, proc_name: &str) -> Result<(usize, isize), String> {
    let wpath = to_wide(path.to_str().ok_or("dll path is not valid UTF-8")?);
    let local = unsafe { LoadLibraryW(wpath.as_ptr()) };
    if local.is_null() {
        return Err(format!("failed to locally load {path:?} to resolve exports"));
    }
    let cname = format!("{proc_name}\0");
    let proc = unsafe { GetProcAddress(local, cname.as_ptr()) };
    let Some(proc) = proc else {
        unsafe { FreeLibrary(local) };
        return Err(format!("export {proc_name} not found in {path:?}"));
    };
    let offset = proc as usize as isize - local as usize as isize;
    unsafe { FreeLibrary(local) };
    Ok((local as usize, offset))
}

fn load_library_w_addr() -> *const c_void {
    let kernel32 = to_wide("kernel32.dll");
    let h = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
    let proc = unsafe { GetProcAddress(h, b"LoadLibraryW\0".as_ptr()) };
    proc.expect("LoadLibraryW must be resolvable in kernel32.dll") as *const c_void
}

fn attach(pid: u32) {
    // Hard exclusion (2026-07-22): refuse before requesting any
    // injection-capable rights on the target — see safety.rs's module doc
    // comment for the three signals checked and why. This is unconditional;
    // there is deliberately no override flag. (Each check's own message
    // already names the pid and says "refusing" — nothing to add here.)
    if let Err(reason) = safety::check(pid) {
        fail(reason);
    }

    let dll = dll_path();
    if !dll.exists() {
        fail(format!("{DLL_NAME} not found at {dll:?} — expected next to heaplens-injector.exe"));
    }
    let remote = RemoteProcess::open(pid);

    if remote.find_module_base().is_some() {
        // Already loaded in this target — HeapLensHookAttach is itself
        // idempotent, so just call it again rather than re-injecting. Uses
        // the `Remote` entry point (see its doc comment in heaplens-hook):
        // hooks may already be active from a prior successful attach, so
        // this raw thread must never return normally.
        println!("heaplens-injector: {DLL_NAME} already loaded in process {pid}, re-invoking attach");
        call_exported_fn(&remote, &dll, "HeapLensHookAttachRemote");
        return;
    }

    // Write the DLL path (wide, null-terminated) into the target so
    // LoadLibraryW can read it there.
    let wpath = to_wide(dll.to_str().unwrap_or_else(|| fail("dll path is not valid UTF-8")));
    let byte_len = wpath.len() * 2;
    let remote_buf = unsafe {
        VirtualAllocEx(remote.handle, std::ptr::null(), byte_len, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE)
    };
    if remote_buf.is_null() {
        fail(format!("VirtualAllocEx failed in process {pid}"));
    }
    let mut written = 0usize;
    let ok = unsafe {
        WriteProcessMemory(remote.handle, remote_buf, wpath.as_ptr() as *const c_void, byte_len, &mut written)
    };
    if ok == 0 || written != byte_len {
        unsafe { VirtualFreeEx(remote.handle, remote_buf, 0, MEM_RELEASE) };
        fail(format!("WriteProcessMemory failed writing the DLL path into process {pid}"));
    }

    let load_library_w = load_library_w_addr();
    match remote.run_remote_thread(load_library_w, remote_buf) {
        Ok(_) => {}
        Err(e) => {
            unsafe { VirtualFreeEx(remote.handle, remote_buf, 0, MEM_RELEASE) };
            fail(e);
        }
    }
    unsafe { VirtualFreeEx(remote.handle, remote_buf, 0, MEM_RELEASE) };

    if remote.find_module_base().is_none() {
        fail(format!("LoadLibraryW ran in process {pid} but {DLL_NAME} is not present in its module list — load failed"));
    }

    // `Remote`, not the plain `HeapLensHookAttach` — this call runs on a
    // fresh `CreateRemoteThread` thread that must never return normally
    // once hooks go live partway through it (see heaplens-hook's doc
    // comment on `HeapLensHookAttachRemote`). Self-load's harness is the
    // only caller of the plain, direct-return `HeapLensHookAttach`.
    call_exported_fn(&remote, &dll, "HeapLensHookAttachRemote");
}

fn call_exported_fn(remote: &RemoteProcess, dll: &Path, export: &str) {
    let target_base = remote.find_module_base().unwrap_or_else(|| {
        fail(format!("{DLL_NAME} not found in process {}'s module list", remote.pid))
    });
    let (local_base, offset) = resolve_offset(dll, export).unwrap_or_else(|e| fail(e));
    let target_addr = (target_base as isize + offset) as *const c_void;
    let _ = local_base;

    match remote.run_remote_thread(target_addr, std::ptr::null_mut()) {
        Ok(rc) => report_hook_rc(export, rc, remote.pid),
        Err(e) => fail(e),
    }
}

fn report_hook_rc(export: &str, rc: u32, pid: u32) {
    if rc == 0 {
        println!("heaplens-injector: {export} succeeded in process {pid}");
        return;
    }
    let detail = match (export, rc) {
        ("HeapLensHookAttachRemote", 1) => "private heap creation (HeapCreate) failed",
        ("HeapLensHookAttachRemote", 2) => "MinHook::create_hook_api failed for RtlAllocateHeap",
        ("HeapLensHookAttachRemote", 3) => "MinHook::create_hook_api failed for RtlReAllocateHeap",
        ("HeapLensHookAttachRemote", 4) => "MinHook::create_hook_api failed for RtlFreeHeap",
        ("HeapLensHookAttachRemote", 5) => "MinHook::enable_all_hooks failed",
        ("HeapLensHookAttachRemote", 7) => "trampoline canary check failed after enabling hooks — \
             hooks were disabled and torn down automatically; the hook was not left resident",
        ("HeapLensHookDetach", 6) => "writer thread did not stop within the detach timeout — \
             hooks were disabled but trampolines/private heap were left in place rather than \
             risk freeing memory a still-running thread depends on; detach did not fully complete",
        _ => "unknown failure code",
    };
    fail(format!("{export} in process {pid} returned failure code {rc}: {detail}"));
}

/// Reads a 4-byte value (matches `AtomicU32`/`AtomicI32`'s layout) from
/// `addr` in the target process.
fn read_remote_i32(remote: &RemoteProcess, addr: *const c_void) -> Result<i32, String> {
    let mut buf = [0u8; 4];
    let mut read = 0usize;
    let ok = unsafe {
        ReadProcessMemory(remote.handle, addr, buf.as_mut_ptr() as *mut c_void, buf.len(), &mut read)
    };
    if ok == 0 || read != buf.len() {
        return Err(format!("ReadProcessMemory failed reading process {}", remote.pid));
    }
    Ok(i32::from_ne_bytes(buf))
}

/// Detach cannot safely run `HeapLensHookDetach` via a second
/// `CreateRemoteThread` call while hooks are active — see the doc comment
/// on `heaplens_hook::WORKER_TID` for why (creating that thread at all,
/// before any of its code runs, triggers a `DLL_THREAD_ATTACH`-driven crash
/// on this build). Instead this reuses `attach_impl`'s own worker thread —
/// already alive, already a normal, safe Rust thread — by queuing an APC
/// onto it directly via its OS thread ID (published at `WORKER_TID`), and
/// polls `DETACH_RESULT` (via `ReadProcessMemory`, since a queued APC has no
/// return channel back to the process that queued it) until the worker
/// publishes an outcome.
fn detach(pid: u32) {
    let dll = dll_path();
    if !dll.exists() {
        fail(format!("{DLL_NAME} not found at {dll:?} — expected next to heaplens-injector.exe"));
    }
    let remote = RemoteProcess::open(pid);

    let Some(target_base) = remote.find_module_base() else {
        println!("heaplens-injector: {DLL_NAME} not loaded in process {pid} — nothing to detach");
        return;
    };

    let (_, tid_offset) = resolve_offset(&dll, "WORKER_TID").unwrap_or_else(|e| fail(e));
    let tid_addr = (target_base as isize + tid_offset) as *const c_void;
    let worker_tid = read_remote_i32(&remote, tid_addr).unwrap_or_else(|e| fail(e)) as u32;
    if worker_tid == 0 {
        fail(format!(
            "process {pid} has {DLL_NAME} loaded but no attach worker thread is recorded — \
             was HeapLensHookAttach ever called successfully in this process?"
        ));
    }

    let (_, apc_offset) = resolve_offset(&dll, "HeapLensHookDetachApc").unwrap_or_else(|e| fail(e));
    let apc_addr = (target_base as isize + apc_offset) as usize;
    let apc_fn: unsafe extern "system" fn(usize) = unsafe { std::mem::transmute(apc_addr) };

    let (_, result_offset) = resolve_offset(&dll, "DETACH_RESULT").unwrap_or_else(|e| fail(e));
    let result_addr = (target_base as isize + result_offset) as *mut c_void;

    // Reset the sentinel before queuing so a stale value from a prior
    // attach/detach cycle in this same process can't be mistaken for this
    // call's result.
    let pending: i32 = -1;
    let mut written = 0usize;
    let ok = unsafe {
        WriteProcessMemory(remote.handle, result_addr, &pending as *const i32 as *const c_void, 4, &mut written)
    };
    if ok == 0 || written != 4 {
        fail(format!("failed to reset detach-result sentinel in process {pid}"));
    }

    let worker_thread = unsafe { OpenThread(THREAD_SET_CONTEXT, 0, worker_tid) };
    if worker_thread.is_null() {
        fail(format!("OpenThread failed for worker thread {worker_tid} in process {pid}"));
    }
    let queued = unsafe { QueueUserAPC(Some(apc_fn), worker_thread, 0) };
    unsafe { CloseHandle(worker_thread) };
    if queued == 0 {
        fail(format!("QueueUserAPC failed for process {pid}'s attach worker thread"));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let rc = loop {
        let v = read_remote_i32(&remote, result_addr).unwrap_or_else(|e| fail(e));
        if v != -1 {
            break v as u32;
        }
        if std::time::Instant::now() >= deadline {
            fail(format!("detach in process {pid} did not complete within 5s"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    if rc != 0 {
        report_hook_rc("HeapLensHookDetach", rc, pid);
        return;
    }
    println!("heaplens-injector: HeapLensHookDetach succeeded in process {pid}");

    // Unload the DLL from the target now that it has cleanly detached.
    let kernel32 = to_wide("kernel32.dll");
    let h = unsafe { GetModuleHandleW(kernel32.as_ptr()) };
    let free_library = unsafe { GetProcAddress(h, b"FreeLibrary\0".as_ptr()) }
        .unwrap_or_else(|| fail("FreeLibrary must be resolvable in kernel32.dll"));
    match remote.run_remote_thread(free_library as *const c_void, target_base as *mut c_void) {
        Ok(rc) if rc != 0 => println!("heaplens-injector: {DLL_NAME} unloaded from process {pid}"),
        Ok(_) => fail(format!("FreeLibrary returned failure unloading {DLL_NAME} from process {pid}")),
        Err(e) => fail(e),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: heaplens-injector <pid> --attach|--detach");
        std::process::exit(2);
    }
    let pid: u32 = args[1].parse().unwrap_or_else(|_| fail(format!("invalid pid: {}", args[1])));
    match args[2].as_str() {
        "--attach" => attach(pid),
        "--detach" => detach(pid),
        other => fail(format!("unknown mode {other:?}; expected --attach or --detach")),
    }
}
