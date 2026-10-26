//! Approval request lifecycle (RFC-0006 sections 4.2 to 4.4).
//!
//! [`ApprovalBook`] is a pure state machine. Every method receives the current
//! time in milliseconds and returns the transitions the owner Worker must
//! publish, answer to the backend, or audit. Nothing here performs I/O.
//!
//! Requests are created with exactly the two once options (0006-009, 0006-010,
//! 0006-057). A request closes after its first honored decision (0006-015) and
//! never reopens.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;

use a2a_types::proto::a2abus::{
    ApprovalDecision, ApprovalOption, ApprovalOptionKind, ApprovalRequest, ApprovalTier,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Option id of the once-allow option. Answered to the backend by kind (0006-055).
pub const ALLOW_ONCE_ID: &str = "allow_once";
/// Option id of the once-reject option.
pub const REJECT_ONCE_ID: &str = "reject_once";
/// Default expiry after creation (0006-006).
pub const DEFAULT_EXPIRY_MS: u64 = 300_000;
/// Lowest configurable expiry (0006-006).
pub const MIN_EXPIRY_MS: u64 = 30_000;
/// Highest configurable expiry (0006-006).
pub const MAX_EXPIRY_MS: u64 = 3_600_000;
/// Summary length bound in characters (0006-008).
pub const SUMMARY_MAX_CHARS: usize = 512;
/// Window in which an identical call joins an open request (0006-021).
pub const COALESCE_MS: u64 = 30_000;
/// Open requests allowed per task before automatic refusal (0006-022).
pub const MAX_OPEN_PER_TASK: usize = 3;
/// Decisions per minute per operator identity and tenant (0006-023).
pub const DECISIONS_PER_MINUTE: usize = 10;
/// Minimum nonce size: 128 random bits (0006-005).
pub const NONCE_MIN_BYTES: usize = 16;

const MINUTE_MS: u64 = 60_000;

/// Risk tier of a tool call (0006-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    Read,
    Write,
    Exec,
    DeleteOrMove,
}

impl Tier {
    /// Wire value of `ApprovalTier`.
    #[must_use]
    pub fn to_proto(self) -> i32 {
        match self {
            Self::Read => ApprovalTier::Read as i32,
            Self::Write => ApprovalTier::Write as i32,
            Self::Exec => ApprovalTier::Exec as i32,
            Self::DeleteOrMove => ApprovalTier::DeleteOrMove as i32,
        }
    }
}

/// Assigns the tier of a tool call (0006-002): an unknown tier becomes `exec`,
/// and an MCP tool is at least `write`.
#[must_use]
pub fn assign_tier(known: Option<Tier>, mcp: bool) -> Tier {
    let tier = known.unwrap_or(Tier::Exec);
    if mcp { tier.max(Tier::Write) } else { tier }
}

/// Lifecycle state of one approval request (0006-013, 0006-017, 0006-018).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestState {
    Open,
    Allowed,
    Denied,
    Expired,
    Closed,
}

/// Answer the Worker owes the backend for a settled request (0006-019, 0006-020, 0006-055).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    Denied,
}

impl Verdict {
    /// Offered option id to select in the backend answer.
    #[must_use]
    pub fn option_id(self) -> &'static str {
        match self {
            Self::Allowed => ALLOW_ONCE_ID,
            Self::Denied => REJECT_ONCE_ID,
        }
    }
}

/// Input for opening a request. The Worker builds it from structured tool call
/// fields (0006-008); `summary` is bounded and stripped of control characters here.
#[derive(Debug, Clone)]
pub struct NewRequest {
    pub request_id: String,
    pub task_id: String,
    pub tool: String,
    pub tier: Tier,
    pub summary: String,
    pub requester: String,
    pub chain: Vec<String>,
    pub nonce: Vec<u8>,
    /// Expiry after creation in milliseconds; `None` selects [`DEFAULT_EXPIRY_MS`].
    pub expiry_ms: Option<u64>,
    /// Canonical working directory of the gated call (0006-006). Must be non-empty.
    pub working_directory: String,
    /// Owner workspace mode: `per-session` or `per-task` (0006-006, RFC-0012).
    pub workspace_mode: String,
}

/// Result of opening a request.
#[derive(Debug, Clone)]
pub enum Opened {
    /// A new open request. The caller commits it before publishing (0006-005);
    /// `digest` is the value the owner stores for 0006-051.
    Created {
        request: Box<ApprovalRequest>,
        digest: String,
    },
    /// An identical call joined this open request (0006-021).
    Joined { request_id: String },
    /// Per-task cap reached (0006-022). The caller answers the call with the
    /// reject option and audits `APPROVAL_FLOOD`; the task does not fail.
    Flood,
}

