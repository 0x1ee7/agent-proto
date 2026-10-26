//! Approval decisions (RFC-0006 [0006-009..015], [0006-051]) under the operator-origin rules of
//! RFC-0007 [0007-095..098].

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use a2a_types::proto::a2abus::{
    ApprovalDecision, ApprovalOption, ApprovalOptionKind, ApprovalRequest,
};
use pbjson_types::Timestamp;
use prost::Message;

use crate::canon::{sha256, to_hex};
use crate::clock::Clock;
use crate::envelope::{Envelope, VerifiedSender, VerifyContext, sign_envelope};
use crate::error::{BusError, Reason, SecurityError};
use crate::identity::Signer;
use crate::mode::SigningMode;
use crate::trust::{OPERATOR_AGENT, Role};

/// Where a decision came from. The layer that authenticated the origin constructs this.
#[derive(Clone, Copy, Debug)]
pub enum DecisionOrigin<'a> {
    /// The envelope signature verified for this sender ([0007-034]).
    Signed(&'a VerifiedSender),
    /// The local operator channel. Accepted only while signing is disabled ([0007-095]).
    LocalOperatorChannel,
    /// No signature block. `via_gateway` marks decisions forwarded by the HTTP gateway.
    Unsigned { via_gateway: bool },
}

/// A decision that passed every check in [0006-013] and [0006-014]. Only [`verify_decision`]
/// builds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDecision {
    request_id: String,
    task_id: String,
    option_id: String,
    kind: ApprovalOptionKind,
    decided_by: String,
    decided_at_ms: i64,
}

impl VerifiedDecision {
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    #[must_use]
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    #[must_use]
    pub fn option_id(&self) -> &str {
        &self.option_id
    }

    #[must_use]
    pub const fn kind(&self) -> ApprovalOptionKind {
        self.kind
    }

    /// The verified identity of the decider. The client's `decided_by` is never trusted.
    #[must_use]
    pub fn decided_by(&self) -> &str {
        &self.decided_by
    }

    /// The time the decision was recorded, from the local clock.
    #[must_use]
    pub const fn decided_at_ms(&self) -> i64 {
        self.decided_at_ms
    }

    /// True when the decision lets the gated call proceed (allow once or allow always).
    #[must_use]
    pub const fn allows(&self) -> bool {
        matches!(
            self.kind,
            ApprovalOptionKind::AllowOnce | ApprovalOptionKind::AllowAlways
        )
    }
}

#[cfg(test)]
impl VerifiedDecision {
    /// An operator decision on `task_id` of the given kind, for delegation tests.
    pub(crate) fn for_test(task_id: &str, kind: ApprovalOptionKind) -> Self {
        Self {
            request_id: "req-1".to_owned(),
            task_id: task_id.to_owned(),
            option_id: "opt-1".to_owned(),
            kind,
            decided_by: OPERATOR_AGENT.to_owned(),
            decided_at_ms: 0,
        }
    }
}

/// Result of an accepted decision. `duplicate` marks an identical repeat of the stored decision
/// ([0006-015]), which returns the stored result and changes nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionOutcome {
    decision: VerifiedDecision,
    duplicate: bool,
}

impl DecisionOutcome {
    #[must_use]
    pub const fn decision(&self) -> &VerifiedDecision {
        &self.decision
    }

    #[must_use]
    pub const fn duplicate(&self) -> bool {
        self.duplicate
    }
}

/// Lowercase hex SHA-256 of the protobuf binary encoding of `request` ([0006-051]).
#[must_use]
pub fn request_digest(request: &ApprovalRequest) -> String {
    to_hex(&sha256(&request.encode_to_vec()))
}

