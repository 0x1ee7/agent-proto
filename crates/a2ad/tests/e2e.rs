//! End-to-end tests for the `a2ad` binary (RFC-0012; exit codes per 0012-055).
//!
//! Plain tests run `check-config`, `status` and `stop` against generated configs. They start
//! no bus and no worker, so they run on every `cargo test -p a2ad --test e2e`.
//!
//! Live tests are `#[ignore]`. Each one starts `a2ad run` with a real nats-server and an omp
//! worker. The test starts the omp stub (`spikes/omp-acp/stub/stub_openai.py`) itself and
//! points omp only at that stub, so no real provider is reached. Required environment:
//!
//! - `A2AD_E2E_NATS_BIN`: path to the nats-server executable.
//! - `A2AD_E2E_NATS_SHA256`: lowercase SHA-256 of that executable (0010-069).
//! - `A2AD_E2E_OMP`: path to the omp executable.
//!
//! Optional: `A2AD_E2E_PYTHON` (default `python`), `A2AD_E2E_BUS_PORT` (default 4292),
//! `A2AD_E2E_HEALTH_PORT` (default 9496), `A2AD_E2E_STUB_PORT` (default 18796). The rival
//! supervisor in the lock test uses the bus and health ports plus one.
//!
//! Run the live tests with `cargo test -p a2ad --test e2e -- --ignored --test-threads=1`.
//! `git` must be on PATH: the worker repository is created with `git init`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const A2AD: &str = env!("CARGO_BIN_EXE_a2ad");
const FAKE_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const STUB_SCRIPT: &str = "../../spikes/omp-acp/stub/stub_openai.py";
const POLL: Duration = Duration::from_millis(250);
const READY_TIMEOUT: Duration = Duration::from_secs(180);
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// The live tests share ports and state layout conventions; run them one at a time.
static LIVE: Mutex<()> = Mutex::new(());

// ---------- helpers ----------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("e2e")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A TOML literal string for `path`. Literal strings keep Windows backslashes as written.
fn lit(path: &Path) -> String {
    let text = path.to_string_lossy();
    assert!(
        !text.contains('\''),
        "a TOML literal string cannot hold a quote: {text}"
    );
    format!("'{text}'")
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("a2ad was killed by a signal")
}

/// Runs `a2ad <command> --config <config> <extra...>` to completion.
fn a2ad(command: &str, config: &Path, extra: &[&str]) -> Output {
    Command::new(A2AD)
        .arg(command)
        .arg("--config")
        .arg(config)
        .args(extra)
        .output()
        .expect("run a2ad")
}

fn status_json(config: &Path) -> Value {
    let out = a2ad("status", config, &[]);
    assert_eq!(exit_code(&out), 0, "status failed: {}", utf8(&out.stderr));
    serde_json::from_slice(&out.stdout).expect("status prints JSON")
}

/// One scratch directory with the paths and binaries a supervisor config refers to.
struct Setup {
    dir: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    omp_home: PathBuf,
    nats: PathBuf,
    nats_sha256: String,
    omp: PathBuf,
}

impl Setup {
    fn new(name: &str, nats: PathBuf, nats_sha256: String, omp: PathBuf) -> Self {
        let dir = scratch(name);
        Self {
            state: dir.join("state"),
            repo: dir.join("repo"),
            omp_home: dir.join("omp-agent"),
            dir,
            nats,
            nats_sha256,
            omp,
        }
    }

    /// A complete supervisor config with one omp worker on the given bus and health ports.
    fn config(&self, bus_port: u16, health_port: u16) -> String {
        format!(
            "config_version = 1\n\
             \n\
             [paths]\n\
             state_dir = {state}\n\
             \n\
             [supervisor]\n\
             drain_ms = 5000\n\
             grace_ms = 3000\n\
             operator_id = \"cli\"\n\
             health_listen = \"127.0.0.1:{health_port}\"\n\
             \n\
             [bus]\n\
             port = {bus_port}\n\
             binary = {nats}\n\
             sha256 = \"{sha256}\"\n\
             \n\
             [[agent]]\n\
             id = \"omp\"\n\
             kind = \"worker\"\n\
             backend = \"acp-omp\"\n\
             command = [{omp}]\n\
             repo = {repo}\n\
             approval_mode = \"ask-exec\"\n\
             env = {{ PI_CODING_AGENT_DIR = {omp_home} }}\n\
             env_passthrough = \"all\"\n\
             readiness = {{ kind = \"bus-presence\" }}\n",
            state = lit(&self.state),
            nats = lit(&self.nats),
            sha256 = self.nats_sha256,
            omp = lit(&self.omp),
            repo = lit(&self.repo),
            omp_home = lit(&self.omp_home),
        )
    }