/// Settled decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub request_id: String,
    pub task_id: String,
    pub verdict: Verdict,
    /// True for a duplicate of the honored decision, which changes no state (0006-015).
    pub replayed: bool,
}

/// A request closed without a decision. The caller answers the backend `cancelled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ended {
    pub request_id: String,
    pub task_id: String,
    /// [`RequestState::Expired`] (0006-017) or [`RequestState::Closed`] (0006-018, 0006-056).
    pub state: RequestState,
}

/// Invalid request input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    #[error("request id and task id must be non-empty")]
    Identity,
    #[error("chain must be non-empty and end with the requester (0006-011)")]
    Chain,
    #[error("nonce must carry at least 128 random bits (0006-005)")]
    Nonce,
    #[error("expiry must be between 30 and 3600 seconds (0006-006)")]
    Expiry,
    #[error("request id already exists")]
    Duplicate,
    #[error(
        "working directory must be non-empty and workspace mode per-session or per-task (0006-006)"
    )]
    Context,
}

/// Refused decision. Each refusal changes no state (0006-013, 0006-023).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("APPROVAL_NOT_APPLICABLE")]
    NotApplicable,
    #[error("APPROVAL_ALREADY_RESOLVED")]
    AlreadyResolved,
    #[error("sender is not an operator of the task tenant (0006-014)")]
    NotOperator,
    #[error("decision rate exceeded (0006-023)")]
    RateLimited,
}

impl Refusal {
    /// `ErrorInfo.metadata.detail` of the refusal (0006-013, 0006-015, 0006-023).
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::NotApplicable | Self::NotOperator => "APPROVAL_NOT_APPLICABLE",
            Self::AlreadyResolved => "APPROVAL_ALREADY_RESOLVED",
            Self::RateLimited => "RATE_LIMITED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    request_id: String,
    task_id: String,
    tool: String,
    tier: Tier,
    summary: String,
    requester: String,
    chain: Vec<String>,
    working_directory: String,
    workspace_mode: String,
    nonce: String,
    created_ms: u64,
    expires_ms: u64,
    digest: String,
    state: RequestState,
    option_id: Option<String>,
    decided_by: Option<String>,
    decided_ms: Option<u64>,
}

/// Sliding one-minute window of accepted decisions per tenant and operator.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RateLimit {
    windows: BTreeMap<String, BTreeMap<String, VecDeque<u64>>>,
}

impl RateLimit {
    fn admit(&mut self, tenant: &str, operator: &str, now_ms: u64) -> bool {
        let window = self
            .windows
            .entry(tenant.to_owned())
            .or_default()
            .entry(operator.to_owned())
            .or_default();
        while let Some(&at) = window.front() {
            if now_ms.saturating_sub(at) < MINUTE_MS {
                break;
            }
            window.pop_front();
        }
        if window.len() >= DECISIONS_PER_MINUTE {
            return false;
        }
        window.push_back(now_ms);
        true
    }
}

/// Approval requests of one owner Worker.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalBook {
    records: BTreeMap<String, Record>,
    rate: RateLimit,
}