/// Signs an operator decision that chooses `option_id` on `request` ([0006-051], [0007-095]).
///
/// The decision carries the request digest, task, and nonce, so a changed request is refused on
/// verification. The envelope signature covers the encoded decision. `decided_by` names the
/// operator identity; `decided_at` stays unset because the verifier records its own clock.
/// Returns the encoded payload that was signed.
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] when the key is not the operator key or the request does
/// not offer `option_id`. Otherwise returns the error from [`sign_envelope`].
pub fn sign_decision(
    signer: &dyn Signer,
    clock: &dyn Clock,
    envelope: &mut Envelope,
    request: &ApprovalRequest,
    option_id: &str,
) -> Result<Vec<u8>, SecurityError> {
    if signer.kid().agent().as_str() != OPERATOR_AGENT {
        return Err(SecurityError::Invalid(
            "only the operator key signs approval decisions",
        ));
    }
    if !request
        .options
        .iter()
        .any(|option| option.option_id == option_id)
    {
        return Err(SecurityError::Invalid(
            "approval option is not offered by the request",
        ));
    }
    let decision = ApprovalDecision {
        request_id: request.request_id.clone(),
        task_id: request.task_id.clone(),
        option_id: option_id.to_owned(),
        nonce: request.nonce.clone(),
        decided_by: OPERATOR_AGENT.to_owned(),
        decided_at: None,
        request_sha256: request_digest(request),
    };
    let payload = decision.encode_to_vec();
    sign_envelope(signer, clock, envelope, &payload)?;
    Ok(payload)
}

/// Open approval requests owned by one Worker. Entries are dropped at their expiry.
#[derive(Debug, Default)]
pub struct ApprovalBook {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug)]
struct Entry {
    task_id: String,
    nonce: String,
    digest: String,
    options: Vec<(String, ApprovalOptionKind)>,
    expires_ms: i64,
    resolution: Option<VerifiedDecision>,
}

