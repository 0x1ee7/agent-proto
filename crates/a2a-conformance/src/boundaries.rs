//! Boundary check for RFC-0001 rules [0001-001] to [0001-006].
//!
//! The check reads the workspace's declared dependencies from `cargo metadata`, classifies
//! every workspace crate by the layer order of RFC-0001 section 4.2, and reports each breach as
//! a [`Violation`] that names the rule, the crate and the dependency. Every rule first selects
//! its subject crates; an empty subject set is itself a violation, so no rule can pass
//! vacuously. Dev-dependencies are exempt from the dependency rules, as RFC-0001 allows.
//!
//! [`check_dependency_rules`] covers the graph rules (0001-001 to 0001-004 and the layer order).
//! [`check_crate_files`] covers the file rules (0001-005 and 0001-006). [`check_workspace`] runs
//! both on the real workspace.

use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Rule [0001-001]: only L1 bindings and L5 executables name `bus-nats` or `bus-mem`.
pub const RULE_BINDINGS: &str = "0001-001";
/// Rule [0001-002]: drivers are independent of each other, the gateway and any concrete bus.
pub const RULE_DRIVERS: &str = "0001-002";
/// Rule [0001-003]: the gateway has no `driver-*` dependency.
pub const RULE_GATEWAY: &str = "0001-003";
/// Rule [0001-004]: `a2a-core` does no I/O except through `bus-api`.
pub const RULE_CORE_IO: &str = "0001-004";
/// Rule [0001-005]: each crate directory carries `AGENTS.md` (owns/provides/uses) and the shim.
pub const RULE_CRATE_FILES: &str = "0001-005";
/// Rule [0001-006]: each swappable seam is one trait, defined in its owner crate.
pub const RULE_SEAMS: &str = "0001-006";
/// RFC-0001 section 4.1 and 4.2: a crate depends only on strictly lower layers.
pub const RULE_LAYER: &str = "0001-s4.2";

/// Crates that may name a concrete bus binding in a normal or build dependency.
pub const BINDING_HOLDERS: &[&str] = &[
    "bus-mem",
    "bus-nats",
    "a2ad",
    "a2a-tui",
    "a2a-cli",
    "a2a-conformance",
];
/// The concrete bus bindings.
pub const BINDINGS: &[&str] = &["bus-mem", "bus-nats"];
/// Workspace crates `a2a-core` may depend on.
pub const CORE_ALLOWED: &[&str] = &["a2a-types", "bus-api"];
/// External crates that perform I/O; `a2a-core` may not depend on them.
pub const IO_CRATES: &[&str] = &[
    "async-nats",
    "axum",
    "h2",
    "hyper",
    "hyper-util",
    "mio",
    "nix",
    "reqwest",
    "rustls",
    "socket2",
    "tokio",
    "tokio-rustls",
    "tokio-stream",
    "tokio-util",
    "tower",
    "tower-http",
];
/// Seam traits (RFC-0001 [0001-006]) and the crate that owns each.
pub const SEAMS: &[(&str, &str)] = &[
    ("Bus", "bus-api"),
    ("Backend", "a2a-worker"),
    ("Signer", "a2a-security"),
    ("Verifier", "a2a-security"),
    ("AuditSink", "a2a-audit"),
];
/// Test crate: exempt from the layer order; it may depend on any layer as needed.
pub const TEST_CRATES: &[&str] = &["a2a-conformance"];

/// Layer rank of each workspace crate (RFC-0001 section 4.2). A dependency must have a strictly
/// lower rank. `bus-api` sits below the bindings, which sit below the core libraries.
pub const LAYERS: &[(&str, u8)] = &[
    ("a2a-types", 0),
    ("bus-api", 1),
    ("bus-mem", 2),
    ("bus-nats", 2),
    ("a2a-core", 3),
    ("a2a-security", 3),
    ("a2a-workspace", 3),
    ("a2a-audit", 3),
    ("a2a-mcp", 3),
    ("a2a-worker", 4),
    ("a2a-client", 4),
    ("driver-acp", 5),
    ("driver-a2a-http", 5),
    ("gateway-http", 5),
    ("a2ad", 6),
    ("a2a-tui", 6),
    ("a2a-cli", 6),
];

/// Layer-order exceptions: each edge is allowed by a named RFC and must still exist, so a stale
/// entry fails the check. `a2a-worker` runs delegation through `a2a-client` (RFC-0015
/// [0015-001]); RFC-0001 section 4.2 does not list that edge yet.
pub const LAYER_EXCEPTIONS: &[(&str, &str, &str)] = &[(
    "a2a-worker",
    "a2a-client",
    "RFC-0015 [0015-001]: the worker's delegation endpoint calls a2a-client",
)];