    fn write_config(&self, file: &str, text: &str) -> PathBuf {
        let path = self.dir.join(file);
        fs::write(&path, text).unwrap();
        path
    }

    /// Creates the git repository and the omp agent directory that point at the omp stub.
    fn prepare_live(&self, stub_port: u16) {
        fs::create_dir_all(&self.repo).unwrap();
        git(&self.repo, &["init", "-q"]);
        git(
            &self.repo,
            &[
                "-c",
                "user.name=a2ad-e2e",
                "-c",
                "user.email=e2e@localhost",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        fs::create_dir_all(&self.omp_home).unwrap();
        fs::write(self.omp_home.join("models.yml"), models_yml(stub_port)).unwrap();
        fs::write(self.omp_home.join("config.yml"), CONFIG_YML).unwrap();
    }
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git on PATH");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        utf8(&out.stderr)
    );
}

/// Stub provider definition. The API key is the dummy value the omp stub expects.
fn models_yml(stub_port: u16) -> String {
    format!(
        "providers:\n\
         \x20 stub:\n\
         \x20   baseUrl: http://127.0.0.1:{stub_port}/v1\n\
         \x20   apiKey: stub-local-key\n\
         \x20   api: openai-completions\n\
         \x20   models:\n\
         \x20     - id: stub-model\n\
         \x20       name: stub-model\n\
         \x20       contextWindow: 32000\n\
         \x20       maxTokens: 4096\n\
         \x20       input: [text]\n"
    )
}

const CONFIG_YML: &str = "modelRoles:\n\
     \x20 default: stub/stub-model\n\
     \x20 smol: stub/stub-model\n\
     \x20 slow: stub/stub-model\n\
     \x20 tiny: stub/stub-model\n\
     \x20 memory: stub/stub-model\n\
     \x20 task: stub/stub-model\n\
     \x20 plan: stub/stub-model\n\
     \x20 commit: stub/stub-model\n";

/// Minimal HTTP/1.1 GET over loopback. Returns the status code, or `None` if nothing answered.
fn http_status(port: u16, path: &str) -> Option<u16> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).ok()?;
    reply.split_whitespace().nth(1)?.parse().ok()
}

/// A running `a2ad run`. Drop asks it to stop and kills it if it is still alive.
struct Supervisor {
    child: Child,
    config: PathBuf,
    err_log: PathBuf,
}

impl Supervisor {
    fn start(config: &Path, dir: &Path, name: &str) -> Self {
        let out = File::create(dir.join(format!("{name}.out"))).unwrap();
        let err_log = dir.join(format!("{name}.err"));
        let err = File::create(&err_log).unwrap();
        let child = Command::new(A2AD)
            .arg("run")
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()
            .expect("spawn a2ad run");
        Self {
            child,
            config: config.to_owned(),
            err_log,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn stderr(&self) -> String {
        fs::read_to_string(&self.err_log).unwrap_or_default()
    }

    fn assert_alive(&mut self) {
        let status = self.child.try_wait().expect("poll a2ad run");
        assert!(
            status.is_none(),
            "a2ad run exited early with {status:?}:\n{}",
            self.stderr()
        );
    }

    /// Polls `/readyz` until it returns 200. Fails early if the supervisor exits first.
    fn wait_ready(&mut self, health_port: u16, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while http_status(health_port, "/readyz") != Some(200) {
            self.assert_alive();
            assert!(
                Instant::now() < deadline,
                "/readyz was not 200 within {timeout:?}:\n{}",
                self.stderr()
            );
            thread::sleep(POLL);
        }
    }

    /// Polls `status` until it reports this supervisor as running with its pid. For tests whose
    /// subject is not readiness.
    fn wait_running(&mut self, config: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            // `status` fails until the supervisor has created `run/`; keep polling until then.
            let out = a2ad("status", config, &[]);
            if exit_code(&out) == 0 {
                let report: Value =
                    serde_json::from_slice(&out.stdout).expect("status prints JSON");
                if report["running"] == true
                    && report["supervisor_pid"].as_u64() == Some(u64::from(self.id()))
                {
                    return;
                }
            }
            self.assert_alive();
            assert!(
                Instant::now() < deadline,
                "status did not report the supervisor running within {timeout:?}:\n{}",
                self.stderr()
            );
            thread::sleep(POLL);
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll a2ad run") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "a2ad run did not exit within {timeout:?}:\n{}",
                self.stderr()
            );
            thread::sleep(POLL);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = a2ad("stop", &self.config, &["--timeout-secs", "60"]);
            if matches!(self.child.try_wait(), Ok(None)) {
                let _ = self.child.kill();
            }
        }
        let _ = self.child.wait();
    }
}