impl ApprovalBook {
    /// Opens a request and returns its digest, which the Worker stores for [0006-051].
    /// Enforces [0006-009]. A request id already in the book is refused, including after the
    /// request was resolved.
    pub fn open(&self, request: &ApprovalRequest, now_ms: i64) -> Result<String, SecurityError> {
        if request.request_id.is_empty() {
            return Err(SecurityError::Invalid("approval request_id is empty"));
        }
        if request.task_id.is_empty() {
            return Err(SecurityError::Invalid("approval task_id is empty"));
        }
        if request.nonce.is_empty() {
            return Err(SecurityError::Invalid("approval nonce is empty"));
        }
        let expires_ms =
            request
                .expires_at
                .as_ref()
                .and_then(timestamp_ms)
                .ok_or(SecurityError::Invalid(
                    "approval expires_at is missing or out of range",
                ))?;
        if expires_ms <= now_ms {
            return Err(SecurityError::Invalid(
                "approval expires_at is not in the future",
            ));
        }
        let options = parse_options(&request.options)?;
        let digest = request_digest(request);
        let mut entries = self.lock();
        entries.retain(|_, entry| entry.expires_ms > now_ms);
        if entries.contains_key(&request.request_id) {
            return Err(SecurityError::Invalid(
                "approval request_id is already open",
            ));
        }
        entries.insert(
            request.request_id.clone(),
            Entry {
                task_id: request.task_id.clone(),
                nonce: request.nonce.clone(),
                digest: digest.clone(),
                options,
                expires_ms,
                resolution: None,
            },
        );
        Ok(digest)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn parse_options(
    options: &[ApprovalOption],
) -> Result<Vec<(String, ApprovalOptionKind)>, SecurityError> {
    let mut parsed: Vec<(String, ApprovalOptionKind)> = Vec::with_capacity(options.len());
    for option in options {
        if option.option_id.is_empty() {
            return Err(SecurityError::Invalid("approval option id is empty"));
        }
        let kind = ApprovalOptionKind::try_from(option.kind)
            .ok()
            .filter(|kind| *kind != ApprovalOptionKind::Unspecified)
            .ok_or(SecurityError::Invalid("approval option kind is unknown"))?;
        if parsed.iter().any(|(id, _)| id == &option.option_id) {
            return Err(SecurityError::Invalid("approval option ids are not unique"));
        }
        parsed.push((option.option_id.clone(), kind));
    }
    let offers = |wanted: ApprovalOptionKind| parsed.iter().any(|(_, kind)| *kind == wanted);
    if !offers(ApprovalOptionKind::AllowOnce) || !offers(ApprovalOptionKind::RejectOnce) {
        return Err(SecurityError::Invalid(
            "approval needs allow_once and reject_once options",
        ));
    }
    Ok(parsed)
}

/// Checks an incoming decision against the book ([0006-013..015], [0006-051]).
///
/// Origin is checked first: signature, role, and identity ([0006-014], [0007-098]). Then the
/// request must be open, unexpired, and match its digest, task, nonce, and offered option. A
/// refusal changes no state. The first accepted decision closes the request.
pub fn verify_decision(
    ctx: &VerifyContext<'_>,
    book: &ApprovalBook,
    origin: DecisionOrigin<'_>,
    decision: &ApprovalDecision,
) -> Result<DecisionOutcome, BusError> {
    let decided_by = authorize_origin(ctx, origin)?;
    if !decision.decided_by.is_empty() && decision.decided_by != decided_by {
        return Err(BusError::new(Reason::SignatureIdentityMismatch));
    }
    let now = ctx.clock.now_ms();
    let refuse = || not_applicable(&decision.request_id);
    let mut entries = book.lock();
    // Entries at or past expiry are dropped here, so an expired request is unknown ([0006-017]).
    entries.retain(|_, entry| entry.expires_ms > now);
    let entry = entries.get_mut(&decision.request_id).ok_or_else(refuse)?;
    if decision.request_sha256 != entry.digest
        || decision.task_id != entry.task_id
        || decision.nonce != entry.nonce
    {
        return Err(refuse());
    }
    let kind = entry
        .options
        .iter()
        .find(|(id, _)| id == &decision.option_id)
        .map(|(_, kind)| *kind)
        .ok_or_else(refuse)?;
    if let Some(prior) = &entry.resolution {
        if prior.option_id == decision.option_id {
            return Ok(DecisionOutcome {
                decision: prior.clone(),
                duplicate: true,
            });
        }
        return Err(BusError::new(Reason::ApprovalAlreadyResolved)
            .with_request_id(decision.request_id.clone()));
    }
    let resolved = VerifiedDecision {
        request_id: decision.request_id.clone(),
        task_id: decision.task_id.clone(),
        option_id: decision.option_id.clone(),
        kind,
        decided_by,
        decided_at_ms: now,
    };
    entry.resolution = Some(resolved.clone());
    Ok(DecisionOutcome {
        decision: resolved,
        duplicate: false,
    })
}

/// Returns the verified identity of the decider, or refuses the origin ([0007-095], [0007-098]).
fn authorize_origin(
    ctx: &VerifyContext<'_>,
    origin: DecisionOrigin<'_>,
) -> Result<String, BusError> {
    let signing_on = ctx.policy.signing == SigningMode::Enabled;
    match origin {
        DecisionOrigin::Signed(sender) => {
            if sender.role() != Role::Operator {
                return Err(BusError::new(Reason::KeyRoleDenied).with_kid(sender.kid().to_string()));
            }
            Ok(sender.agent().as_str().to_owned())
        }
        DecisionOrigin::LocalOperatorChannel if signing_on => {
            Err(BusError::new(Reason::SignatureMissing))
        }
        DecisionOrigin::LocalOperatorChannel => Ok(OPERATOR_AGENT.to_owned()),
        DecisionOrigin::Unsigned { via_gateway: true } => Err(BusError::new(Reason::KeyRoleDenied)),
        DecisionOrigin::Unsigned { via_gateway: false } if signing_on => {
            Err(BusError::new(Reason::SignatureMissing))
        }
        DecisionOrigin::Unsigned { via_gateway: false } => {
            Err(BusError::new(Reason::KeyRoleDenied))
        }
    }
}

fn not_applicable(request_id: &str) -> BusError {
    BusError::new(Reason::ApprovalNotApplicable).with_request_id(request_id)
}

fn timestamp_ms(timestamp: &Timestamp) -> Option<i64> {
    timestamp
        .seconds
        .checked_mul(1_000)?
        .checked_add(i64::from(timestamp.nanos) / 1_000_000)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use a2a_types::ids::{AgentId, TenantId};
    use a2a_types::proto::a2abus::ApprovalTier;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::clock::ManualClock;
    use crate::envelope::{MessageClass, verify_request};
    use crate::identity::Kid;
    use crate::mode::Policy;
    use crate::replay::ReplayCache;
    use crate::testkit::{agent, key_block, key_block_with_role, signer, store};
    use crate::trust::TrustStore;

    const NOW: i64 = 1_800_000_000_000;
    const REQUEST_ID: &str = "0190f3c2-7d1a-7b2e-8c3d-4e5f60718293";
    const TASK: &str = "task-1";
    const NONCE: &str = "nonce-1";

    fn ts(ms: i64) -> Timestamp {
        Timestamp {
            seconds: ms / 1_000,
            nanos: 0,
        }
    }

    fn option(id: &str, kind: ApprovalOptionKind) -> ApprovalOption {
        ApprovalOption {
            option_id: id.to_owned(),
            name: id.to_owned(),
            kind: kind.into(),
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            request_id: REQUEST_ID.to_owned(),
            task_id: TASK.to_owned(),
            tool: "bash".to_owned(),
            tier: ApprovalTier::Exec.into(),
            summary: "run the test suite".to_owned(),
            requester: "claude".to_owned(),
            chain: vec!["claude".to_owned()],
            options: vec![
                option("allow-once", ApprovalOptionKind::AllowOnce),
                option("allow-always", ApprovalOptionKind::AllowAlways),
                option("reject-once", ApprovalOptionKind::RejectOnce),
            ],
            expires_at: Some(ts(NOW + 60_000)),
            nonce: NONCE.to_owned(),
            working_directory: "/work/app".to_owned(),
            workspace_mode: "per-task".to_owned(),
        }
    }

    fn decision_for(option_id: &str, digest: &str) -> ApprovalDecision {
        ApprovalDecision {
            request_id: REQUEST_ID.to_owned(),
            task_id: TASK.to_owned(),
            option_id: option_id.to_owned(),
            nonce: NONCE.to_owned(),
            decided_by: String::new(),
            decided_at: None,
            request_sha256: digest.to_owned(),
        }
    }

    fn sender(id: AgentId, role: Role) -> VerifiedSender {
        VerifiedSender::for_test(Kid::derive(&id, &[7; 32]), id, role)
    }

    /// A trust store, a manual clock, and one opened request.
    struct Harness {
        trust: TrustStore,
        clock: ManualClock,
        tenant: TenantId,
        book: ApprovalBook,
        digest: String,
    }

    impl Harness {
        fn new() -> Self {
            let book = ApprovalBook::default();
            let digest = book.open(&request(), NOW).expect("request opens");
            Self {
                trust: store(&key_block("alice", 7, "")),
                clock: ManualClock::new(NOW),
                tenant: TenantId::parse("local").expect("tenant"),
                book,
                digest,
            }
        }

        fn decide(
            &self,
            signing: SigningMode,
            origin: DecisionOrigin<'_>,
            decision: &ApprovalDecision,
        ) -> Result<DecisionOutcome, BusError> {
            let ctx = VerifyContext {
                trust: &self.trust,
                clock: &self.clock,
                policy: Policy {
                    signing,
                    cross_host: false,
                },
                tenant: &self.tenant,
            };
            verify_decision(&ctx, &self.book, origin, decision)
        }
    }

    #[test]
    fn req_0006_009_request_with_allow_once_and_reject_once_opens() {
        let digest = ApprovalBook::default()
            .open(&request(), NOW)
            .expect("valid request opens");
        assert_eq!(digest, request_digest(&request()));
    }

    #[test]
    fn req_0006_009_request_missing_allow_once_or_reject_once_is_refused() {
        for dropped in ["allow-once", "reject-once"] {
            let mut req = request();
            req.options.retain(|o| o.option_id != dropped);
            let result = ApprovalBook::default().open(&req, NOW);
            assert!(
                matches!(result, Err(SecurityError::Invalid(_))),
                "{dropped} must be offered"
            );
        }
        let mut none = request();
        none.options.clear();
        assert!(ApprovalBook::default().open(&none, NOW).is_err());
    }

    #[test]
    fn req_0006_009_duplicate_option_ids_are_refused() {
        let mut req = request();
        req.options
            .push(option("allow-once", ApprovalOptionKind::RejectAlways));
        assert!(ApprovalBook::default().open(&req, NOW).is_err());
    }

    #[test]
    fn req_0006_009_unknown_or_unspecified_option_kind_is_refused() {
        for raw in [0, 42] {
            let mut req = request();
            req.options.push(ApprovalOption {
                option_id: "odd".to_owned(),
                name: "odd".to_owned(),
                kind: raw,
            });
            assert!(
                ApprovalBook::default().open(&req, NOW).is_err(),
                "kind {raw}"
            );
        }
    }

    #[test]
    fn req_0006_009_request_missing_identifiers_or_expiry_is_refused() {
        let mutations: [fn(&mut ApprovalRequest); 4] = [
            |r| r.request_id.clear(),
            |r| r.nonce.clear(),
            |r| r.expires_at = None,
            |r| r.expires_at = Some(ts(NOW - 1_000)),
        ];
        for mutate in mutations {
            let mut req = request();
            mutate(&mut req);
            assert!(ApprovalBook::default().open(&req, NOW).is_err());
        }
    }

    #[test]
    fn req_0006_009_request_id_cannot_be_reopened_even_after_resolution() {
        let h = Harness::new();
        assert!(h.book.open(&request(), NOW).is_err());
        h.decide(
            SigningMode::Disabled,
            DecisionOrigin::LocalOperatorChannel,
            &decision_for("allow-once", &h.digest),
        )
        .expect("decides");
        assert!(h.book.open(&request(), NOW).is_err());
    }

    #[test]
    fn req_0006_051_digest_is_lowercase_sha256_of_protobuf_bytes() {
        let bytes = request().encode_to_vec();
        let expected: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(expected.len(), 64);
        assert_eq!(request_digest(&request()), expected);

        let mut changed = request();
        changed.summary.push('!');
        assert_ne!(request_digest(&changed), expected);
    }

    #[test]
    fn req_0006_051_missing_or_wrong_digest_is_not_applicable() {
        let h = Harness::new();
        let wrong = "0".repeat(64);
        for digest in ["", wrong.as_str()] {
            let err = h
                .decide(
                    SigningMode::Disabled,
                    DecisionOrigin::LocalOperatorChannel,
                    &decision_for("allow-once", digest),
                )
                .expect_err("digest must match the stored request");
            assert_eq!(err.reason(), Reason::ApprovalNotApplicable);
        }
        let ok = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &decision_for("allow-once", &h.digest),
            )
            .expect("correct digest decides");
        assert!(ok.decision().allows());
    }

    #[test]
    fn req_0006_013_unknown_request_is_not_applicable() {
        let h = Harness::new();
        let mut d = decision_for("allow-once", &h.digest);
        d.request_id = "0190f3c2-7d1a-7b2e-8c3d-000000000000".to_owned();
        let err = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &d,
            )
            .expect_err("unknown request");
        assert_eq!(err.reason(), Reason::ApprovalNotApplicable);
        assert!(err.reason().is_approval());
    }

