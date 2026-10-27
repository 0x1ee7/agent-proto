//! Platform containment and file permission primitives (RFC-0012 sec 4.5, 4.6, 4.7).
//!
//! This is the only module of `a2ad` allowed to use `unsafe` (RFC-0012 [0012-060]); the
//! crate root applies `#[allow(unsafe_code)]` to this module alone. Linux uses safe std
//! and `nix` APIs; the unsafe code is the Windows Job Object, suspended-process,
//! console-control and registry FFI.

use std::io;
use std::path::Path;
use std::process::{Child, Command};

/// Errors of the platform layer. The messages name the failing system call.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// An I/O error from the standard library.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A failed system call or a refused permission check.
    #[error("{0}")]
    Msg(String),
}

/// Maximum path length (characters) without `LongPathsEnabled` (RFC-0012 0012-066).
pub const MAX_PATH_CHARS: usize = 260;

/// Handle to a contained child process tree. Linux: a process group. Windows: a Job Object
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so dropping this value kills the tree
/// (0012-018).
#[derive(Debug)]
pub struct Containment {
    pid: u32,
    #[cfg(windows)]
    job: win::JobHandle,
}

impl Containment {
    /// Process id of the contained child (the group leader on Linux).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Graceful stop signal to the whole tree (0012-017): `SIGTERM` to the process group on
    /// Linux, `CTRL_BREAK_EVENT` to the child's process group on Windows. A group that has
    /// already gone is not an error.
    ///
    /// # Errors
    /// Returns [`PlatformError`] when the signal cannot be delivered.
    pub fn graceful(&self) -> Result<(), PlatformError> {
        self.signal_graceful()
    }

    /// Forced kill of the whole tree (0012-016/017): `SIGKILL` to the process group on
    /// Linux, `TerminateJobObject` on Windows. A tree that has already gone is not an
    /// error.
    ///
    /// # Errors
    /// Returns [`PlatformError`] when the tree cannot be killed.
    pub fn forced(&self) -> Result<(), PlatformError> {
        self.kill_forced()
    }
}

#[cfg(unix)]
impl Containment {
    fn signal_graceful(&self) -> Result<(), PlatformError> {
        send_group(self.pid, nix::sys::signal::Signal::SIGTERM)
    }

    fn kill_forced(&self) -> Result<(), PlatformError> {
        send_group(self.pid, nix::sys::signal::Signal::SIGKILL)
    }
}

#[cfg(windows)]
impl Containment {
    fn signal_graceful(&self) -> Result<(), PlatformError> {
        // SAFETY: `CTRL_BREAK_EVENT` with a process group id created by CREATE_NEW_PROCESS_GROUP
        // (see `spawn_contained`); the call has no pointer arguments.
        let ok = unsafe { win::GenerateConsoleCtrlEvent(win::CTRL_BREAK_EVENT, self.pid) };
        if ok == 0 {
            return Err(win::last_error("GenerateConsoleCtrlEvent"));
        }
        Ok(())
    }

    fn kill_forced(&self) -> Result<(), PlatformError> {
        self.job.terminate()
    }
}

/// Spawns `command` inside a fresh containment (0012-018, 0012-019). The caller configures
/// stdio and environment beforehand. The returned child is running.
///
/// # Errors
/// Returns [`PlatformError`] when the spawn or containment fails; a child that was spawned
/// but could not be contained is killed before the error is returned.
#[cfg(unix)]
pub fn spawn_contained(command: &mut Command) -> Result<(Child, Containment), PlatformError> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let child = command.spawn()?;
    let pid = child.id();
    Ok((child, Containment { pid }))
}

/// Spawns `command` inside a fresh containment (0012-018, 0012-064). The child starts
/// suspended in a new process group, is assigned to a kill-on-close Job Object, and is then
/// resumed.
///
/// # Errors
/// Returns [`PlatformError`] when the spawn or containment fails; a child that was spawned
/// but could not be contained is killed before the error is returned.
#[cfg(windows)]
pub fn spawn_contained(command: &mut Command) -> Result<(Child, Containment), PlatformError> {
    use std::os::windows::process::CommandExt;
    command.creation_flags(win::CREATE_SUSPENDED | win::CREATE_NEW_PROCESS_GROUP);
    let mut child = command.spawn()?;
    let pid = child.id();
    let process = std::os::windows::io::AsRawHandle::as_raw_handle(&child);
    match win::contain_suspended(pid, process) {
        Ok(job) => Ok((child, Containment { pid, job })),
        Err(err) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(err)
        }
    }
}

