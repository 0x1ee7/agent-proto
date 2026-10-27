//! Agent process spawning and tree termination (RFC-0009 4.9, 0009-059, 0009-060).
//!
//! The actor talks to agents only through [`Spawner`], so tests can substitute
//! in-memory pipes for real processes.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::LaunchSpec;
use crate::redact::redact;

/// Maximum stderr bytes kept for crash detail.
const STDERR_TAIL_BYTES: usize = 2048;
/// Largest stderr read without a newline; bounds the line buffer.
const MAX_LOG_LINE: usize = 4096;
/// Characters of one stderr line that reach the log.
const MAX_LOG_CHARS: usize = 512;

/// Termination handle for one agent process tree.
pub(crate) trait Terminator: Send {
    /// Waits up to `timeout` for the process to exit on its own. Returns true if it did.
    fn wait_exit(&mut self, timeout: Duration) -> bool;
    /// Kills the process and its descendants. Best effort, idempotent.
    fn kill_tree(&mut self);
    /// Exit status of the reaped process, e.g. `exit status: 3`. None while it runs or when unknown.
    fn exit_description(&self) -> Option<String>;
}

/// A freshly spawned agent with its three standard streams.
pub(crate) struct Spawned {
    pub(crate) stdin: Box<dyn Write + Send>,
    pub(crate) stdout: Box<dyn Read + Send>,
    pub(crate) terminator: Box<dyn Terminator>,
    /// Tail of stderr, kept for redacted crash detail.
    pub(crate) stderr_tail: Arc<Mutex<Vec<u8>>>,
}

/// Starts agent processes from a launch spec.
pub(crate) trait Spawner: Send {
    fn spawn(&mut self, spec: &LaunchSpec) -> io::Result<Spawned>;
}

/// Spawns real child processes without a shell (0009-037).
pub(crate) struct OsSpawner;

impl Spawner for OsSpawner {
    fn spawn(&mut self, spec: &LaunchSpec) -> io::Result<Spawned> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn()?;
        let stdin = take(child.stdin.take(), "stdin")?;
        let stdout = take(child.stdout.take(), "stdout")?;
        let stderr = take(child.stderr.take(), "stderr")?;
        let stderr_tail = Arc::new(Mutex::new(Vec::new()));
        drain_stderr(stderr, Arc::clone(&stderr_tail));
        let program = spec
            .program
            .file_name()
            .map_or_else(|| "?".to_owned(), |n| n.to_string_lossy().into_owned());
        tracing::info!(target: "driver_acp::child", program = %program, pid = child.id(), "agent process spawned");
        Ok(Spawned {
            stdin: Box::new(ChildStdinWriter(stdin)),
            stdout: Box::new(ChildStdoutReader(stdout)),
            terminator: Box::new(ChildTerminator {
                child,
                status: None,
            }),
            stderr_tail,
        })
    }
}

fn take<T>(value: Option<T>, name: &str) -> io::Result<T> {
    value.ok_or_else(|| io::Error::other(format!("child {name} not captured")))
}

struct ChildStdinWriter(ChildStdin);

impl Write for ChildStdinWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

struct ChildStdoutReader(ChildStdout);

impl Read for ChildStdoutReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

fn drain_stderr(stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        loop {
            let mut line = Vec::new();
            match (&mut reader)
                .take(MAX_LOG_LINE as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    keep_tail(&tail, &line);
                    log_stderr_line(&line);
                }
            }
        }
    });
}

fn keep_tail(tail: &Mutex<Vec<u8>>, bytes: &[u8]) {
    if let Ok(mut buf) = tail.lock() {
        buf.extend_from_slice(bytes);
        if buf.len() > STDERR_TAIL_BYTES {
            let excess = buf.len() - STDERR_TAIL_BYTES;
            buf.drain(..excess);
        }
    }
}

/// Logs one stderr line redacted; error-like lines at warn, the rest at debug (target `driver_acp::child`).
fn log_stderr_line(line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    let text: String = text.trim_end().chars().take(MAX_LOG_CHARS).collect();
    if text.is_empty() {
        return;
    }
    let shown = redact(&text);
    let lower = shown.to_ascii_lowercase();
    if ["error", "panic", "fatal"]
        .iter()
        .any(|word| lower.contains(word))
    {
        tracing::warn!(target: "driver_acp::child", line = %shown, "agent stderr");
    } else {
        tracing::debug!(target: "driver_acp::child", line = %shown, "agent stderr");
    }
}

struct ChildTerminator {
    child: Child,
    status: Option<ExitStatus>,
}

impl Terminator for ChildTerminator {
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.status = Some(status);
                    return true;
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    }

    fn kill_tree(&mut self) {
        let pid = self.child.id();
        kill_group(pid);
        let _ = self.child.kill();
        if let Ok(status) = self.child.wait() {
            self.status = Some(status);
        }
    }

    fn exit_description(&self) -> Option<String> {
        self.status.map(|status| status.to_string())
    }
}

/// Kills the process group led by `pid` (Unix). The child was spawned with
/// `process_group(0)`, so its pgid equals its pid.
#[cfg(unix)]
fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{pid}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Kills the process tree (Windows) with `taskkill /T /F`, observed in the spikes.
#[cfg(windows)]
fn kill_group(pid: u32) {
    let system_root = std::env::var_os("SYSTEMROOT").unwrap_or_else(|| "C:\\Windows".into());
    let taskkill = std::path::PathBuf::from(system_root)
        .join("System32")
        .join("taskkill.exe");
    let _ = Command::new(taskkill)
        .arg("/T")
        .arg("/F")
        .arg("/PID")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn kill_group(_pid: u32) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    /// A child that writes one stderr line and exits with status 3.
    fn failing_child() -> LaunchSpec {
        #[cfg(unix)]
        let (program, args, env) = (
            PathBuf::from("/bin/sh"),
            vec![
                OsString::from("-c"),
                OsString::from("echo boom >&2; exit 3"),
            ],
            Vec::new(),
        );
        #[cfg(windows)]
        let (program, args, env) = {
            let root = std::env::var_os("SYSTEMROOT").unwrap_or_else(|| "C:\\Windows".into());
            (
                PathBuf::from(&root).join("System32").join("cmd.exe"),
                vec![
                    OsString::from("/C"),
                    OsString::from("echo boom 1>&2 & exit 3"),
                ],
                vec![(OsString::from("SYSTEMROOT"), root)],
            )
        };
        LaunchSpec {
            program,
            args,
            env,
            cwd: std::env::temp_dir(),
        }
    }

    #[test]
    fn exit_status_and_stderr_tail_are_observable_after_stdout_closes_0009_023() {
        let mut spawned = OsSpawner.spawn(&failing_child()).expect("spawn");
        let mut sink = Vec::new();
        spawned.stdout.read_to_end(&mut sink).expect("stdout EOF");
        assert!(spawned.terminator.wait_exit(Duration::from_secs(10)));
        let status = spawned.terminator.exit_description().expect("reaped");
        assert!(status.contains('3'), "exit status names code 3: {status}");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let tail = spawned.stderr_tail.lock().expect("tail").clone();
            if String::from_utf8_lossy(&tail).contains("boom") {
                break;
            }
            assert!(Instant::now() < deadline, "stderr tail reached the driver");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
