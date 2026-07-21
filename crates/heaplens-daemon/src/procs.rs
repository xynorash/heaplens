//! Stage 7 Step 3 (docs/stage7-injection-design.md §3): process enumeration
//! for the attach-target picker. Windows-only, same toolhelp-snapshot
//! technique `heaplens-injector` already uses to find a loaded module.

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::SystemInformation::{IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_UNKNOWN};
use windows_sys::Win32::System::Threading::{IsWow64Process2, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use heaplens_protocol::ProcessInfo;

/// Best-effort architecture probe. `OpenProcess` legitimately fails for
/// protected/elevated/system processes the daemon has no rights to query —
/// that is reported as `"unknown"`, not treated as an enumeration failure
/// (dropping the process from the list would be worse: the user could never
/// see, let alone attach to, anything they don't already have rights to).
fn probe_arch(pid: u32) -> String {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return "unknown".to_owned();
    }
    let mut process_machine = 0u16;
    let mut native_machine = 0u16;
    let ok = unsafe { IsWow64Process2(handle, &mut process_machine, &mut native_machine) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return "unknown".to_owned();
    }
    if process_machine != IMAGE_FILE_MACHINE_UNKNOWN {
        // Running under WOW64 emulation — a 32-bit process on this 64-bit system.
        "x86".to_owned()
    } else if native_machine == IMAGE_FILE_MACHINE_AMD64 {
        "x64".to_owned()
    } else {
        "unknown".to_owned()
    }
}

/// Lists every running process visible to a `CreateToolhelp32Snapshot`
/// snapshot, with a best-effort architecture probe per process. Never
/// panics on a single process's probe failure — an inaccessible or
/// already-exited process is reported with `arch: "unknown"`, not dropped
/// or fatal to the whole call.
pub fn list_processes() -> Vec<ProcessInfo> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() {
        return Vec::new();
    }

    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

    let mut out = Vec::new();
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok != 0 {
        let name_len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
        let pid = entry.th32ProcessID;
        if pid != 0 {
            out.push(ProcessInfo { pid, name, arch: probe_arch(pid) });
        }
        ok = unsafe { Process32NextW(snap, &mut entry) };
    }
    unsafe { CloseHandle(snap) };
    out
}