    #[test]
    fn req_0006_013_wrong_task_nonce_or_option_is_not_applicable() {
        let h = Harness::new();
        let mut wrong_task = decision_for("allow-once", &h.digest);
        wrong_task.task_id = "other-task".to_owned();
        let mut wrong_nonce = decision_for("allow-once", &h.digest);
        wrong_nonce.nonce = "other-nonce".to_owned();
        let not_offered = decision_for("allow-everything", &h.digest);
        for d in [wrong_task, wrong_nonce, not_offered] {
            let err = h
                .decide(
                    SigningMode::Disabled,
                    DecisionOrigin::LocalOperatorChannel,
                    &d,
                )
                .expect_err("mismatched decision is refused");
            assert_eq!(err.reason(), Reason::ApprovalNotApplicable);
        }
        // The refusals changed no state: the honest decision still lands.
        h.decide(
            SigningMode::Disabled,
            DecisionOrigin::LocalOperatorChannel,
            &decision_for("allow-once", &h.digest),
        )
        .expect("honest decision still accepted");
    }

    #[test]
    fn req_0006_013_expiry_is_exclusive_at_the_deadline() {
        let before = Harness::new();
        before.clock.set(NOW + 59_999);
        before
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &decision_for("allow-once", &before.digest),
            )
            .expect("one millisecond before expiry is still open");