/// Error while loading the workspace graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryError(pub String);

impl Display for BoundaryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BoundaryError {}

/// Kind of a declared dependency, as `cargo metadata` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// `[dependencies]`.
    Normal,
    /// `[build-dependencies]`.
    Build,
    /// `[dev-dependencies]`.
    Dev,
}

/// One declared dependency of a workspace crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Package name of the dependency.
    pub name: String,
    /// Declared kind.
    pub kind: DepKind,
}

/// One workspace crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Package name.
    pub name: String,
    /// Directory holding the manifest.
    pub dir: PathBuf,
    /// Declared dependencies, all kinds.
    pub dependencies: Vec<Dependency>,
}

/// The workspace dependency graph. Holds workspace members only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Graph {
    /// Workspace member packages.
    pub packages: Vec<Package>,
}

/// One breach of a boundary rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Rule identifier, for example `0001-001`.
    pub rule: &'static str,
    /// Crate (or subject set) the rule names.
    pub krate: String,
    /// Dependency or trait the breach concerns, when there is one.
    pub dependency: Option<String>,
    /// What was observed and what the rule requires.
    pub detail: String,
}

impl Violation {
    fn new(
        rule: &'static str,
        krate: impl Into<String>,
        dependency: Option<&str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            rule,
            krate: krate.into(),
            dependency: dependency.map(str::to_owned),
            detail: detail.into(),
        }
    }
}

impl Display for Violation {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.dependency {
            Some(dep) => write!(
                f,
                "[{}] crate `{}` -> `{}`: {}",
                self.rule, self.krate, dep, self.detail
            ),
            None => write!(f, "[{}] `{}`: {}", self.rule, self.krate, self.detail),
        }
    }
}

impl Graph {
    /// Parses `cargo metadata --format-version 1` output. Only packages listed in
    /// `workspace_members` are kept; each dependency keeps its declared kind.
    ///
    /// # Errors
    /// Returns [`BoundaryError`] when the JSON is malformed or lacks a required field.
    pub fn from_metadata(json: &str) -> Result<Self, BoundaryError> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| BoundaryError(format!("metadata JSON: {e}")))?;
        let members: BTreeSet<&str> = array(&value, "workspace_members")?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        let mut packages = Vec::new();
        for package in array(&value, "packages")? {
            let id = str_field(package, "id")?;
            if !members.contains(id) {
                continue;
            }
            let name = str_field(package, "name")?.to_owned();
            let manifest = str_field(package, "manifest_path")?;
            let dir = Path::new(manifest)
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| BoundaryError(format!("manifest without directory: {manifest}")))?;
            let mut dependencies = Vec::new();
            for dep in array(package, "dependencies")? {
                let kind = match dep.get("kind").and_then(serde_json::Value::as_str) {
                    None => DepKind::Normal,
                    Some("dev") => DepKind::Dev,
                    Some("build") => DepKind::Build,
                    Some(other) => {
                        return Err(BoundaryError(format!(
                            "unknown dependency kind `{other}` in `{name}`"
                        )));
                    }
                };
                dependencies.push(Dependency {
                    name: str_field(dep, "name")?.to_owned(),
                    kind,
                });
            }
            packages.push(Package {
                name,
                dir,
                dependencies,
            });
        }
        Ok(Self { packages })
    }

    fn package(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    fn has_normal_edge(&self, from: &str, to: &str) -> bool {
        self.package(from)
            .is_some_and(|p| normal_deps(p).any(|d| d.name == to))
    }
}

fn array<'v>(
    value: &'v serde_json::Value,
    key: &str,
) -> Result<&'v Vec<serde_json::Value>, BoundaryError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| BoundaryError(format!("metadata lacks array `{key}`")))
}

fn str_field<'v>(value: &'v serde_json::Value, key: &str) -> Result<&'v str, BoundaryError> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| BoundaryError(format!("metadata lacks string `{key}`")))
}

