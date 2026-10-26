//! Agent process spawning and tree termination (RFC-0009 4.9, 0009-059, 0009-060).
//!
//! The actor talks to agents only through [`Spawner`], so tests can substitute
//! in-memory pipes for real processes.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::LaunchSpec;

/// Maximum stderr bytes kept for crash detail.
const STDERR_TAIL_BYTES: usize = 2048;

/// Termination handle for one agent process tree.
pub(crate) trait Terminator: Send {
    /// Waits up to `timeout` for the process to exit on its own. Returns true if it did.
    fn wait_exit(&mut self, timeout: Duration) -> bool;
    /// Kills the process and its descendants. Best effort, idempotent.
    fn kill_tree(&mut self);
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
        Ok(Spawned {
            stdin: Box::new(ChildStdinWriter(stdin)),
            stdout: Box::new(ChildStdoutReader(stdout)),
            terminator: Box::new(ChildTerminator { child }),
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

fn drain_stderr(mut stderr: ChildStderr, tail: Arc<Mutex<Vec<u8>>>) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 512];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if let Ok(mut buf) = tail.lock() {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.len() > STDERR_TAIL_BYTES {
                            let excess = buf.len() - STDERR_TAIL_BYTES;
                            buf.drain(..excess);
                        }
                    }
                }
            }
        }
    });
}

struct ChildTerminator {
    child: Child,
}

impl Terminator for ChildTerminator {
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
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
        let _ = self.child.wait();
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
