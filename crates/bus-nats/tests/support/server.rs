//! A throwaway `nats-server` for the live tests: its own store, a free port, and
//! NKey users derived from the conformance fixtures. Killed and removed on drop.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use a2a_conformance::fixtures::{Fixtures, GrantSpec};
use bus_nats::layout::{kv_bucket, kv_stream};
use bus_nats::permissions::{
    Permissions, Role, ServerSpec, ServerUser, permissions, render_server_conf,
};
use bus_nats::provision::SUPERVISOR;

/// Environment override for the `nats-server` binary.
pub const SERVER_BIN_ENV: &str = "NATS_SERVER_BIN";
/// Spike binary used when `NATS_SERVER_BIN` is unset (Windows checkout).
const DEFAULT_SERVER_BIN: &str =
    "../../.spikes/nats/bin/srv/nats-server-v2.15.2-windows-amd64/nats-server.exe";
const READY_TIMEOUT: Duration = Duration::from_secs(20);

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A running `nats-server` with JetStream, configured for the suite fixtures.
pub struct TestServer {
    child: Child,
    dir: PathBuf,
    url: String,
    seeds: BTreeMap<String, String>,
}

impl TestServer {
    /// Starts a server whose users are the fixture principals (plus the
    /// supervisor and the denied principal, which has no user entry).
    ///
    /// # Errors
    /// Returns a message when the binary is missing, the config cannot be
    /// written, or the server does not accept connections in time.
    pub fn start(fx: &Fixtures) -> Result<Self, String> {
        let bin = server_binary()?;
        let mut seeds = BTreeMap::new();
        let users = server_users(fx, &mut seeds)?;
        let port = free_port()?;
        let dir = unique_dir()?;
        match spawn_and_wait(&bin, port, &dir, &users) {
            Ok(child) => Ok(Self {
                child,
                dir,
                url: format!("nats://127.0.0.1:{port}"),
                seeds,
            }),
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    /// The client URL of this server.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The NKey seed of `identity`, as the server knows it.
    ///
    /// # Errors
    /// Returns a message when the identity was not generated for this server.
    pub fn seed(&self, identity: &str) -> Result<String, String> {
        self.seeds
            .get(identity)
            .cloned()
            .ok_or_else(|| format!("no NKey generated for {identity}"))
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn io(error: std::io::Error) -> String {
    error.to_string()
}

fn server_binary() -> Result<PathBuf, String> {
    let path = match std::env::var_os(SERVER_BIN_ENV) {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_SERVER_BIN),
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "nats-server binary not found at {}; set {SERVER_BIN_ENV}",
            path.display()
        ))
    }
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(io)?;
    Ok(listener.local_addr().map_err(io)?.port())
}

fn unique_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let counter = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bus-nats-live-{}-{nanos}-{counter}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(io)?;
    Ok(dir)
}

fn spawn_and_wait(
    bin: &Path,
    port: u16,
    dir: &Path,
    users: &[ServerUser],
) -> Result<Child, String> {
    let store = dir.join("store");
    fs::create_dir_all(&store).map_err(io)?;
    let conf_path = dir.join("nats.conf");
    let conf = render_server_conf(&ServerSpec {
        host: "127.0.0.1",
        port,
        store_dir: &store,
        users,
    });
    fs::write(&conf_path, conf).map_err(io)?;
    let log = File::create(dir.join("server.log")).map_err(io)?;
    let log_err = log.try_clone().map_err(io)?;
    let mut child = Command::new(bin)
        .arg("-c")
        .arg(&conf_path)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(io)?;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(child);
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "nats-server exited early ({status}); see {}",
                dir.join("server.log").display()
            ));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "nats-server not listening within {READY_TIMEOUT:?}; see {}",
                dir.join("server.log").display()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Generates a user key for `identity`, recording its seed.
fn keypair(seeds: &mut BTreeMap<String, String>, identity: &str) -> Result<String, String> {
    let pair = nkeys::KeyPair::new_user();
    let seed = pair.seed().map_err(|error| error.to_string())?;
    seeds.insert(identity.to_owned(), seed);
    Ok(pair.public_key())
}

