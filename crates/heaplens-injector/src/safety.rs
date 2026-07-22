//! Pre-attach exclusion check (2026-07-22): refuses to inject into any
//! process that has a kernel-mode driver component, or belongs to a
//! category of software known to (VPN/network-filter clients, anti-cheat,
//! security/AV products, virtualization hosts) — the same hard exclusion
//! this project's own testing has followed all along, now enforced by the
//! tool itself instead of relying on the operator to remember it every time.
//!
//! Three independent, read-only signals, checked in order from most general
//! to most specific. Any one of them matching is a refusal — this is a
//! union of "reasons to say no," not a vote.
//!
//! # Design constraint: no handle upgrade before the verdict
//! Every check here uses `PROCESS_QUERY_LIMITED_INFORMATION` at most (or no
//! handle to the target at all, for the Toolhelp32-based checks) — strictly
//! less than the injection-capable rights `RemoteProcess::open` requests.
//! The point is that this module runs, and can refuse, *before* the
//! injector ever asks Windows for `PROCESS_CREATE_THREAD`/`PROCESS_VM_WRITE`
//! on an excluded process, not just before it uses them.
//!
//! # Signal 1 — process protection level (most general, most reliable)
//! `GetProcessInformation`/`ProcessProtectionLevelInfo` (documented since
//! Windows 8.1) reports whether a process is a Protected Process (PP) or
//! Protected Process Light (PPL). Third-party software has no legitimate
//! reason to run protected *unless* it's exactly this excluded category —
//! anti-malware engines (`PROTECTION_LEVEL_ANTIMALWARE_LIGHT` is the
//! standard AV signature-verification level; real AV engines run at this
//! level specifically so nothing can tamper with them, which is the same
//! property that makes DLL injection into them meaningless to attempt and
//! worth refusing outright) and various system-security components. This
//! check needs no name list at all — it generalizes to vendors never
//! explicitly enumerated below, which the other two signals cannot do.
//!
//! # Signal 2 — loaded module names (vendor SDK fingerprint)
//! Anti-cheat and some VPN clients link a vendor-supplied DLL into the
//! *userspace* process that talks to their kernel driver (the driver itself
//! is never in a process's module list — drivers live in kernel address
//! space, not any process's — so this checks for the userspace half of the
//! pair, not the driver directly). Read via the same
//! `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid)` walk already used
//! elsewhere in this tool to confirm `heaplens_hook.dll` loaded.
//!
//! # Signal 3 — process image name (weakest, but zero-cost and catches
//! what the other two miss)
//! A maintained denylist of known driver-bearing product process names.
//! Trivially spoofable (rename the exe) and incomplete by construction —
//! explicitly the "fallback... weaker but still better than relying on the
//! operator to remember" this task asked for, not the primary defense.
//! Read via `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)`, which needs
//! no handle to the target process at all.

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
    MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessInformation, OpenProcess, ProcessProtectionLevelInfo,
    PROCESS_PROTECTION_LEVEL_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    PROTECTION_LEVEL_NONE,
};

/// Process image names (case-insensitive, no path) known to belong to
/// VPN/network-filter clients, anti-cheat, security/AV products, or
/// virtualization hosts. Incomplete by construction — see the module doc
/// comment's Signal 3 section. Extend this list as specific products are
/// identified; do not remove entries without a specific reason.
const DENYLIST_PROCESS_NAMES: &[&str] = &[
    // VPN / network-filter clients
    "exitlag.exe",
    "nordvpn.exe",
    "nordvpnservice.exe",
    "expressvpn.exe",
    "expressvpnservice.exe",
    "protonvpn.exe",
    "protonvpnservice.exe",
    "openvpn.exe",
    "openvpn-gui.exe",
    "wireguard.exe",
    "pia_manager.exe",
    "privateinternetaccess.exe",
    "surfshark.exe",
    "surfsharkservice.exe",
    "cyberghost.exe",
    "cyberghostservice.exe",
    "windscribe.exe",
    "tunnelbear.exe",
    "zerotier-one.exe",
    "tailscale.exe",
    "tailscale-ipn.exe",
    "mullvad vpn.exe",
    "mullvad-daemon.exe",
    // Anti-cheat
    "easyanticheat.exe",
    "easyanticheat_eos_setup.exe",
    "beservice.exe",
    "bedaisy.exe",
    "battleye.exe",
    "vgc.exe",
    "vgtray.exe",
    "faceit.exe",
    "faceitclient.exe",
    // Security / AV
    "msmpeng.exe",
    "avp.exe",
    "avastui.exe",
    "avastsvc.exe",
    "avgui.exe",
    "avgsvc.exe",
    "mcshield.exe",
    "mcafee.exe",
    "nortonsecurity.exe",
    "bdagent.exe",
    "mbam.exe",
    "mbamservice.exe",
    "sophosui.exe",
    "sophosav.exe",
    "ekrn.exe",
    "wrsa.exe",
    "cylancesvc.exe",
    // Virtualization hosts
    "vmware.exe",
    "vmware-vmx.exe",
    "vmware-authd.exe",
    "vmnat.exe",
    "vboxheadless.exe",
    "virtualbox.exe",
    "vboxsvc.exe",
    "vmms.exe",
    "vmwp.exe",
    "qemu-system-x86_64.exe",
    "qemu-system-x86_64w.exe",
];