        let at = Harness::new();
        at.clock.set(NOW + 60_000);
        let err = at
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &decision_for("allow-once", &at.digest),
            )
            .expect_err("at expiry the request is gone");
        assert_eq!(err.reason(), Reason::ApprovalNotApplicable);
    }

    #[test]
    fn req_0007_098_agent_and_gateway_keys_cannot_decide() {
        let h = Harness::new();
        let d = decision_for("allow-once", &h.digest);
        for (id, role) in [
            (agent("claude"), Role::Agent),
            (AgentId::gateway(), Role::Gateway),
        ] {
            let verified = sender(id, role);
            let err = h
                .decide(SigningMode::Enabled, DecisionOrigin::Signed(&verified), &d)
                .expect_err("only the operator role decides");
            assert_eq!(err.reason(), Reason::KeyRoleDenied);
            assert!(err.kid().is_some(), "kid is attached to the refusal");
        }
    }

    #[test]
    fn req_0006_014_operator_signed_decision_is_accepted() {
        let h = Harness::new();
        let op = sender(agent(OPERATOR_AGENT), Role::Operator);
        let outcome = h
            .decide(
                SigningMode::Enabled,
                DecisionOrigin::Signed(&op),
                &decision_for("reject-once", &h.digest),
            )
            .expect("operator decides");
        assert_eq!(outcome.decision().decided_by(), OPERATOR_AGENT);
        assert!(!outcome.decision().allows());
    }

    #[test]
    fn req_0006_014_decided_by_must_match_verified_identity_or_be_empty() {
        let h = Harness::new();
        let op = sender(agent(OPERATOR_AGENT), Role::Operator);
        let origin = DecisionOrigin::Signed(&op);

        let mut forged = decision_for("allow-once", &h.digest);
        forged.decided_by = "mallory".to_owned();
        let err = h
            .decide(SigningMode::Enabled, origin, &forged)
            .expect_err("claimed identity differs from the verified one");
        assert_eq!(err.reason(), Reason::SignatureIdentityMismatch);

        // The refused forgery did not close the request. An empty decided_by is filled in.
        let outcome = h
            .decide(
                SigningMode::Enabled,
                origin,
                &decision_for("allow-once", &h.digest),
            )
            .expect("empty decided_by is accepted");
        assert_eq!(outcome.decision().decided_by(), OPERATOR_AGENT);
    }

    #[test]
    fn req_0007_095_local_channel_decides_only_while_signing_is_disabled() {
        let h = Harness::new();
        let d = decision_for("allow-once", &h.digest);
        let err = h
            .decide(
                SigningMode::Enabled,
                DecisionOrigin::LocalOperatorChannel,
                &d,
            )
            .expect_err("signing enabled requires a signed decision");
        assert_eq!(err.reason(), Reason::SignatureMissing);

        let outcome = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &d,
            )
            .expect("local channel decides while signing is disabled");
        assert_eq!(outcome.decision().decided_by(), OPERATOR_AGENT);
    }

    #[test]
    fn req_0007_095_gateway_and_unsigned_origins_never_decide() {
        let h = Harness::new();
        let d = decision_for("allow-once", &h.digest);
        for signing in [SigningMode::Disabled, SigningMode::Enabled] {
            let err = h
                .decide(signing, DecisionOrigin::Unsigned { via_gateway: true }, &d)
                .expect_err("gateway never decides");
            assert_eq!(err.reason(), Reason::KeyRoleDenied);
        }
        let unsigned_enabled = h
            .decide(
                SigningMode::Enabled,
                DecisionOrigin::Unsigned { via_gateway: false },
                &d,
            )
            .expect_err("signing enabled requires a signature");
        assert_eq!(unsigned_enabled.reason(), Reason::SignatureMissing);
        let unsigned_disabled = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::Unsigned { via_gateway: false },
                &d,
            )
            .expect_err("unsigned direct decision is not a local operator decision");
        assert_eq!(unsigned_disabled.reason(), Reason::KeyRoleDenied);
    }

    #[test]
    fn req_0006_015_first_decision_closes_and_identical_repeat_returns_it() {
        let h = Harness::new();
        let d = decision_for("allow-once", &h.digest);
        let first = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &d,
            )
            .expect("first decision");
        assert!(!first.duplicate());
        assert!(first.decision().allows());

        h.clock.advance(1_000);
        let repeat = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &d,
            )
            .expect("identical repeat");
        assert!(repeat.duplicate());
        assert_eq!(repeat.decision(), first.decision());
        assert_eq!(
            repeat.decision().decided_at_ms(),
            NOW,
            "stored result is returned"
        );
    }

    #[test]
    fn req_0006_015_other_option_after_resolution_is_already_resolved() {
        let h = Harness::new();
        h.decide(
            SigningMode::Disabled,
            DecisionOrigin::LocalOperatorChannel,
            &decision_for("reject-once", &h.digest),
        )
        .expect("first decision");
        let err = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &decision_for("allow-once", &h.digest),
            )
            .expect_err("a different option cannot resolve a closed request");
        assert_eq!(err.reason(), Reason::ApprovalAlreadyResolved);
        assert!(err.reason().is_approval());
    }

    #[test]
    fn req_0007_096_verified_decision_binds_every_field() {
        let h = Harness::new();
        let outcome = h
            .decide(
                SigningMode::Disabled,
                DecisionOrigin::LocalOperatorChannel,
                &decision_for("allow-always", &h.digest),
            )
            .expect("decides");
        let d = outcome.decision();
        assert_eq!(d.request_id(), REQUEST_ID);
        assert_eq!(d.task_id(), TASK);
        assert_eq!(d.option_id(), "allow-always");
        assert_eq!(d.kind(), ApprovalOptionKind::AllowAlways);
        assert_eq!(d.decided_by(), OPERATOR_AGENT);
        assert_eq!(d.decided_at_ms(), NOW);
        assert!(d.allows());
    }

    #[test]
    fn req_0007_096_any_changed_field_is_refused() {
        let h = Harness::new();
        let mutations: [fn(&mut ApprovalDecision); 6] = [
            |d| d.request_id = "0190f3c2-7d1a-7b2e-8c3d-000000000000".to_owned(),
            |d| d.task_id = "other-task".to_owned(),
            |d| d.nonce = "other-nonce".to_owned(),
            |d| d.option_id = "allow-everything".to_owned(),
            |d| d.request_sha256 = "0".repeat(64),
            |d| d.decided_by = "mallory".to_owned(),
        ];
        for mutate in mutations {
            let mut d = decision_for("allow-once", &h.digest);
            mutate(&mut d);
            assert!(
                h.decide(
                    SigningMode::Disabled,
                    DecisionOrigin::LocalOperatorChannel,
                    &d
                )
                .is_err()
            );
        }
        // None of the refusals closed the request.
        h.decide(
            SigningMode::Disabled,
            DecisionOrigin::LocalOperatorChannel,
            &decision_for("allow-once", &h.digest),
        )
        .expect("untouched decision still accepted");
    }

    fn envelope_from(sender: &str) -> Envelope {
        Envelope {
            subject: "a2a.v1.agent.bob.task.send".to_owned(),
            msg_id: "01J00000000000000000000002".to_owned(),
            sender: sender.to_owned(),
            version: "1.0".to_owned(),
            payload_type: "a2a.v1.ApprovalDecision".to_owned(),
            content_type: "application/x-protobuf".to_owned(),
            ..Envelope::default()
        }
    }

    /// Verifies `env` as an operator decision relayed by `publisher` ([0007-038]).
    fn verify_relayed(
        h: &Harness,
        publisher: &str,
        env: &Envelope,
        payload: &[u8],
    ) -> Result<VerifiedSender, BusError> {
        let ctx = VerifyContext {
            trust: &h.trust,
            clock: &h.clock,
            policy: Policy::default(),
            tenant: &h.tenant,
        };
        let publisher = agent(publisher);
        verify_request(
            &ctx,
            MessageClass::OperatorDecision {
                publisher: &publisher,
            },
            &ReplayCache::new(64),
            env,
            payload,
        )
        .map(|sender| sender.expect("signed envelope yields a sender"))
    }

    #[test]
    fn req_0006_051_signed_decision_verifies_end_to_end() {
        let h = Harness {
            trust: store(&key_block_with_role("operator", 7, "operator", "")),
            ..Harness::new()
        };
        let op = signer("operator", 7);
        let mut env = envelope_from("tui");
        let payload = sign_decision(&op, &h.clock, &mut env, &request(), "allow-once")
            .expect("operator key signs a decision");
        let verified =
            verify_relayed(&h, "tui", &env, &payload).expect("relayed decision verifies");
        let decoded = ApprovalDecision::decode(payload.as_slice()).expect("payload decodes");
        let outcome = h
            .decide(
                SigningMode::Enabled,
                DecisionOrigin::Signed(&verified),
                &decoded,
            )
            .expect("operator decision is accepted");
        assert_eq!(outcome.decision().decided_by(), OPERATOR_AGENT);
        assert_eq!(outcome.decision().option_id(), "allow-once");
        assert!(outcome.decision().allows());
        assert!(!outcome.duplicate());
    }

    #[test]
    fn req_0006_051_sign_decision_refuses_non_operator_and_unoffered_option() {
        let clock = ManualClock::new(NOW);
        let alice = signer("alice", 7);
        let mut env = envelope_from("alice");
        let refused = sign_decision(&alice, &clock, &mut env, &request(), "allow-once");
        assert!(matches!(refused, Err(SecurityError::Invalid(_))));
        assert_eq!(env.kid, None);

        let op = signer("operator", 7);
        let mut env = envelope_from("tui");
        let refused = sign_decision(&op, &clock, &mut env, &request(), "maybe");
        assert!(matches!(refused, Err(SecurityError::Invalid(_))));
        assert_eq!(env.kid, None);
    }

    #[test]
    fn req_0006_051_decision_for_an_altered_request_is_not_applicable() {
        let h = Harness {
            trust: store(&key_block_with_role("operator", 7, "operator", "")),
            ..Harness::new()
        };
        let mut altered = request();
        altered.summary = "run a different command".to_owned();
        let op = signer("operator", 7);
        let mut env = envelope_from("tui");
        let payload = sign_decision(&op, &h.clock, &mut env, &altered, "allow-once")
            .expect("operator key signs a decision");
        let verified = verify_relayed(&h, "tui", &env, &payload).expect("envelope verifies");
        let decoded = ApprovalDecision::decode(payload.as_slice()).expect("payload decodes");
        let err = h
            .decide(
                SigningMode::Enabled,
                DecisionOrigin::Signed(&verified),
                &decoded,
            )
            .expect_err("digest of the altered request does not match the open request");
        assert_eq!(err.reason(), Reason::ApprovalNotApplicable);
    }
}