fn server_users(
    fx: &Fixtures,
    seeds: &mut BTreeMap<String, String>,
) -> Result<Vec<ServerUser>, String> {
    let mut users = Vec::new();
    users.push(ServerUser {
        public_key: keypair(seeds, SUPERVISOR)?,
        permissions: supervisor_permissions(fx)?,
    });
    for grant in &fx.grants {
        let identity = grant
            .principal
            .name()
            .ok_or("fixture grant without a principal name")?;
        users.push(ServerUser {
            public_key: keypair(seeds, identity)?,
            permissions: grant_permissions(fx, grant)?,
        });
    }
    // The denied principal gets a seed but no user entry: connecting must fail.
    let denied = fx
        .denied
        .name()
        .ok_or("fixture denied principal without a name")?;
    keypair(seeds, denied)?;
    Ok(users)
}

/// Supervisor: the RFC-0010 supervisor permissions for both tenants, plus the
/// JetStream administration the suite fixtures need (streams and buckets are
/// created by the harness, not by `provision`).
fn supervisor_permissions(fx: &Fixtures) -> Result<Permissions, String> {
    let mut merged = Permissions {
        publish: Vec::new(),
        subscribe: Vec::new(),
        responses: false,
    };
    for tenant in [&fx.local, &fx.other] {
        let perms = permissions(Role::Supervisor, SUPERVISOR, tenant)
            .map_err(|error| format!("{error:?}"))?;
        merge(&mut merged, perms);
    }
    merged.publish.push("$JS.API.>".to_owned());
    merged.publish.push("$KV.>".to_owned());
    merged.publish.push("$JS.ACK.>".to_owned());
    merged.subscribe.push(format!("_INBOX.{SUPERVISOR}.>"));
    Ok(merged)
}

/// A fixture principal: the RFC-0010 agent permissions for its tenant, plus
/// the grant's publish and serve patterns, its streams, and the suite buckets
/// of its tenant.
fn grant_permissions(fx: &Fixtures, grant: &GrantSpec) -> Result<Permissions, String> {
    let name = grant
        .principal
        .name()
        .ok_or("fixture grant without a principal name")?;
    let mut perms =
        permissions(Role::Agent, name, &grant.tenant).map_err(|error| format!("{error:?}"))?;
    perms
        .publish
        .extend(grant.publish.iter().map(|p| p.as_str().to_owned()));
    perms
        .subscribe
        .extend(grant.serve.iter().map(|p| p.as_str().to_owned()));
    for stream in &grant.streams {
        perms.publish.extend(read_bundle(stream.as_str()));
    }
    for bucket in fx.buckets.iter().filter(|b| b.tenant() == &grant.tenant) {
        let kv = kv_bucket(bucket.tenant(), bucket.name());
        perms.publish.push(format!("$KV.{kv}.>"));
        perms.publish.extend(read_bundle(&kv_stream(&kv)));
    }
    Ok(perms)
}

/// The JetStream read subjects of one stream (mirrors the RFC-0010 read bundle
/// that `bus_nats::permissions` grants to agents on their event streams).
fn read_bundle(stream: &str) -> Vec<String> {
    vec![
        format!("$JS.API.STREAM.INFO.{stream}"),
        format!("$JS.API.CONSUMER.CREATE.{stream}"),
        format!("$JS.API.CONSUMER.CREATE.{stream}.>"),
        format!("$JS.API.CONSUMER.DURABLE.CREATE.{stream}.>"),
        format!("$JS.API.CONSUMER.INFO.{stream}.>"),
        format!("$JS.API.CONSUMER.DELETE.{stream}.>"),
        format!("$JS.API.DIRECT.GET.{stream}"),
        format!("$JS.API.DIRECT.GET.{stream}.>"),
        format!("$JS.API.STREAM.MSG.GET.{stream}"),
        format!("$JS.ACK.{stream}.>"),
    ]
}

fn merge(into: &mut Permissions, from: Permissions) {
    into.publish.extend(from.publish);
    into.subscribe.extend(from.subscribe);
    into.responses |= from.responses;
}