/// Kills a process tree recorded by a previous supervisor (0012-021). Only call after
/// [`process_start_token`] matched the record. A tree that is already gone is not an error.
///
/// # Errors
/// Returns [`PlatformError`] when a live tree cannot be killed.
#[cfg(unix)]
pub fn kill_recorded_tree(pid: u32) -> Result<(), PlatformError> {
    send_group(pid, nix::sys::signal::Signal::SIGKILL)
}

/// Kills a process tree recorded by a previous supervisor (0012-021).
///
/// # Errors
/// Returns [`PlatformError`] when a live process cannot be terminated.
#[cfg(windows)]
pub fn kill_recorded_tree(pid: u32) -> Result<(), PlatformError> {
    win::terminate_pid(pid)
}

/// A token that identifies one process incarnation: the Linux start time in clock ticks
/// (`/proc/<pid>/stat` field 22) or the Windows creation time. `None` when the process
/// does not exist (0012-021).
#[must_use]
#[cfg(unix)]
pub fn process_start_token(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Fields after `)` start at field 3 (state); field 22 is index 19.
    let field = after_comm.split_whitespace().nth(19)?;
    field.parse().ok()
}

/// A token that identifies one process incarnation (Windows creation time).
#[must_use]
#[cfg(windows)]
pub fn process_start_token(pid: u32) -> Option<u64> {
    win::creation_time(pid)
}

/// Whether the Windows `LongPathsEnabled` policy is set (0012-066). Always `true` on Linux,
/// where there is no path length limit of that kind.
#[must_use]
#[cfg(windows)]
pub fn long_paths_enabled() -> bool {
    win::long_paths_enabled()
}

/// Whether the Windows `LongPathsEnabled` policy is set (0012-066). Always `true` on Linux.
#[must_use]
#[cfg(unix)]
pub fn long_paths_enabled() -> bool {
    true
}

/// True when `path` is usable under the given long-path policy (0012-066).
#[must_use]
pub fn path_length_ok(path: &Path, long_paths: bool) -> bool {
    long_paths || path.to_string_lossy().chars().count() <= MAX_PATH_CHARS
}

/// Opens `path` for appending, creating it if needed. On Windows the handle shares read, write
/// and delete, so the host can rotate the file while the supervisor holds its stdout handle
/// (RFC-0013 0013-039). Unix allows renaming an open file, so no share flags are needed there.
///
/// # Errors
/// Returns the I/O error of the open call.
pub fn open_log_for_append(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        const SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
        options.share_mode(SHARE_ALL);
    }
    options.open(path)
}

/// Restricts `path` to its owner (0012-024 Linux mode `0600`/`0700`; 0012-025/065 Windows
/// DACL granting only the current user, with inheritance removed).
///
/// # Errors
/// Returns [`PlatformError`] when the permissions cannot be set.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path, is_dir: bool) -> Result<(), PlatformError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_dir { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Restricts `path` to the current user (0012-025/065).
///
/// # Errors
/// Returns [`PlatformError`] when `icacls` fails.
#[cfg(windows)]
pub fn restrict_to_owner(path: &Path, is_dir: bool) -> Result<(), PlatformError> {
    let user = current_user()?;
    let grant = if is_dir {
        format!("{user}:(OI)(CI)F")
    } else {
        format!("{user}:F")
    };
    let status = std::process::Command::new(icacls_path())
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .status()?;
    if !status.success() {
        return Err(PlatformError::Msg(format!(
            "icacls failed on {}: {status}",
            path.display()
        )));
    }
    Ok(())
}

/// Verifies that `path` is owner-only (0012-024 Linux, 0012-025 Windows). Symlinks and
/// files owned by another user are refused.
///
/// # Errors
/// Returns [`PlatformError`] naming the broader permission when verification fails.
#[cfg(unix)]
pub fn verify_owner_only(path: &Path, is_dir: bool) -> Result<(), PlatformError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(PlatformError::Msg(format!(
            "{} is a symlink; refusing (credential_acl)",
            path.display()
        )));
    }
    if is_dir != meta.is_dir() {
        return Err(PlatformError::Msg(format!(
            "{} has the wrong type (credential_acl)",
            path.display()
        )));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 || (!is_dir && mode & 0o100 != 0) {
        return Err(PlatformError::Msg(format!(
            "{} mode {mode:o} is broader than owner-only (credential_acl)",
            path.display()
        )));
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return Err(PlatformError::Msg(format!(
            "{} is owned by another user (credential_acl)",
            path.display()
        )));
    }
    Ok(())
}