impl ApprovalBook {
    /// Opens a request, joins an identical open one, or reports the flood cap.
    ///
    /// Coalescing is checked before the flood cap, so a joined call never counts
    /// toward the cap (0006-021, 0006-022).
    pub fn open(&mut self, spec: NewRequest, now_ms: u64) -> Result<Opened, OpenError> {
        if spec.request_id.is_empty() || spec.task_id.is_empty() {
            return Err(OpenError::Identity);
        }
        if spec.chain.last().map(String::as_str) != Some(spec.requester.as_str()) {
            return Err(OpenError::Chain);
        }
        if spec.nonce.len() < NONCE_MIN_BYTES {
            return Err(OpenError::Nonce);
        }
        let expiry_ms = spec.expiry_ms.unwrap_or(DEFAULT_EXPIRY_MS);
        if !(MIN_EXPIRY_MS..=MAX_EXPIRY_MS).contains(&expiry_ms) {
            return Err(OpenError::Expiry);
        }
        if spec.working_directory.is_empty()
            || !matches!(spec.workspace_mode.as_str(), "per-session" | "per-task")
        {
            return Err(OpenError::Context);
        }
        let summary = bound_summary(&spec.summary);
        let joinable = self.records.values().find(|r| {
            r.state == RequestState::Open
                && r.task_id == spec.task_id
                && r.tool == spec.tool
                && r.working_directory == spec.working_directory
                && r.workspace_mode == spec.workspace_mode
                && r.tier == spec.tier
                && r.summary == summary
                && now_ms.saturating_sub(r.created_ms) < COALESCE_MS
        });
        if self.records.contains_key(&spec.request_id) {
            return Err(OpenError::Duplicate);
        }
        if let Some(existing) = joinable {
            return Ok(Opened::Joined {
                request_id: existing.request_id.clone(),
            });
        }
        if self.open_for_task(&spec.task_id) >= MAX_OPEN_PER_TASK {
            return Ok(Opened::Flood);
        }
        let mut record = Record {
            request_id: spec.request_id,
            task_id: spec.task_id,
            tool: spec.tool,
            tier: spec.tier,
            summary,
            requester: spec.requester,
            chain: spec.chain,
            working_directory: spec.working_directory,
            workspace_mode: spec.workspace_mode,
            nonce: to_hex(&spec.nonce),
            created_ms: now_ms,
            expires_ms: now_ms.saturating_add(expiry_ms),
            digest: String::new(),
            state: RequestState::Open,
            option_id: None,
            decided_by: None,
            decided_ms: None,
        };
        let request = build_request(&record);
        record.digest = digest_of(&request);
        let digest = record.digest.clone();
        self.records.insert(record.request_id.clone(), record);
        Ok(Opened::Created {
            request: Box::new(request),
            digest,
        })
    }

    /// Applies an operator decision (0006-012 to 0006-015, 0006-023, 0006-051).
    ///
    /// `verified_operator` is the identity the signature check established, or
    /// `None` for a sender that is not an operator. The caller has already
    /// verified the envelope signature when signing is enabled (0006-014).
    /// Checks run in the order of 0006-013 after the authority and rate gates.
    pub fn decide(
        &mut self,
        decision: &ApprovalDecision,
        verified_operator: Option<&str>,
        tenant: &str,
        now_ms: u64,
    ) -> Result<Settled, Refusal> {
        let operator = verified_operator.ok_or(Refusal::NotOperator)?;
        if !decision.decided_by.is_empty() && decision.decided_by != operator {
            return Err(Refusal::NotOperator);
        }
        if !self.rate.admit(tenant, operator, now_ms) {
            return Err(Refusal::RateLimited);
        }
        let record = self
            .records
            .get_mut(&decision.request_id)
            .ok_or(Refusal::NotApplicable)?;
        if record.task_id != decision.task_id || record.digest != decision.request_sha256 {
            return Err(Refusal::NotApplicable);
        }
        match record.state {
            RequestState::Open => {}
            RequestState::Allowed | RequestState::Denied => {
                if record.nonce != decision.nonce {
                    return Err(Refusal::NotApplicable);
                }
                if record.option_id.as_deref() == Some(decision.option_id.as_str()) {
                    let verdict = if record.state == RequestState::Allowed {
                        Verdict::Allowed
                    } else {
                        Verdict::Denied
                    };
                    return Ok(Settled {
                        request_id: record.request_id.clone(),
                        task_id: record.task_id.clone(),
                        verdict,
                        replayed: true,
                    });
                }
                return Err(Refusal::AlreadyResolved);
            }
            RequestState::Expired | RequestState::Closed => return Err(Refusal::NotApplicable),
        }
        let (state, verdict) = match decision.option_id.as_str() {
            ALLOW_ONCE_ID => (RequestState::Allowed, Verdict::Allowed),
            REJECT_ONCE_ID => (RequestState::Denied, Verdict::Denied),
            _ => return Err(Refusal::NotApplicable),
        };
        if decision.nonce != record.nonce || now_ms >= record.expires_ms {
            return Err(Refusal::NotApplicable);
        }
        record.state = state;
        record.option_id = Some(decision.option_id.clone());
        record.decided_by = Some(operator.to_owned());
        record.decided_ms = Some(now_ms);
        Ok(Settled {
            request_id: record.request_id.clone(),
            task_id: record.task_id.clone(),
            verdict,
            replayed: false,
        })
    }

    /// Closes every open request whose expiry has passed (0006-017).
    pub fn expire_due(&mut self, now_ms: u64) -> Vec<Ended> {
        self.close_where(|r| now_ms >= r.expires_ms, RequestState::Expired)
    }

    /// Closes every open request of a task that was cancelled (0006-018).
    pub fn close_task(&mut self, task_id: &str) -> Vec<Ended> {
        self.close_where(|r| r.task_id == task_id, RequestState::Closed)
    }

