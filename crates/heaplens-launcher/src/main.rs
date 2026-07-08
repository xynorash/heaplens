//! Single entry point for the packaged HeapLens delivery folder.
//!
//! Spawns the daemon as a child process, waits for its WebSocket port to
//! start accepting connections, then launches the Flutter runner and blocks
//! until it exits. The daemon is assigned to a Windows Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so it is torn down automatically the
//! moment this launcher process ends for *any* reason (clean exit, crash, or
//! being killed directly) — not only on the happy path where this process
//! gets to run its own cleanup code.
#![windows_subsystem = "windows"]

use std::fs::File;
use std::io::Write;
use std::net::TcpStream;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const DAEMON_EXE: &str = "heaplens-daemon.exe";
const FLUTTER_EXE: &str = "heaplens_flutter.exe";
const WS_ADDR: &str = "127.0.0.1:9999";
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

fn log(file: &mut File, msg: &str) {
    let _ = writeln!(file, "{msg}");
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .expect("cannot determine launcher's own exe path")
        .parent()
        .expect("launcher exe has no parent directory")
        .to_path_buf()
}

/// Creates a Job Object that kills every process assigned to it as soon as
/// its last handle closes (including implicitly, when this process exits or
/// is terminated). Returns the raw job handle.
fn create_kill_on_close_job() -> HANDLE {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(!job.is_null(), "CreateJobObjectW failed");

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        assert!(ok != 0, "SetInformationJobObject failed");
        job
    }
}

fn assign_to_job(job: HANDLE, child: &Child) {
    unsafe {
        let handle = child.as_raw_handle() as HANDLE;
        let ok = AssignProcessToJobObject(job, handle);
        assert!(ok != 0, "AssignProcessToJobObject failed");
    }
}

fn wait_for_port(addr: &str, timeout: Duration, daemon: &mut Child, log_file: &mut File) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = daemon.try_wait() {
            log(log_file, &format!("daemon exited early with {status} before port came up"));
            return false;
        }
        if TcpStream::connect(addr).is_ok() {
            return true;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    false
}

fn open_log(dir: &Path, name: &str) -> File {
    File::create(dir.join(name)).expect("cannot create log file next to launcher")
}

fn main() {
    let dir = exe_dir();
    let mut log_file = open_log(&dir, "launcher.log");

    let daemon_path = dir.join(DAEMON_EXE);
    let flutter_path = dir.join(FLUTTER_EXE);
    assert!(daemon_path.exists(), "missing {DAEMON_EXE} next to launcher");
    assert!(flutter_path.exists(), "missing {FLUTTER_EXE} next to launcher");

    let job = create_kill_on_close_job();

    log(&mut log_file, "starting daemon...");
    let daemon_log = open_log(&dir, "daemon.log");
    let mut daemon = Command::new(&daemon_path)
        .current_dir(&dir)
        .stdout(Stdio::from(daemon_log.try_clone().unwrap()))
        .stderr(Stdio::from(daemon_log))
        .spawn()
        .expect("failed to spawn heaplens-daemon.exe");
    assign_to_job(job, &daemon);

    log(&mut log_file, "waiting for daemon WS port to accept connections...");
    if !wait_for_port(WS_ADDR, READY_TIMEOUT, &mut daemon, &mut log_file) {
        log(&mut log_file, "daemon did not become ready in time; killing it and exiting");
        let _ = daemon.kill();
        std::process::exit(1);
    }
    log(&mut log_file, "daemon is ready; launching Flutter app...");

    let mut app = Command::new(&flutter_path)
        .current_dir(&dir)
        .spawn()
        .expect("failed to spawn heaplens_flutter.exe");
    assign_to_job(job, &app);

    // Block until the user closes the app.
    let status = app.wait().expect("failed to wait on heaplens_flutter.exe");
    log(&mut log_file, &format!("Flutter app exited with {status}; stopping daemon"));

    // Explicit teardown on the happy path, in addition to the Job Object's
    // kill-on-close guarantee (which also fires if this process is killed
    // abruptly instead of reaching this line).
    let _ = daemon.kill();
    let _ = daemon.wait();
    log(&mut log_file, "daemon stopped; launcher exiting");
}