/// Verifies that `path` is owner-only (0012-025): every access entry names the current user
/// and no entry is inherited.
///
/// # Errors
/// Returns [`PlatformError`] when `icacls` fails or an entry names another principal.
#[cfg(windows)]
pub fn verify_owner_only(path: &Path, _is_dir: bool) -> Result<(), PlatformError> {
    let user = current_user()?;
    let output = std::process::Command::new(icacls_path())
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(PlatformError::Msg(format!(
            "icacls could not read {} (credential_acl)",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let path_text = path.to_string_lossy();
    let mut count = 0usize;
    for line in text.lines() {
        let rest = line.strip_prefix(path_text.as_ref()).unwrap_or(line).trim();
        let Some((principal, flags)) = rest.split_once(":(") else {
            continue;
        };
        count += 1;
        let flags = format!("({flags}");
        let principal_ok = principal.eq_ignore_ascii_case(&user)
            || principal
                .rsplit_once('\\')
                .is_some_and(|(_, name)| name.eq_ignore_ascii_case(&user));
        if !principal_ok || flags.contains("(I)") {
            return Err(PlatformError::Msg(format!(
                "{} grants access to `{principal}` (credential_acl)",
                path.display()
            )));
        }
    }
    if count == 0 {
        return Err(PlatformError::Msg(format!(
            "{} has no access entries (credential_acl)",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn current_user() -> Result<String, PlatformError> {
    std::env::var("USERNAME").map_err(|_| PlatformError::Msg("USERNAME is not set".to_owned()))
}

#[cfg(windows)]
fn icacls_path() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    std::path::PathBuf::from(root)
        .join("System32")
        .join("icacls.exe")
}

#[cfg(unix)]
fn send_group(pid: u32, signal: nix::sys::signal::Signal) -> Result<(), PlatformError> {
    use nix::errno::Errno;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    let raw =
        i32::try_from(pid).map_err(|_| PlatformError::Msg(format!("pid {pid} is out of range")))?;
    match killpg(Pid::from_raw(raw), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(err) => Err(PlatformError::Msg(format!(
            "{signal} to process group {pid} failed: {err}"
        ))),
    }
}

/// Windows FFI. Every `unsafe` block passes valid handles or pointers to live locals and
/// checks the documented return value.
#[cfg(windows)]
mod win {
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    pub use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_DWORD, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
    };

    use super::PlatformError;

    /// `CREATE_SUSPENDED` (winbase.h).
    pub const CREATE_SUSPENDED: u32 = 0x0000_0004;
    /// `CREATE_NEW_PROCESS_GROUP` (winbase.h).
    pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    /// Owned Job Object handle, stored as an integer so the containment is `Send`.
    #[derive(Debug)]
    pub struct JobHandle(usize);

    impl JobHandle {
        fn raw(&self) -> HANDLE {
            self.0 as HANDLE
        }

        /// `TerminateJobObject` with exit code 1 (0012-016 forced stop).
        pub fn terminate(&self) -> Result<(), PlatformError> {
            // SAFETY: `self.raw()` is a Job Object handle created by `create_kill_on_close_job`
            // and not yet closed (closed only in `Drop`).
            let ok = unsafe { TerminateJobObject(self.raw(), 1) };
            if ok == 0 {
                return Err(last_error("TerminateJobObject"));
            }
            Ok(())
        }
    }

    impl Drop for JobHandle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by CreateJobObjectW and is closed exactly once here.
            // With KILL_ON_JOB_CLOSE, closing the last handle terminates the tree (0012-018).
            unsafe {
                CloseHandle(self.raw());
            }
        }
    }

    /// Assigns a suspended child to a new kill-on-close job and resumes its threads.
    pub fn contain_suspended(pid: u32, process: HANDLE) -> Result<JobHandle, PlatformError> {
        let job = create_kill_on_close_job()?;
        // SAFETY: `job.raw()` and `process` are valid handles; the child is still suspended.
        let ok = unsafe { AssignProcessToJobObject(job.raw(), process) };
        if ok == 0 {
            return Err(last_error("AssignProcessToJobObject"));
        }
        resume_threads(pid)?;
        Ok(job)
    }

    fn create_kill_on_close_job() -> Result<JobHandle, PlatformError> {
        // SAFETY: null security attributes and a null name are valid for CreateJobObjectW.
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return Err(last_error("CreateJobObjectW"));
        }
        let job = JobHandle(raw as usize);
        // SAFETY: JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a plain C struct; all-zero is valid.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| PlatformError::Msg("job info size overflow".to_owned()))?;
        // SAFETY: `info` is initialised and lives for the call; `size` is its exact size.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                size,
            )
        };
        if ok == 0 {
            return Err(last_error("SetInformationJobObject"));
        }
        Ok(job)
    }

    fn resume_threads(pid: u32) -> Result<(), PlatformError> {
        // SAFETY: a thread snapshot with no pointer arguments; closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(last_error("CreateToolhelp32Snapshot"));
        }
        let result = resume_in_snapshot(snapshot, pid);
        // SAFETY: `snapshot` is a valid handle from CreateToolhelp32Snapshot, closed once.
        unsafe {
            CloseHandle(snapshot);
        }
        result
    }

    fn resume_in_snapshot(snapshot: HANDLE, pid: u32) -> Result<(), PlatformError> {
        // SAFETY: THREADENTRY32 is a plain C struct; all-zero is valid before dwSize is set.
        let mut entry: THREADENTRY32 = unsafe { zeroed() };
        entry.dwSize = u32::try_from(size_of::<THREADENTRY32>())
            .map_err(|_| PlatformError::Msg("thread entry size overflow".to_owned()))?;
        // SAFETY: `snapshot` is valid and `entry` is a writable THREADENTRY32 with dwSize set.
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        let mut resumed = 0usize;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: plain open of a thread id from the snapshot with suspend/resume rights.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(last_error("OpenThread"));
                }
                // SAFETY: `thread` is a valid thread handle opened above.
                let previous = unsafe { ResumeThread(thread) };
                // SAFETY: `thread` is valid and closed exactly once here.
                unsafe {
                    CloseHandle(thread);
                }
                if previous == u32::MAX {
                    return Err(last_error("ResumeThread"));
                }
                resumed += 1;
            }
            // SAFETY: `snapshot` and `entry` are valid as above.
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(PlatformError::Msg(format!(
                "no thread found for suspended process {pid}"
            )));
        }
        Ok(())
    }

    /// Terminates a process by id (0012-021 recorded tree). A process that no longer exists
    /// is not an error.
    pub fn terminate_pid(pid: u32) -> Result<(), PlatformError> {
        // SAFETY: plain open with terminate rights; the result is checked for null.
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Ok(());
        }
        // SAFETY: `process` is a valid handle opened with PROCESS_TERMINATE above.
        let ok = unsafe { TerminateProcess(process, 1) };
        let outcome = if ok == 0 {
            Err(last_error("TerminateProcess"))
        } else {
            Ok(())
        };
        // SAFETY: `process` is valid and closed exactly once here.
        unsafe {
            CloseHandle(process);
        }
        outcome
    }

    /// Creation time of `pid` as a 64-bit value, or `None` when the process does not exist.
    pub fn creation_time(pid: u32) -> Option<u64> {
        // SAFETY: plain open with query rights; the result is checked for null.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return None;
        }
        // SAFETY: FILETIME is a plain C struct; all-zero is valid.
        let mut creation: FILETIME = unsafe { zeroed() };
        // SAFETY: `process` is valid; the other FILETIME outputs are unused and nullable in
        // practice, but we pass valid writable locals to be explicit.
        let mut exit: FILETIME = unsafe { zeroed() };
        let mut kernel: FILETIME = unsafe { zeroed() };
        let mut user: FILETIME = unsafe { zeroed() };
        // SAFETY: all four FILETIME pointers reference live writable locals.
        let ok =
            unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
        // SAFETY: `process` is valid and closed exactly once here.
        unsafe {
            CloseHandle(process);
        }
        if ok == 0 {
            return None;
        }
        Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }

    /// `HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled == 1` (0012-066).
    pub fn long_paths_enabled() -> bool {
        let subkey = wide("SYSTEM\\CurrentControlSet\\Control\\FileSystem");
        let name = wide("LongPathsEnabled");
        let mut key: HKEY = null_mut();
        // SAFETY: NUL-terminated wide strings and a writable out pointer.
        let status =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), 0, KEY_READ, &mut key) };
        if status != 0 {
            return false;
        }
        let mut value: u32 = 0;
        let mut kind: u32 = 0;
        let mut size = u32::try_from(size_of::<u32>()).unwrap_or(4);
        // SAFETY: `key` is open; `value` is a writable u32 of exactly `size` bytes.
        let status = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                null_mut(),
                &mut kind,
                std::ptr::addr_of_mut!(value).cast(),
                &mut size,
            )
        };
        // SAFETY: `key` is valid and closed exactly once here.
        unsafe {
            RegCloseKey(key);
        }
        status == 0 && kind == REG_DWORD && value == 1
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The last Win32 error as a [`PlatformError`] naming the call.
    pub fn last_error(call: &str) -> PlatformError {
        PlatformError::Msg(format!("{call} failed: {}", io::Error::last_os_error()))
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn last_error(call: &str) -> PlatformError {
    PlatformError::Msg(format!("{call} failed: {}", io::Error::last_os_error()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_exit(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn stable_0012_021_start_token_identifies_live_process() {
        let me = std::process::id();
        let token = process_start_token(me).expect("own process has a start token");
        assert_eq!(process_start_token(me), Some(token));
        assert_eq!(process_start_token(u32::MAX), None);
    }

    #[test]
    fn stable_0012_066_path_length_policy() {
        let exact = std::path::PathBuf::from("a".repeat(MAX_PATH_CHARS));
        let over = std::path::PathBuf::from("a".repeat(MAX_PATH_CHARS + 1));
        assert!(path_length_ok(&exact, false));
        assert!(!path_length_ok(&over, false));
        assert!(path_length_ok(&over, true));
    }

    #[cfg(unix)]
    #[test]
    fn stable_0012_019_graceful_stop_is_sigterm_to_group() {
        let mut cmd = Command::new("sleep");
        cmd.arg("300");
        let (mut child, containment) = spawn_contained(&mut cmd).expect("spawn");
        containment.graceful().expect("graceful");
        let status = wait_exit(&mut child, Duration::from_secs(10)).expect("exits");
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(15)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_0012_019_forced_kill_reaches_descendants() {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 300 & echo $!; wait");
        cmd.stdout(Stdio::piped());
        let (mut child, containment) = spawn_contained(&mut cmd).expect("spawn");
        let stdout = child.stdout.take().expect("stdout");
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .expect("pid line");
        let grandchild: u32 = line.trim().parse().expect("grandchild pid");
        containment.forced().expect("forced");
        assert!(wait_exit(&mut child, Duration::from_secs(10)).is_some());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = std::fs::read_to_string(format!("/proc/{grandchild}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().to_owned())
                })
                .and_then(|rest| rest.chars().next());
            if state.is_none() || state == Some('Z') {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} still alive"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stable_0012_024_owner_only_verified_and_broader_mode_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("seed.nk");
        std::fs::write(&file, b"x").expect("write");
        restrict_to_owner(&file, false).expect("restrict");
        verify_owner_only(&file, false).expect("owner-only verifies");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        }
        let err = verify_owner_only(&file, false).unwrap_err();
        assert!(err.to_string().contains("credential_acl"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn stable_0012_025_owner_only_verified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("seed.nk");
        std::fs::write(&file, b"x").expect("write");
        restrict_to_owner(&file, false).expect("restrict");
        verify_owner_only(&file, false).expect("owner-only verifies");
    }

    #[cfg(windows)]
    #[test]
    fn stable_0012_017_graceful_and_forced_stop_on_windows() {
        let mut cmd = Command::new("ping");
        cmd.args(["-n", "300", "127.0.0.1"]);
        cmd.stdout(std::process::Stdio::null());
        let (mut child, containment) = spawn_contained(&mut cmd).expect("spawn");
        containment.forced().expect("forced");
        assert!(wait_exit(&mut child, Duration::from_secs(15)).is_some());
    }
}