/// Runs `cargo metadata --no-deps` for the workspace at `root`.
///
/// # Errors
/// Returns [`BoundaryError`] when cargo cannot be run, fails, or prints unparsable output.
pub fn load_workspace(root: &Path) -> Result<Graph, BoundaryError> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--no-deps")
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .map_err(|e| BoundaryError(format!("cannot run cargo metadata: {e}")))?;
    if !output.status.success() {
        return Err(BoundaryError(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let json = String::from_utf8(output.stdout)
        .map_err(|e| BoundaryError(format!("cargo metadata output is not UTF-8: {e}")))?;
    Graph::from_metadata(&json)
}

/// The workspace root, two levels above this crate.
#[must_use]
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Loads the real workspace and runs every rule. The shim is `crates/CLAUDE.md`.
///
/// # Errors
/// Returns [`BoundaryError`] when the graph or the shim cannot be read.
pub fn check_workspace(root: &Path) -> Result<Vec<Violation>, BoundaryError> {
    let graph = load_workspace(root)?;
    let shim_path = root.join("crates").join("CLAUDE.md");
    let shim = fs::read_to_string(&shim_path)
        .map_err(|e| BoundaryError(format!("cannot read {}: {e}", shim_path.display())))?;
    let mut out = check_dependency_rules(&graph);
    out.extend(check_crate_files(&graph, &shim));
    Ok(out)
}

fn normal_deps(package: &Package) -> impl Iterator<Item = &Dependency> {
    package
        .dependencies
        .iter()
        .filter(|d| d.kind != DepKind::Dev)
}

fn is_driver(name: &str) -> bool {
    name.starts_with("driver-")
}

/// Selects the subject crates of a rule. An empty set is recorded as a violation.
fn select<'g>(
    graph: &'g Graph,
    rule: &'static str,
    subject: &str,
    keep: impl Fn(&str) -> bool,
    out: &mut Vec<Violation>,
) -> Vec<&'g Package> {
    let found: Vec<&Package> = graph.packages.iter().filter(|p| keep(&p.name)).collect();
    if found.is_empty() {
        out.push(Violation::new(
            rule,
            subject,
            None,
            "subject set is empty in the workspace; the rule would pass vacuously",
        ));
    }
    found
}

/// Checks the graph rules: [0001-001] to [0001-004] and the RFC-0001 layer order. Pure: reads
/// no files.
#[must_use]
pub fn check_dependency_rules(graph: &Graph) -> Vec<Violation> {
    let mut out = Vec::new();
    let members: BTreeSet<&str> = graph.packages.iter().map(|p| p.name.as_str()).collect();

    select(
        graph,
        RULE_BINDINGS,
        "bus binding crates",
        |n| BINDINGS.contains(&n),
        &mut out,
    );
    for package in graph
        .packages
        .iter()
        .filter(|p| !BINDING_HOLDERS.contains(&p.name.as_str()))
    {
        for dep in normal_deps(package).filter(|d| BINDINGS.contains(&d.name.as_str())) {
            out.push(Violation::new(
                RULE_BINDINGS,
                &package.name,
                Some(&dep.name),
                "names a concrete bus binding; only L1 bindings and L5 executables may (dev edges are exempt)",
            ));
        }
    }

    for driver in select(graph, RULE_DRIVERS, "driver crates", is_driver, &mut out) {
        for dep in normal_deps(driver) {
            if is_driver(&dep.name) {
                out.push(Violation::new(
                    RULE_DRIVERS,
                    &driver.name,
                    Some(&dep.name),
                    "drivers are independent of each other",
                ));
            } else if dep.name == "gateway-http" {
                out.push(Violation::new(
                    RULE_DRIVERS,
                    &driver.name,
                    Some(&dep.name),
                    "drivers are independent of the gateway",
                ));
            } else if BINDINGS.contains(&dep.name.as_str()) {
                out.push(Violation::new(
                    RULE_DRIVERS,
                    &driver.name,
                    Some(&dep.name),
                    "drivers are independent of any concrete bus",
                ));
            }
        }
    }

    for gateway in select(
        graph,
        RULE_GATEWAY,
        "gateway crate",
        |n| n == "gateway-http",
        &mut out,
    ) {
        for dep in normal_deps(gateway).filter(|d| is_driver(&d.name)) {
            out.push(Violation::new(
                RULE_GATEWAY,
                &gateway.name,
                Some(&dep.name),
                "the gateway has no driver dependency",
            ));
        }
    }

    for core in select(
        graph,
        RULE_CORE_IO,
        "core crate",
        |n| n == "a2a-core",
        &mut out,
    ) {
        for dep in normal_deps(core) {
            let name = dep.name.as_str();
            if CORE_ALLOWED.contains(&name) {
                continue;
            }
            if IO_CRATES.contains(&name) {
                out.push(Violation::new(
                    RULE_CORE_IO,
                    &core.name,
                    Some(name),
                    "an I/O crate; a2a-core reaches I/O only through bus-api",
                ));
            } else if members.contains(name) {
                out.push(Violation::new(
                    RULE_CORE_IO,
                    &core.name,
                    Some(name),
                    "a workspace crate outside a2a-core's allowed set {a2a-types, bus-api}",
                ));
            }
        }
    }

    check_layers(graph, &mut out);
    out
}