    /// Closes every open request when the backend exits or restarts (0006-056).
    pub fn close_backend(&mut self) -> Vec<Ended> {
        self.close_where(|_| true, RequestState::Closed)
    }

    /// Earliest expiry among open requests, for the owner's timer.
    #[must_use]
    pub fn next_expiry(&self) -> Option<u64> {
        self.records
            .values()
            .filter(|r| r.state == RequestState::Open)
            .map(|r| r.expires_ms)
            .min()
    }

    /// Number of open requests of a task.
    #[must_use]
    pub fn open_for_task(&self, task_id: &str) -> usize {
        self.records
            .values()
            .filter(|r| r.state == RequestState::Open && r.task_id == task_id)
            .count()
    }

    /// Whether a request of a task has expired (0006-017). The expired record
    /// stays in the book, so the marker holds until the owner commits the timeout.
    #[must_use]
    pub fn expired_for_task(&self, task_id: &str) -> bool {
        self.records
            .values()
            .any(|r| r.state == RequestState::Expired && r.task_id == task_id)
    }

    /// Current state of a request.
    #[must_use]
    pub fn state(&self, request_id: &str) -> Option<RequestState> {
        self.records.get(request_id).map(|r| r.state)
    }

    /// Rebuilds the emitted `ApprovalRequest` for a request.
    #[must_use]
    pub fn request(&self, request_id: &str) -> Option<ApprovalRequest> {
        self.records.get(request_id).map(build_request)
    }

    /// Stored digest of the emitted request (0006-051).
    #[must_use]
    pub fn digest(&self, request_id: &str) -> Option<&str> {
        self.records.get(request_id).map(|r| r.digest.as_str())
    }

    /// Operator that settled the request.
    #[must_use]
    pub fn decided_by(&self, request_id: &str) -> Option<&str> {
        self.records
            .get(request_id)
            .and_then(|r| r.decided_by.as_deref())
    }

    /// Time of the honored decision.
    #[must_use]
    pub fn decided_ms(&self, request_id: &str) -> Option<u64> {
        self.records.get(request_id).and_then(|r| r.decided_ms)
    }

    fn close_where(&mut self, pred: impl Fn(&Record) -> bool, to: RequestState) -> Vec<Ended> {
        let mut ended = Vec::new();
        for record in self.records.values_mut() {
            if record.state == RequestState::Open && pred(record) {
                record.state = to;
                ended.push(Ended {
                    request_id: record.request_id.clone(),
                    task_id: record.task_id.clone(),
                    state: to,
                });
            }
        }
        ended
    }
}

/// Strips control characters and truncates to [`SUMMARY_MAX_CHARS`] (0006-008).
#[must_use]
pub fn bound_summary(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(SUMMARY_MAX_CHARS)
        .collect()
}

fn build_request(r: &Record) -> ApprovalRequest {
    ApprovalRequest {
        request_id: r.request_id.clone(),
        task_id: r.task_id.clone(),
        tool: r.tool.clone(),
        tier: r.tier.to_proto(),
        summary: r.summary.clone(),
        requester: r.requester.clone(),
        chain: r.chain.clone(),
        working_directory: r.working_directory.clone(),
        workspace_mode: r.workspace_mode.clone(),
        options: vec![
            ApprovalOption {
                option_id: ALLOW_ONCE_ID.to_owned(),
                name: "Allow once".to_owned(),
                kind: ApprovalOptionKind::AllowOnce as i32,
            },
            ApprovalOption {
                option_id: REJECT_ONCE_ID.to_owned(),
                name: "Reject once".to_owned(),
                kind: ApprovalOptionKind::RejectOnce as i32,
            },
        ],
        expires_at: Some(timestamp_of(r.expires_ms)),
        nonce: r.nonce.clone(),
    }
}

fn timestamp_of(ms: u64) -> pbjson_types::Timestamp {
    pbjson_types::Timestamp {
        seconds: i64::try_from(ms / 1000).unwrap_or(i64::MAX),
        nanos: i32::try_from((ms % 1000) * 1_000_000).unwrap_or(0),
    }
}

