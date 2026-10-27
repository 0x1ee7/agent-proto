//! In-process fake ACP agent for offline tests. Each spawn runs a scripted
//! responder on a thread; stdin and stdout are in-memory pipes.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::LaunchSpec;
use crate::process::{Spawned, Spawner, Terminator};

/// What the fake agent does after receiving one inbound frame.
pub(crate) enum Step {
    /// Send these frames (in order) back to the driver.
    Emit(Vec<Value>),
    /// Exit at once, as a crashed process would (stdout reaches end-of-file).
    Crash,
}

/// Scripted responder for one agent process.
pub(crate) type Script = Box<dyn FnMut(&Value) -> Step + Send>;

/// Counters shared between a test and the fake processes it spawns.
#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) spawns: AtomicUsize,
    pub(crate) kills: AtomicUsize,
}

/// Spawner that runs a fresh scripted fake agent per spawn.
pub(crate) struct FakeSpawner {
    make_script: Box<dyn FnMut() -> Script + Send>,
    hang_on_stdin_close: bool,
    counters: Arc<Counters>,
}

struct ProcState {
    killed: AtomicBool,
    done: AtomicBool,
}

impl FakeSpawner {
    /// Creates a spawner; `make_script` is called once per spawn.
    pub(crate) fn new(
        make_script: impl FnMut() -> Script + Send + 'static,
    ) -> (Self, Arc<Counters>) {
        let counters = Arc::new(Counters::default());
        let spawner = Self {
            make_script: Box::new(make_script),
            hang_on_stdin_close: false,
            counters: counters.clone(),
        };
        (spawner, counters)
    }

    /// The fake process ignores stdin end-of-file and runs until killed (0009-036).
    pub(crate) fn hanging(mut self) -> Self {
        self.hang_on_stdin_close = true;
        self
    }
}

impl Spawner for FakeSpawner {
    fn spawn(&mut self, _spec: &LaunchSpec) -> io::Result<Spawned> {
        self.counters.spawns.fetch_add(1, Ordering::SeqCst);
        let (to_agent, from_driver) = mpsc::channel::<Vec<u8>>();
        let (to_driver, from_agent) = mpsc::channel::<Vec<u8>>();
        let state = Arc::new(ProcState {
            killed: AtomicBool::new(false),
            done: AtomicBool::new(false),
        });
        let script = (self.make_script)();
        let hang = self.hang_on_stdin_close;
        let thread_state = state.clone();
        drop(thread::spawn(move || {
            run_agent(script, hang, &from_driver, &to_driver, &thread_state);
        }));
        Ok(Spawned {
            stdin: Box::new(PipeWriter(to_agent)),
            stdout: Box::new(PipeReader {
                rx: from_agent,
                buf: Vec::new(),
                pos: 0,
            }),
            terminator: Box::new(FakeTerminator {
                state,
                counters: self.counters.clone(),
            }),
            stderr_tail: Arc::new(Mutex::new(Vec::new())),
        })
    }
}

fn run_agent(
    mut script: Script,
    hang: bool,
    inbox: &Receiver<Vec<u8>>,
    outbox: &Sender<Vec<u8>>,
    state: &ProcState,
) {
    let mut pending: Vec<u8> = Vec::new();
    'run: loop {
        if state.killed.load(Ordering::SeqCst) {
            break;
        }
        match inbox.recv_timeout(Duration::from_millis(5)) {
            Ok(bytes) => {
                pending.extend_from_slice(&bytes);
                while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=pos).collect();
                    let Ok(frame) = serde_json::from_slice::<Value>(&line) else {
                        continue;
                    };
                    match script(&frame) {
                        Step::Crash => break 'run,
                        Step::Emit(frames) => {
                            for out in frames {
                                let mut wire = serde_json::to_vec(&out).unwrap_or_default();
                                wire.push(b'\n');
                                if outbox.send(wire).is_err() {
                                    break 'run;
                                }
                            }
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if !hang {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    state.done.store(true, Ordering::SeqCst);
}

struct PipeWriter(Sender<Vec<u8>>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "fake agent exited"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(bytes) => {
                    self.buf = bytes;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

struct FakeTerminator {
    state: Arc<ProcState>,
    counters: Arc<Counters>,
}

impl Terminator for FakeTerminator {
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.state.done.load(Ordering::SeqCst) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn kill_tree(&mut self) {
        self.counters.kills.fetch_add(1, Ordering::SeqCst);
        self.state.killed.store(true, Ordering::SeqCst);
    }

    fn exit_description(&self) -> Option<String> {
        None
    }
}

/// Successful JSON-RPC response to request `id`.
pub(crate) fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// `session/update` notification for `session`.
pub(crate) fn update(session: &str, update: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session, "update": update}})
}