fn rank_of(name: &str) -> Option<u8> {
    LAYERS.iter().find(|(n, _)| *n == name).map(|(_, r)| *r)
}

fn check_layers(graph: &Graph, out: &mut Vec<Violation>) {
    let subjects = select(
        graph,
        RULE_LAYER,
        "layered crates",
        |n| rank_of(n).is_some(),
        out,
    );
    if subjects.is_empty() {
        return;
    }
    for package in &graph.packages {
        let Some(rank) = rank_of(&package.name) else {
            if !TEST_CRATES.contains(&package.name.as_str()) {
                out.push(Violation::new(
                    RULE_LAYER,
                    &package.name,
                    None,
                    "crate has no layer in the RFC-0001 4.2 table",
                ));
            }
            continue;
        };
        for dep in normal_deps(package) {
            let Some(dep_rank) = rank_of(&dep.name) else {
                continue;
            };
            if dep_rank < rank {
                continue;
            }
            let excepted = LAYER_EXCEPTIONS
                .iter()
                .any(|(from, to, _)| *from == package.name && *to == dep.name);
            if !excepted {
                out.push(Violation::new(
                    RULE_LAYER,
                    &package.name,
                    Some(&dep.name),
                    "depends on a crate in the same or a higher layer (RFC-0001 4.1)",
                ));
            }
        }
    }
    for (from, to, reason) in LAYER_EXCEPTIONS {
        if !graph.has_normal_edge(from, to) {
            out.push(Violation::new(
                RULE_LAYER,
                *from,
                Some(to),
                format!("stale layer exception ({reason}); the edge is gone, remove the exception"),
            ));
        }
    }
}

/// Checks the file rules: [0001-005] (AGENTS.md sections and the CLAUDE.md shim) and
/// [0001-006] (one trait per seam, in its owner). Reads each crate directory.
#[must_use]
pub fn check_crate_files(graph: &Graph, shim: &str) -> Vec<Violation> {
    let mut out = Vec::new();
    let subjects = select(
        graph,
        RULE_CRATE_FILES,
        "workspace crates",
        |_| true,
        &mut out,
    );
    let shim = normalise(shim);
    for package in subjects {
        check_agents_files(package, &shim, &mut out);
    }
    check_seams(graph, &mut out);
    out
}

fn check_agents_files(package: &Package, shim: &str, out: &mut Vec<Violation>) {
    let agents = package.dir.join("AGENTS.md");
    match fs::read_to_string(&agents) {
        Err(e) => out.push(Violation::new(
            RULE_CRATE_FILES,
            &package.name,
            None,
            format!("AGENTS.md unreadable at {}: {e}", agents.display()),
        )),
        Ok(text) => {
            for heading in ["Owns", "Provides", "Uses"] {
                if !has_section(&text, heading) {
                    out.push(Violation::new(
                        RULE_CRATE_FILES,
                        &package.name,
                        None,
                        format!("AGENTS.md has no `{heading}` section"),
                    ));
                }
            }
        }
    }
    let claude = package.dir.join("CLAUDE.md");
    match fs::read_to_string(&claude) {
        Err(e) => out.push(Violation::new(
            RULE_CRATE_FILES,
            &package.name,
            None,
            format!("CLAUDE.md unreadable at {}: {e}", claude.display()),
        )),
        Ok(text) => {
            if normalise(&text) != shim {
                out.push(Violation::new(
                    RULE_CRATE_FILES,
                    &package.name,
                    None,
                    "CLAUDE.md is not the crates/CLAUDE.md shim",
                ));
            }
        }
    }
}

/// True when a line starts with `word` (case-insensitive) after heading or emphasis marks and
/// the word is not followed by more letters.
fn has_section(text: &str, word: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line
            .trim()
            .trim_start_matches(|c: char| c == '#' || c == '*' || c.is_whitespace());
        trimmed
            .get(..word.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(word))
            && !trimmed[word.len()..]
                .chars()
                .next()
                .is_some_and(char::is_alphanumeric)
    })
}

fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n")
}

fn check_seams(graph: &Graph, out: &mut Vec<Violation>) {
    let mut traits: Vec<(&Package, Vec<String>)> = Vec::new();
    for package in &graph.packages {
        match rust_sources(&package.dir.join("src")) {
            Err(e) => out.push(Violation::new(
                RULE_SEAMS,
                &package.name,
                None,
                format!("cannot read sources: {e}"),
            )),
            Ok(files) => {
                let mut names = Vec::new();
                for file in files {
                    match fs::read_to_string(&file) {
                        Ok(text) => names.extend(public_traits(&text)),
                        Err(e) => out.push(Violation::new(
                            RULE_SEAMS,
                            &package.name,
                            None,
                            format!("cannot read {}: {e}", file.display()),
                        )),
                    }
                }
                traits.push((package, names));
            }
        }
    }
    for (seam, owner) in SEAMS {
        let defining: Vec<&str> = traits
            .iter()
            .filter(|(_, names)| names.iter().any(|n| n == seam))
            .map(|(p, _)| p.name.as_str())
            .collect();
        if !defining.contains(owner) {
            out.push(Violation::new(
                RULE_SEAMS,
                *owner,
                Some(seam),
                format!("seam trait `{seam}` is not defined in its owner crate"),
            ));
        }
        for other in defining.iter().filter(|c| *c != owner) {
            out.push(Violation::new(
                RULE_SEAMS,
                *other,
                Some(seam),
                format!("defines `pub trait {seam}`; each seam has one trait, in `{owner}`"),
            ));
        }
    }
}