/// Loaded-module (DLL) names known to be the userspace half of a
/// driver-backed vendor SDK — see the module doc comment's Signal 2
/// section. Matched by filename only (no path), case-insensitive.
const DENYLIST_MODULE_NAMES: &[&str] = &[
    "easyanticheat_x64.dll",
    "easyanticheat_x86.dll",
    "beclient_x64.dll",
    "beclient_x86.dll",
    "beclient.dll",
];

/// Refuses (with a specific, printable reason) if `pid` matches any
/// exclusion signal. `Ok(())` means none of the three checks fired — it is
/// not a positive guarantee the process has no kernel driver, only that
/// none of these specific, safe, read-only checks found one.
pub fn check(pid: u32) -> Result<(), String> {
    if let Some(reason) = check_protection_level(pid) {
        return Err(reason);
    }
    if let Some(reason) = check_loaded_modules(pid) {
        return Err(reason);
    }
    if let Some(reason) = check_process_name(pid) {
        return Err(reason);
    }
    Ok(())
}

/// Signal 1 — process protection level. Opens the most limited handle that
/// can answer this question (`PROCESS_QUERY_LIMITED_INFORMATION`) and closes
/// it immediately after; never escalates to injection-capable rights here.
fn check_protection_level(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // Can't query — most likely a protected/elevated process this tool
        // has no rights to inspect at all, which is itself exactly the
        // signal this check exists to catch. Fail closed: refuse rather
        // than silently skip the check.
        return Some(format!(
            "cannot query process {pid}'s protection level (access denied) — refusing rather \
             than assuming it is safe; a process this tool cannot even query is treated as \
             excluded"
        ));
    }
    let mut info = PROCESS_PROTECTION_LEVEL_INFORMATION { ProtectionLevel: 0 };
    let ok = unsafe {
        GetProcessInformation(
            handle,
            ProcessProtectionLevelInfo,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<PROCESS_PROTECTION_LEVEL_INFORMATION>() as u32,
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        // Query genuinely failed (not "access denied to open" — this is a
        // failure of the info call itself on an already-open handle).
        // Treat the same as unknown/unsafe rather than assume None.
        return Some(format!(
            "GetProcessInformation(ProcessProtectionLevelInfo) failed for process {pid} — \
             refusing rather than assuming it is unprotected"
        ));
    }
    if info.ProtectionLevel != PROTECTION_LEVEL_NONE {
        return Some(format!(
            "process {pid} is a protected process (protection level {}) — third-party software \
             has no legitimate reason to run protected other than belonging to exactly the \
             excluded category (anti-malware engines, system-security components); refusing",
            info.ProtectionLevel
        ));
    }
    None
}

/// Signal 2 — loaded module names. No handle to the target opened by this
/// function at all; `CreateToolhelp32Snapshot` manages its own access.
fn check_loaded_modules(pid: u32) -> Option<String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) };
    if snap.is_null() {
        return None; // no modules visible (process may have exited, or be inaccessible) — Signal 1/3 cover the rest
    }
    let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
    let mut found = None;
    let mut ok = unsafe { Module32FirstW(snap, &mut entry) };
    while ok != 0 {
        let name_len = entry.szModule.iter().position(|&c| c == 0).unwrap_or(entry.szModule.len());
        let name = String::from_utf16_lossy(&entry.szModule[..name_len]);
        if DENYLIST_MODULE_NAMES.iter().any(|d| name.eq_ignore_ascii_case(d)) {
            found = Some(format!(
                "process {pid} has {name} loaded — a known driver-backed vendor SDK module \
                 (anti-cheat/VPN userspace client); refusing"
            ));
            break;
        }
        ok = unsafe { Module32NextW(snap, &mut entry) };
    }
    unsafe { CloseHandle(snap) };
    found
}

/// Signal 3 — process image name. No handle to the target opened by this
/// function; system-wide process snapshot, filtered by pid.
fn check_process_name(pid: u32) -> Option<String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap.is_null() {
        return None;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
    while ok != 0 {
        if entry.th32ProcessID == pid {
            let name_len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
            if DENYLIST_PROCESS_NAMES.iter().any(|d| name.eq_ignore_ascii_case(d)) {
                found = Some(format!(
                    "process {pid} ({name}) matches the maintained denylist of known \
                     driver-bearing software (VPN/network-filter, anti-cheat, security/AV, \
                     virtualization); refusing"
                ));
            }
            break;
        }
        ok = unsafe { Process32NextW(snap, &mut entry) };
    }
    unsafe { CloseHandle(snap) };
    found
}