/// The omp stub started by the test. Drop kills it.
struct Stub(Child);

impl Drop for Stub {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_stub(setup: &Setup, python: &str, port: u16) -> Stub {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join(STUB_SCRIPT);
    let err = File::create(setup.dir.join("stub.err")).unwrap();
    let child = Command::new(python)
        .arg(script)
        .arg(port.to_string())
        .arg(setup.dir.join("stub-logs"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .spawn()
        .expect("start omp stub");
    let stub = Stub(child);
    let deadline = Instant::now() + Duration::from_secs(20);
    while http_status(port, "/v1/models") != Some(200) {
        assert!(
            Instant::now() < deadline,
            "omp stub is not serving on port {port}"
        );
        thread::sleep(POLL);
    }
    stub
}

struct LiveEnv {
    nats: PathBuf,
    nats_sha256: String,
    omp: PathBuf,
    python: String,
    bus_port: u16,
    health_port: u16,
    stub_port: u16,
}

fn live_env() -> LiveEnv {
    let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let port = |key: &str, default: u16| var(key).map_or(default, |v| v.parse().expect(key));
    LiveEnv {
        nats: PathBuf::from(var("A2AD_E2E_NATS_BIN").expect("set A2AD_E2E_NATS_BIN")),
        nats_sha256: var("A2AD_E2E_NATS_SHA256")
            .expect("set A2AD_E2E_NATS_SHA256")
            .to_ascii_lowercase(),
        omp: PathBuf::from(var("A2AD_E2E_OMP").expect("set A2AD_E2E_OMP")),
        python: var("A2AD_E2E_PYTHON").unwrap_or_else(|| "python".to_owned()),
        bus_port: port("A2AD_E2E_BUS_PORT", 4292),
        health_port: port("A2AD_E2E_HEALTH_PORT", 9496),
        stub_port: port("A2AD_E2E_STUB_PORT", 18796),
    }
}

/// A plain setup whose binary paths are never executed (check-config, status and stop only).
fn plain_setup(name: &str) -> Setup {
    Setup::new(
        name,
        PathBuf::from("unused/nats-server"),
        FAKE_SHA256.to_owned(),
        PathBuf::from("unused/omp"),
    )
}

fn live_setup(name: &str, env: &LiveEnv) -> Setup {
    let setup = Setup::new(
        name,
        env.nats.clone(),
        env.nats_sha256.clone(),
        env.omp.clone(),
    );
    setup.prepare_live(env.stub_port);
    setup
}

// ---------- plain tests: no bus, no worker ------------------------------------------------

#[test]
fn req_0012_004_check_config_accepts_a_valid_config() {
    let setup = plain_setup("check-valid");
    let cfg = setup.write_config("supervisor.toml", &setup.config(4292, 9496));
    let out = a2ad("check-config", &cfg, &[]);
    assert_eq!(exit_code(&out), 0, "{}", utf8(&out.stderr));
    assert!(
        utf8(&out.stdout).starts_with("config ok: 1 agent(s), state dir "),
        "{}",
        utf8(&out.stdout)
    );
}

#[test]
fn req_0012_001_unknown_key_is_refused_with_exit_4_and_names_the_key() {
    let setup = plain_setup("check-unknown");
    let text = setup
        .config(4292, 9496)
        .replacen("[bus]\n", "[bus]\nbogus_key = 1\n", 1);
    let cfg = setup.write_config("supervisor.toml", &text);
    let out = a2ad("check-config", &cfg, &[]);
    assert_eq!(exit_code(&out), 4);
    let err = utf8(&out.stderr);
    assert!(
        err.contains("bogus_key"),
        "error does not name the key: {err}"
    );
}

#[test]
fn req_0010_069_missing_bus_digest_is_refused_with_exit_4() {
    let setup = plain_setup("check-no-digest");
    let text = setup
        .config(4292, 9496)
        .replace(&format!("sha256 = \"{FAKE_SHA256}\"\n"), "");
    let cfg = setup.write_config("supervisor.toml", &text);
    let out = a2ad("check-config", &cfg, &[]);
    assert_eq!(exit_code(&out), 4);
    assert!(
        utf8(&out.stderr).contains("sha256"),
        "{}",
        utf8(&out.stderr)
    );
}

#[test]
fn req_0012_055_missing_config_exits_3() {
    let setup = plain_setup("check-missing");
    let out = a2ad("check-config", &setup.dir.join("absent.toml"), &[]);
    assert_eq!(exit_code(&out), 3);
}

/// `status` with no supervisor: nothing is running and no agent is listed. The `run/`
/// directory exists as an earlier supervisor run leaves it.
#[test]
fn status_reports_not_running_when_no_supervisor_holds_the_lock() {
    let setup = plain_setup("status-idle");
    fs::create_dir_all(setup.state.join("run")).unwrap();
    let cfg = setup.write_config("supervisor.toml", &setup.config(4292, 9496));
    let report = status_json(&cfg);
    assert_eq!(report["running"], false);
    assert!(report["supervisor_pid"].is_null(), "{report}");
    assert_eq!(report["agents"], serde_json::json!([]));
}

#[test]
fn req_0012_055_stop_without_supervisor_exits_0() {
    let setup = plain_setup("stop-idle");
    let cfg = setup.write_config("supervisor.toml", &setup.config(4292, 9496));
    let out = a2ad("stop", &cfg, &["--timeout-secs", "5"]);
    assert_eq!(exit_code(&out), 0, "{}", utf8(&out.stderr));
    assert_eq!(utf8(&out.stdout).trim(), "no supervisor is running");
}

// ---------- live tests: real nats-server, omp worker and omp stub ------------------------

/// [0013-036] `/readyz` is 200 on a live supervisor; [0012-055] `stop` then exits 0.
#[test]
#[ignore = "needs A2AD_E2E_NATS_BIN, A2AD_E2E_NATS_SHA256 and A2AD_E2E_OMP; see the file header"]
fn req_0013_036_live_supervisor_reaches_readyz_and_stops_with_exit_0() {
    let _live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
    let env = live_env();
    let setup = live_setup("live-run", &env);
    let _stub = start_stub(&setup, &env.python, env.stub_port);
    let cfg = setup.write_config(
        "supervisor.toml",
        &setup.config(env.bus_port, env.health_port),
    );

    let mut sup = Supervisor::start(&cfg, &setup.dir, "run");
    sup.wait_ready(env.health_port, READY_TIMEOUT);

    let status = status_json(&cfg);
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["supervisor_pid"].as_u64(), Some(u64::from(sup.id())));
    let omp_record = status["agents"]
        .as_array()
        .expect("agents is an array")
        .iter()
        .find(|record| record["agent"] == "omp");
    assert!(
        omp_record.is_some_and(|record| record["pid"].as_u64().unwrap_or(0) > 0),
        "no run record with a pid for omp: {status}"
    );

    let stop = a2ad("stop", &cfg, &["--timeout-secs", "90"]);
    assert_eq!(exit_code(&stop), 0, "stop stderr: {}", utf8(&stop.stderr));
    assert!(
        utf8(&stop.stdout).contains(&format!("supervisor {} stopped", sup.id())),
        "{}",
        utf8(&stop.stdout)
    );
    assert_eq!(sup.wait_exit(STOP_TIMEOUT).code(), Some(0));
    assert_eq!(status_json(&cfg)["running"], false);
}

#[test]
#[ignore = "needs A2AD_E2E_NATS_BIN, A2AD_E2E_NATS_SHA256 and A2AD_E2E_OMP; see the file header"]
fn req_0012_005_second_supervisor_on_same_state_exits_6() {
    let _live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
    let env = live_env();
    let setup = live_setup("live-lock", &env);
    let _stub = start_stub(&setup, &env.python, env.stub_port);
    let cfg = setup.write_config(
        "supervisor.toml",
        &setup.config(env.bus_port, env.health_port),
    );
    let rival = setup.write_config(
        "rival.toml",
        &setup.config(env.bus_port + 1, env.health_port + 1),
    );

    let mut sup = Supervisor::start(&cfg, &setup.dir, "run");
    // Readiness is not the subject here; the lock is held once status reports the run.
    sup.wait_running(&cfg, READY_TIMEOUT);

    let mut second = Supervisor::start(&rival, &setup.dir, "rival");
    assert_eq!(second.wait_exit(LOCK_TIMEOUT).code(), Some(6));
    let err = second.stderr();
    assert!(
        err.contains(&format!("supervisor lock held by pid {}", sup.id())),
        "rival does not name the lock holder: {err}"
    );
}