fn rust_sources(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn public_traits(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("pub trait ")?;
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then_some(name)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const NAMES: &[&str] = &[
        "a2a-types",
        "bus-api",
        "bus-mem",
        "bus-nats",
        "a2a-core",
        "a2a-security",
        "a2a-workspace",
        "a2a-audit",
        "a2a-mcp",
        "a2a-worker",
        "a2a-client",
        "driver-acp",
        "driver-a2a-http",
        "gateway-http",
        "a2ad",
        "a2a-tui",
        "a2a-cli",
        "a2a-conformance",
    ];

    fn dep(name: &str, kind: DepKind) -> Dependency {
        Dependency {
            name: name.to_owned(),
            kind,
        }
    }

    /// A clean synthetic graph: the real edges that RFC-0001 allows, plus the named exception.
    fn clean() -> Graph {
        use DepKind::{Dev, Normal};
        let edges: &[(&str, &[(&str, DepKind)])] = &[
            ("a2a-types", &[]),
            ("bus-api", &[("a2a-types", Normal)]),
            ("bus-mem", &[("bus-api", Normal), ("a2a-conformance", Dev)]),
            ("bus-nats", &[("bus-api", Normal)]),
            (
                "a2a-core",
                &[("a2a-types", Normal), ("bus-api", Normal), ("bus-mem", Dev)],
            ),
            ("a2a-security", &[("a2a-types", Normal)]),
            ("a2a-workspace", &[("a2a-types", Dev)]),
            ("a2a-audit", &[("a2a-types", Normal)]),
            ("a2a-mcp", &[]),
            (
                "a2a-worker",
                &[
                    ("a2a-core", Normal),
                    ("a2a-security", Normal),
                    ("a2a-audit", Normal),
                    ("a2a-mcp", Normal),
                    ("a2a-workspace", Normal),
                    ("a2a-client", Normal),
                    ("bus-api", Normal),
                    ("bus-mem", Dev),
                ],
            ),
            (
                "a2a-client",
                &[
                    ("a2a-core", Normal),
                    ("a2a-security", Normal),
                    ("a2a-types", Normal),
                    ("bus-api", Normal),
                    ("bus-mem", Dev),
                ],
            ),
            ("driver-acp", &[("a2a-worker", Normal)]),
            (
                "driver-a2a-http",
                &[("a2a-worker", Normal), ("a2a-types", Normal)],
            ),
            (
                "gateway-http",
                &[
                    ("a2a-client", Normal),
                    ("a2a-core", Normal),
                    ("bus-api", Normal),
                    ("bus-mem", Dev),
                ],
            ),
            (
                "a2ad",
                &[
                    ("a2a-worker", Normal),
                    ("a2a-client", Normal),
                    ("bus-nats", Normal),
                    ("bus-mem", Dev),
                    ("driver-acp", Normal),
                    ("driver-a2a-http", Normal),
                    ("gateway-http", Normal),
                ],
            ),
            ("a2a-tui", &[("a2a-client", Normal), ("bus-nats", Normal)]),
            (
                "a2a-cli",
                &[
                    ("a2a-client", Normal),
                    ("bus-nats", Normal),
                    ("bus-mem", Dev),
                ],
            ),
            (
                "a2a-conformance",
                &[
                    ("bus-mem", Normal),
                    ("a2a-worker", Normal),
                    ("gateway-http", Normal),
                    ("bus-api", Normal),
                ],
            ),
        ];
        Graph {
            packages: edges
                .iter()
                .map(|(name, deps)| Package {
                    name: (*name).to_owned(),
                    dir: PathBuf::from("crates").join(name),
                    dependencies: deps.iter().map(|(n, k)| dep(n, *k)).collect(),
                })
                .collect(),
        }
    }

    fn with_dep(mut graph: Graph, from: &str, to: &str, kind: DepKind) -> Graph {
        let package = graph
            .packages
            .iter_mut()
            .find(|p| p.name == from)
            .expect("fixture crate");
        package.dependencies.push(dep(to, kind));
        graph
    }

    fn without_dep(mut graph: Graph, from: &str, to: &str) -> Graph {
        let package = graph
            .packages
            .iter_mut()
            .find(|p| p.name == from)
            .expect("fixture crate");
        package.dependencies.retain(|d| d.name != to);
        graph
    }

    fn rules(violations: &[Violation]) -> Vec<&'static str> {
        violations.iter().map(|v| v.rule).collect()
    }

    fn only(violations: &[Violation], rule: &str, krate: &str, dependency: &str) -> bool {
        violations.iter().any(|v| {
            v.rule == rule && v.krate == krate && v.dependency.as_deref() == Some(dependency)
        })
    }

    #[test]
    fn req_0001_all_fixture_names_are_workspace_members() {
        let graph = clean();
        let names: Vec<&str> = graph.packages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, NAMES);
        assert!(check_dependency_rules(&graph).is_empty());
    }

    #[test]
    fn req_0001_metadata_keeps_members_and_kinds() {
        let json = serde_json::json!({
            "workspace_members": ["path+file:///r/crates/a2a-core#0.1.0"],
            "packages": [
                {
                    "id": "path+file:///r/crates/a2a-core#0.1.0",
                    "name": "a2a-core",
                    "manifest_path": "/r/crates/a2a-core/Cargo.toml",
                    "dependencies": [
                        {"name": "a2a-types", "kind": null},
                        {"name": "bus-mem", "kind": "dev"},
                        {"name": "cc", "kind": "build"}
                    ]
                },
                {
                    "id": "registry+https://x#tokio@1.0.0",
                    "name": "tokio",
                    "manifest_path": "/cargo/tokio/Cargo.toml",
                    "dependencies": []
                }
            ]
        })
        .to_string();
        let graph = Graph::from_metadata(&json).unwrap();
        assert_eq!(graph.packages.len(), 1);
        let kinds: Vec<DepKind> = graph.packages[0]
            .dependencies
            .iter()
            .map(|d| d.kind)
            .collect();
        assert_eq!(kinds, [DepKind::Normal, DepKind::Dev, DepKind::Build]);
        assert_eq!(graph.packages[0].dir, PathBuf::from("/r/crates/a2a-core"));
    }

    #[test]
    fn req_0001_metadata_rejects_malformed_input() {
        assert!(Graph::from_metadata("{").is_err());
        assert!(Graph::from_metadata("{\"packages\": []}").is_err());
    }

    #[test]
    fn req_0001_001_bus_binding_in_core_is_refused() {
        let bad = with_dep(clean(), "a2a-core", "bus-mem", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(only(&v, RULE_BINDINGS, "a2a-core", "bus-mem"), "{v:?}");
    }

    #[test]
    fn req_0001_001_bus_nats_in_worker_is_refused_but_dev_edge_is_allowed() {
        let bad = with_dep(clean(), "a2a-worker", "bus-nats", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(only(&v, RULE_BINDINGS, "a2a-worker", "bus-nats"), "{v:?}");
        let dev = with_dep(clean(), "a2a-worker", "bus-nats", DepKind::Dev);
        assert!(check_dependency_rules(&dev).is_empty());
    }

    #[test]
    fn req_0001_001_binding_subjects_must_exist() {
        let mut graph = clean();
        graph
            .packages
            .retain(|p| p.name != "bus-nats" && p.name != "bus-mem");
        let v = check_dependency_rules(&graph);
        assert!(
            v.iter()
                .any(|x| x.rule == RULE_BINDINGS && x.krate == "bus binding crates")
        );
    }

    #[test]
    fn req_0001_002_driver_to_driver_is_refused() {
        let bad = with_dep(clean(), "driver-acp", "driver-a2a-http", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(
            only(&v, RULE_DRIVERS, "driver-acp", "driver-a2a-http"),
            "{v:?}"
        );
    }

    #[test]
    fn req_0001_002_driver_to_gateway_or_bus_is_refused() {
        let gw = with_dep(clean(), "driver-a2a-http", "gateway-http", DepKind::Normal);
        assert!(only(
            &check_dependency_rules(&gw),
            RULE_DRIVERS,
            "driver-a2a-http",
            "gateway-http"
        ));
        let bus = with_dep(clean(), "driver-acp", "bus-mem", DepKind::Normal);
        assert!(only(
            &check_dependency_rules(&bus),
            RULE_DRIVERS,
            "driver-acp",
            "bus-mem"
        ));
    }

    #[test]
    fn req_0001_002_driver_subjects_must_exist() {
        let mut graph = clean();
        graph.packages.retain(|p| !is_driver(&p.name));
        let v = check_dependency_rules(&graph);
        assert!(
            v.iter()
                .any(|x| x.rule == RULE_DRIVERS && x.krate == "driver crates")
        );
    }

    #[test]
    fn req_0001_003_gateway_to_driver_is_refused() {
        let bad = with_dep(clean(), "gateway-http", "driver-acp", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(
            only(&v, RULE_GATEWAY, "gateway-http", "driver-acp"),
            "{v:?}"
        );
    }

    #[test]
    fn req_0001_004_core_io_crate_is_refused() {
        let bad = with_dep(clean(), "a2a-core", "tokio", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(only(&v, RULE_CORE_IO, "a2a-core", "tokio"), "{v:?}");
    }

    #[test]
    fn req_0001_004_core_workspace_dependency_is_refused() {
        let bad = with_dep(clean(), "a2a-core", "a2a-security", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(only(&v, RULE_CORE_IO, "a2a-core", "a2a-security"), "{v:?}");
    }

    #[test]
    fn req_0001_004_core_tokio_dev_dependency_is_allowed() {
        let dev = with_dep(clean(), "a2a-core", "tokio", DepKind::Dev);
        assert!(check_dependency_rules(&dev).is_empty());
    }

    #[test]
    fn req_0001_s4_2_upward_edge_is_refused() {
        let bad = with_dep(clean(), "a2a-client", "a2a-worker", DepKind::Normal);
        let v = check_dependency_rules(&bad);
        assert!(only(&v, RULE_LAYER, "a2a-client", "a2a-worker"), "{v:?}");
    }

    #[test]
    fn req_0001_s4_2_worker_client_exception_is_accepted_then_stale_when_removed() {
        assert!(check_dependency_rules(&clean()).is_empty());
        let stale = without_dep(clean(), "a2a-worker", "a2a-client");
        let v = check_dependency_rules(&stale);
        assert!(only(&v, RULE_LAYER, "a2a-worker", "a2a-client"), "{v:?}");
        assert!(v.iter().any(|x| x.detail.contains("stale layer exception")));
    }

    #[test]
    fn req_0001_s4_2_unlisted_crate_is_refused() {
        let mut graph = clean();
        graph.packages.push(Package {
            name: "new-crate".to_owned(),
            dir: PathBuf::from("crates/new-crate"),
            dependencies: Vec::new(),
        });
        let v = check_dependency_rules(&graph);
        assert!(
            v.iter()
                .any(|x| x.rule == RULE_LAYER && x.krate == "new-crate")
        );
    }

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("a2a-boundary-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Writes one crate directory with the given AGENTS.md, CLAUDE.md and source files.
    fn write_crate(
        root: &Path,
        name: &str,
        agents: Option<&str>,
        claude: Option<&str>,
        src: &[(&str, &str)],
    ) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("src")).unwrap();
        if let Some(text) = agents {
            fs::write(dir.join("AGENTS.md"), text).unwrap();
        }
        if let Some(text) = claude {
            fs::write(dir.join("CLAUDE.md"), text).unwrap();
        }
        for (file, text) in src {
            fs::write(dir.join("src").join(file), text).unwrap();
        }
        dir
    }

    const SHIM: &str = "Claude Code reads this file.\n\n@AGENTS.md\n";
    const GOOD_AGENTS: &str = "# x\n\n## Owns\n\n- a\n\n## Provides\n\n- b\n\n## Uses\n\n- c\n";

    /// Graph whose directories point at `root`, one per name in `NAMES`.
    fn graph_in(root: &Path) -> Graph {
        let mut graph = clean();
        for package in &mut graph.packages {
            package.dir = root.join(&package.name);
        }
        graph
    }

    fn write_all_clean(root: &Path) {
        for name in NAMES {
            let mut body = String::from("pub fn f() {}\n");
            for &(seam, owner) in SEAMS {
                if owner == *name {
                    body.push_str(&format!("pub trait {seam} {{}}\n"));
                }
            }
            write_crate(
                root,
                name,
                Some(GOOD_AGENTS),
                Some(SHIM),
                &[("lib.rs", body.as_str())],
            );
        }
    }

    #[test]
    fn req_0001_005_clean_crate_files_pass() {
        let root = temp_root("clean");
        write_all_clean(&root);
        let graph = graph_in(&root);
        let v = check_crate_files(&graph, SHIM);
        assert!(v.is_empty(), "{v:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_005_missing_agents_is_refused() {
        let root = temp_root("noagents");
        write_all_clean(&root);
        fs::remove_file(root.join("a2a-core").join("AGENTS.md")).unwrap();
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(
            v.iter().any(|x| x.rule == RULE_CRATE_FILES
                && x.krate == "a2a-core"
                && x.detail.contains("AGENTS.md unreadable")),
            "{v:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_005_missing_section_is_refused() {
        let root = temp_root("nouses");
        write_all_clean(&root);
        fs::write(
            root.join("a2a-core").join("AGENTS.md"),
            "## Owns\n\n## Provides\n",
        )
        .unwrap();
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(
            v.iter().any(|x| x.rule == RULE_CRATE_FILES
                && x.krate == "a2a-core"
                && x.detail.contains("`Uses`")),
            "{v:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_005_shim_mismatch_is_refused() {
        let root = temp_root("shim");
        write_all_clean(&root);
        fs::write(
            root.join("a2a-types").join("CLAUDE.md"),
            "copy of AGENTS.md\n",
        )
        .unwrap();
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(
            v.iter().any(|x| x.rule == RULE_CRATE_FILES
                && x.krate == "a2a-types"
                && x.detail.contains("shim")),
            "{v:?}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_006_clean_seams_pass() {
        let root = temp_root("seams-clean");
        write_all_clean(&root);
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(v.iter().all(|x| x.rule != RULE_SEAMS), "{v:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_006_second_seam_trait_is_refused() {
        let root = temp_root("seams-dup");
        write_all_clean(&root);
        fs::write(
            root.join("a2a-client").join("src").join("lib.rs"),
            "pub trait Bus {}\n",
        )
        .unwrap();
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(only(&v, RULE_SEAMS, "a2a-client", "Bus"), "{v:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_006_missing_owner_seam_is_refused() {
        let root = temp_root("seams-missing");
        write_all_clean(&root);
        fs::write(
            root.join("a2a-audit").join("src").join("lib.rs"),
            "pub fn f() {}\n",
        )
        .unwrap();
        let v = check_crate_files(&graph_in(&root), SHIM);
        assert!(only(&v, RULE_SEAMS, "a2a-audit", "AuditSink"), "{v:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn req_0001_006_public_trait_scan_reads_names() {
        let names = public_traits(
            "pub trait Bus: Send {}\n  pub trait Verifier {}\nfn x() {}\npub(crate) trait Hidden {}\n",
        );
        assert_eq!(names, ["Bus", "Verifier"]);
    }

    #[test]
    fn req_0001_mutation_names_each_rule_it_breaks() {
        let bad = with_dep(clean(), "a2a-core", "bus-mem", DepKind::Normal);
        let ids = rules(&check_dependency_rules(&bad));
        assert_eq!(ids, [RULE_BINDINGS, RULE_CORE_IO]);
    }

    #[test]
    fn req_0001_real_workspace_satisfies_every_rule() {
        let root = workspace_root();
        let graph = load_workspace(&root).unwrap();
        for name in NAMES {
            assert!(graph.package(name).is_some(), "workspace lacks {name}");
        }
        let violations = check_workspace(&root).unwrap();
        assert!(
            violations.is_empty(),
            "boundary violations:\n{}",
            violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