/// Lowercase hex SHA-256 of the protobuf binary encoding (0006-051).
fn digest_of(request: &ApprovalRequest) -> String {
    to_hex(&Sha256::digest(request.encode_to_vec()))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;
    const OPERATOR: &str = "operator:alice";
    const TENANT: &str = "tenant-a";

    fn spec(id: &str) -> NewRequest {
        NewRequest {
            request_id: id.to_owned(),
            task_id: "task-1".to_owned(),
            tool: "bash".to_owned(),
            tier: Tier::Exec,
            summary: "run cargo test".to_owned(),
            requester: "claude".to_owned(),
            chain: vec!["omp".to_owned(), "claude".to_owned()],
            nonce: id.bytes().cycle().take(NONCE_MIN_BYTES).collect(),
            expiry_ms: None,
            working_directory: "/work/project".to_owned(),
            workspace_mode: "per-task".to_owned(),
        }
    }

    fn created(opened: Opened) -> Option<(ApprovalRequest, String)> {
        match opened {
            Opened::Created { request, digest } => Some((*request, digest)),
            Opened::Joined { .. } | Opened::Flood => None,
        }
    }

    fn open_one(book: &mut ApprovalBook, id: &str, now: u64) -> (ApprovalRequest, String) {
        created(book.open(spec(id), now).expect("open")).expect("created")
    }

    fn decision(req: &ApprovalRequest, digest: &str, option: &str, by: &str) -> ApprovalDecision {
        ApprovalDecision {
            request_id: req.request_id.clone(),
            task_id: req.task_id.clone(),
            option_id: option.to_owned(),
            nonce: req.nonce.clone(),
            decided_by: by.to_owned(),
            decided_at: None,
            request_sha256: digest.to_owned(),
        }
    }

    #[test]
    fn req_0006_002_unknown_tier_is_exec_and_mcp_is_at_least_write() {
        assert_eq!(assign_tier(None, false), Tier::Exec);
        assert_eq!(assign_tier(None, true), Tier::Exec);
        assert_eq!(assign_tier(Some(Tier::Read), true), Tier::Write);
        assert_eq!(assign_tier(Some(Tier::Read), false), Tier::Read);
        assert_eq!(
            assign_tier(Some(Tier::DeleteOrMove), true),
            Tier::DeleteOrMove
        );
    }

    #[test]
    fn req_0006_005_nonce_has_at_least_128_bits_and_duplicate_id_is_refused() {
        let mut book = ApprovalBook::default();
        let mut short = spec("r1");
        short.nonce = vec![1; NONCE_MIN_BYTES - 1];
        assert_eq!(book.open(short, T0).err(), Some(OpenError::Nonce));
        let (req, _) = open_one(&mut book, "r1", T0);
        assert_eq!(req.nonce.len(), NONCE_MIN_BYTES * 2);
        assert_eq!(book.open(spec("r1"), T0).err(), Some(OpenError::Duplicate));
    }

    #[test]
    fn req_0006_006_request_carries_all_fields_and_bounded_expiry() {
        let mut book = ApprovalBook::default();
        let (req, _) = open_one(&mut book, "r1", T0);
        assert_eq!(req.request_id, "r1");
        assert_eq!(req.task_id, "task-1");
        assert_eq!(req.tool, "bash");
        assert_eq!(req.tier, Tier::Exec.to_proto());
        assert_eq!(req.summary, "run cargo test");
        assert_eq!(req.requester, "claude");
        assert_eq!(req.chain, vec!["omp".to_owned(), "claude".to_owned()]);
        assert_eq!(req.options.len(), 2);
        let expires = req.expires_at.expect("expiry set");
        assert_eq!(expires.seconds, 1300);
        assert_eq!(expires.nanos, 0);

        for (id, expiry, ok) in [
            ("e1", 29_999, false),
            ("e2", 30_000, true),
            ("e3", 3_600_000, true),
            ("e4", 3_600_001, false),
        ] {
            let mut s = spec(id);
            s.expiry_ms = Some(expiry);
            assert_eq!(book.open(s, T0).is_ok(), ok, "expiry {expiry}");
        }
    }
    #[test]
    fn req_0006_006_request_carries_working_directory_and_mode() {
        let mut book = ApprovalBook::default();
        let (req, _) = open_one(&mut book, "wd1", T0);
        assert_eq!(req.working_directory, "/work/project");
        assert_eq!(req.workspace_mode, "per-task");
        for (id, dir, mode) in [
            ("wd2", "", "per-task"),
            ("wd3", "/w", "per-run"),
            ("wd4", "/w", ""),
        ] {
            let mut s = spec(id);
            s.working_directory = dir.to_owned();
            s.workspace_mode = mode.to_owned();
            assert!(
                matches!(book.open(s, T0), Err(OpenError::Context)),
                "{dir} {mode}"
            );
        }
    }

    #[test]
    fn req_0006_008_summary_is_bounded_and_control_free() {
        let mut book = ApprovalBook::default();
        let mut s = spec("r1");
        s.summary = format!("ls\u{7}\n{}", "x".repeat(600));
        let (req, _) = created(book.open(s, T0).expect("open")).expect("created");
        assert!(req.summary.chars().all(|c| !c.is_control()));
        assert_eq!(req.summary.chars().count(), SUMMARY_MAX_CHARS);
        assert!(req.summary.starts_with("ls"));
    }

    #[test]
    fn req_0006_009_options_always_include_allow_once_and_reject_once() {
        let mut book = ApprovalBook::default();
        let (req, _) = open_one(&mut book, "r1", T0);
        let ids: Vec<&str> = req.options.iter().map(|o| o.option_id.as_str()).collect();
        assert_eq!(ids, [ALLOW_ONCE_ID, REJECT_ONCE_ID]);
        assert_eq!(req.options[0].kind, ApprovalOptionKind::AllowOnce as i32);
        assert_eq!(req.options[1].kind, ApprovalOptionKind::RejectOnce as i32);
    }

    #[test]
    fn req_0006_010_and_057_no_always_or_cache_option_for_any_tier() {
        let mut book = ApprovalBook::default();
        for (i, tier) in [Tier::Read, Tier::Write, Tier::Exec, Tier::DeleteOrMove]
            .into_iter()
            .enumerate()
        {
            let mut s = spec(&format!("r{i}"));
            s.task_id = format!("t{i}");
            s.tier = tier;
            let (req, _) = created(book.open(s, T0).expect("open")).expect("created");
            for option in &req.options {
                assert!(
                    option.kind == ApprovalOptionKind::AllowOnce as i32
                        || option.kind == ApprovalOptionKind::RejectOnce as i32,
                    "tier {tier:?} offered {}",
                    option.option_id
                );
            }
        }
    }

    #[test]
    fn req_0006_011_chain_must_end_with_requester() {
        let mut book = ApprovalBook::default();
        let mut bad = spec("r1");
        bad.requester = "omp".to_owned();
        assert_eq!(book.open(bad, T0).err(), Some(OpenError::Chain));
        let mut empty = spec("r2");
        empty.chain.clear();
        assert_eq!(book.open(empty, T0).err(), Some(OpenError::Chain));
    }

    #[test]
    fn req_0006_013_each_check_refuses_without_state_change() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let expires = T0 + DEFAULT_EXPIRY_MS;

        let mut bad_nonce = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        bad_nonce.nonce = "00".repeat(NONCE_MIN_BYTES);
        assert_eq!(
            book.decide(&bad_nonce, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));

        let unknown = decision(&req, &digest, "allow_always", OPERATOR);
        assert_eq!(
            book.decide(&unknown, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));

        let at_expiry = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&at_expiry, Some(OPERATOR), TENANT, expires),
            Err(Refusal::NotApplicable)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));

        let not_operator = decision(&req, &digest, ALLOW_ONCE_ID, "");
        assert_eq!(
            book.decide(&not_operator, None, TENANT, T0 + 1),
            Err(Refusal::NotOperator)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));

        let ok = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        let settled = book
            .decide(&ok, Some(OPERATOR), TENANT, T0 + 1)
            .expect("all checks pass");
        assert_eq!(settled.verdict, Verdict::Allowed);
        assert!(!settled.replayed);
    }

    #[test]
    fn req_0006_013_request_not_open_is_refused() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        book.expire_due(T0 + DEFAULT_EXPIRY_MS);
        let d = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&d, Some(OPERATOR), TENANT, T0 + DEFAULT_EXPIRY_MS + 1),
            Err(Refusal::NotApplicable)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Expired));
    }

    #[test]
    fn req_0006_014_agent_identity_and_mismatched_decided_by_are_refused() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let as_agent = decision(&req, &digest, ALLOW_ONCE_ID, "claude");
        assert_eq!(
            book.decide(&as_agent, None, TENANT, T0 + 1),
            Err(Refusal::NotOperator)
        );
        let claimed = decision(&req, &digest, ALLOW_ONCE_ID, "operator:mallory");
        assert_eq!(
            book.decide(&claimed, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotOperator)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));

        let empty = decision(&req, &digest, ALLOW_ONCE_ID, "");
        book.decide(&empty, Some(OPERATOR), TENANT, T0 + 1)
            .expect("empty decided_by is filled by the verified identity");
        assert_eq!(book.decided_by("r1"), Some(OPERATOR));
        assert_eq!(book.decided_ms("r1"), Some(T0 + 1));
    }

    #[test]
    fn req_0006_015_single_use_replay_is_harmless_and_conflict_is_refused() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let allow = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        let first = book
            .decide(&allow, Some(OPERATOR), TENANT, T0 + 1)
            .expect("first");
        assert!(!first.replayed);
        let replay = book
            .decide(&allow, Some(OPERATOR), TENANT, T0 + 2)
            .expect("replay");
        assert!(replay.replayed);
        assert_eq!(replay.verdict, Verdict::Allowed);
        let conflict = decision(&req, &digest, REJECT_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&conflict, Some(OPERATOR), TENANT, T0 + 3),
            Err(Refusal::AlreadyResolved)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Allowed));
        assert_eq!(book.decided_ms("r1"), Some(T0 + 1));
    }

    #[test]
    fn req_0006_016_decision_scope_is_its_own_request_and_task() {
        let mut book = ApprovalBook::default();
        let (a, digest_a) = open_one(&mut book, "ra", T0);
        book.open(
            NewRequest {
                tool: "other".to_owned(),
                ..spec("rb")
            },
            T0,
        )
        .expect("rb");
        let wrong_task = ApprovalDecision {
            task_id: "task-2".to_owned(),
            ..decision(&a, &digest_a, ALLOW_ONCE_ID, OPERATOR)
        };
        assert_eq!(
            book.decide(&wrong_task, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        let for_a = decision(&a, &digest_a, ALLOW_ONCE_ID, OPERATOR);
        book.decide(&for_a, Some(OPERATOR), TENANT, T0 + 1)
            .expect("a");
        assert_eq!(book.state("ra"), Some(RequestState::Allowed));
        assert_eq!(book.state("rb"), Some(RequestState::Open));
    }

    #[test]
    fn req_0006_017_expiry_closes_request_and_late_decision_is_refused() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let expires = T0 + DEFAULT_EXPIRY_MS;
        assert_eq!(book.next_expiry(), Some(expires));
        assert!(book.expire_due(expires - 1).is_empty());
        let late = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&late, Some(OPERATOR), TENANT, expires),
            Err(Refusal::NotApplicable)
        );
        let ended = book.expire_due(expires);
        assert_eq!(
            ended,
            vec![Ended {
                request_id: "r1".to_owned(),
                task_id: "task-1".to_owned(),
                state: RequestState::Expired,
            }]
        );
        assert_eq!(book.state("r1"), Some(RequestState::Expired));
        assert_eq!(book.next_expiry(), None);
    }

    #[test]
    fn req_0006_018_cancel_closes_every_open_request_of_the_task() {
        let mut book = ApprovalBook::default();
        let (a, digest_a) = open_one(&mut book, "ra", T0);
        book.open(
            NewRequest {
                tool: "other".to_owned(),
                ..spec("rb")
            },
            T0,
        )
        .expect("rb");
        let ended = book.close_task("task-1");
        assert_eq!(ended.len(), 2);
        assert!(ended.iter().all(|e| e.state == RequestState::Closed));
        let d = decision(&a, &digest_a, ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&d, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        assert!(book.close_task("task-1").is_empty());
    }

    #[test]
    fn req_0006_019_denied_settles_as_denied_with_reject_option() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let reject = decision(&req, &digest, REJECT_ONCE_ID, OPERATOR);
        let settled = book
            .decide(&reject, Some(OPERATOR), TENANT, T0 + 1)
            .expect("reject");
        assert_eq!(settled.verdict, Verdict::Denied);
        assert_eq!(settled.verdict.option_id(), REJECT_ONCE_ID);
        assert_eq!(book.state("r1"), Some(RequestState::Denied));
    }

    #[test]
    fn req_0006_021_identical_call_joins_within_30s_and_later_call_is_new() {
        let mut book = ApprovalBook::default();
        let (_, _) = open_one(&mut book, "ra", T0);
        let joined = book.open(spec("rb"), T0 + COALESCE_MS - 1).expect("open");
        assert!(matches!(joined, Opened::Joined { ref request_id } if request_id == "ra"));
        let later = book.open(spec("rc"), T0 + COALESCE_MS).expect("open");
        assert!(matches!(later, Opened::Created { .. }));
        assert_eq!(book.open_for_task("task-1"), 2);
    }

    #[test]
    fn req_0006_022_fourth_open_request_is_refused_as_flood() {
        let mut book = ApprovalBook::default();
        for i in 0..MAX_OPEN_PER_TASK {
            let mut s = spec(&format!("r{i}"));
            s.summary = format!("cmd {i}");
            assert!(matches!(book.open(s, T0), Ok(Opened::Created { .. })));
        }
        let mut fourth = spec("r3");
        fourth.summary = "cmd 3".to_owned();
        assert!(matches!(book.open(fourth, T0), Ok(Opened::Flood)));
        assert_eq!(book.open_for_task("task-1"), MAX_OPEN_PER_TASK);
        assert_eq!(book.state("r3"), None);
    }

    #[test]
    fn req_0006_023_eleventh_decision_in_a_minute_is_refused_without_state_change() {
        let mut book = ApprovalBook::default();
        let mut opened = Vec::new();
        for i in 0..=DECISIONS_PER_MINUTE {
            let mut s = spec(&format!("r{i}"));
            s.task_id = format!("t{i}");
            opened.push(created(book.open(s, T0).expect("open")).expect("created"));
        }
        for (i, (req, digest)) in opened.iter().enumerate().take(DECISIONS_PER_MINUTE) {
            let d = decision(req, digest, ALLOW_ONCE_ID, OPERATOR);
            book.decide(&d, Some(OPERATOR), TENANT, T0 + i as u64)
                .expect("within rate");
        }
        let (req, digest) = &opened[DECISIONS_PER_MINUTE];
        let eleventh = decision(req, digest, ALLOW_ONCE_ID, OPERATOR);
        let refused = book.decide(&eleventh, Some(OPERATOR), TENANT, T0 + 10);
        assert_eq!(refused, Err(Refusal::RateLimited));
        assert_eq!(refused.expect_err("refused").detail(), "RATE_LIMITED");
        assert_eq!(book.state(&req.request_id), Some(RequestState::Open));
        let other_tenant = book.decide(&eleventh, Some(OPERATOR), "tenant-b", T0 + 10);
        assert_ne!(other_tenant, Err(Refusal::RateLimited));
        let after_window = book.decide(&eleventh, Some(OPERATOR), TENANT, T0 + MINUTE_MS);
        assert!(after_window.is_ok());
    }

    #[test]
    fn req_0006_051_decision_digest_must_match_the_stored_request_digest() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        assert_eq!(digest, digest_of(&req));
        assert_eq!(book.digest("r1"), Some(digest.as_str()));
        let missing = decision(&req, "", ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&missing, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        let wrong = decision(&req, &"0".repeat(64), ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&wrong, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
        assert_eq!(book.state("r1"), Some(RequestState::Open));
        let good = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        book.decide(&good, Some(OPERATOR), TENANT, T0 + 1)
            .expect("matching digest honored");
    }

    #[test]
    fn req_0006_055_verdict_selects_offered_once_option_id() {
        assert_eq!(Verdict::Allowed.option_id(), ALLOW_ONCE_ID);
        assert_eq!(Verdict::Denied.option_id(), REJECT_ONCE_ID);
        let mut book = ApprovalBook::default();
        let (req, _) = open_one(&mut book, "r1", T0);
        let offered: Vec<&str> = req.options.iter().map(|o| o.option_id.as_str()).collect();
        assert!(offered.contains(&Verdict::Allowed.option_id()));
        assert!(offered.contains(&Verdict::Denied.option_id()));
    }

    #[test]
    fn req_0006_056_backend_exit_closes_open_requests_and_refuses_later_decision() {
        let mut book = ApprovalBook::default();
        let (a, digest_a) = open_one(&mut book, "ra", T0);
        let mut other = spec("rb");
        other.task_id = "task-2".to_owned();
        book.open(other, T0).expect("open");
        let ended = book.close_backend();
        assert_eq!(ended.len(), 2);
        assert_eq!(book.state("ra"), Some(RequestState::Closed));
        let late = decision(&a, &digest_a, ALLOW_ONCE_ID, OPERATOR);
        assert_eq!(
            book.decide(&late, Some(OPERATOR), TENANT, T0 + 1),
            Err(Refusal::NotApplicable)
        );
    }

    #[test]
    fn req_0006_005_book_survives_serde_restart() {
        let mut book = ApprovalBook::default();
        let (req, digest) = open_one(&mut book, "r1", T0);
        let json = serde_json::to_string(&book).expect("serialize");
        let mut restored: ApprovalBook = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.digest("r1"), Some(digest.as_str()));
        let d = decision(&req, &digest, ALLOW_ONCE_ID, OPERATOR);
        restored
            .decide(&d, Some(OPERATOR), TENANT, T0 + 1)
            .expect("restored request still decidable");
    }
}
