//! Envelope signing and verification ([0007-030..046]).
//!
//! The signed bytes are a fixed line layout over the message's own headers plus the SHA-256 of
//! the payload bytes as received. Verification checks status, identity and replay before any
//! cryptography runs, and records a nonce only after its signature verifies.

use std::fmt;

use a2a_types::ids::{AgentId, TenantId};

use crate::canon::{b64_decode_canonical, b64_encode, sha256};
use crate::clock::{Clock, format_ts_ms, parse_ts_ms};
use crate::error::{BusError, Reason, SecurityError};
use crate::identity::{Kid, Signer, Verifier};
use crate::mode::Policy;
use crate::replay::ReplayCache;
use crate::trust::{Role, TrustStore};

/// Nonce length in bytes ([0007-041]).
const NONCE_LEN: usize = 16;
/// Longest semantic-header key ([0007-034] `A2AB-SP-<key>`).
const SP_KEY_MAX: usize = 64;
/// First line of the signing input ([0007-034]).
const SIGNING_TAG: &str = "a2abus-sig/1";

/// A message as it reaches the signing layer. Empty strings mean the header is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    pub subject: String,
    pub msg_id: String,
    pub sender: String,
    pub version: String,
    pub extensions: String,
    pub origin: String,
    pub ttl_ms: String,
    pub causation_id: String,
    pub payload_type: String,
    pub after_seq: String,
    pub content_type: String,
    /// `A2AB-SP-<key>` headers as (key, value) pairs, in any order; signing sorts them.
    pub semantic: Vec<(String, String)>,
    /// `A2AB-Kid` of the signature block.
    pub kid: Option<String>,
    /// `A2AB-Ts` of the signature block.
    pub ts: Option<String>,
    /// `A2AB-Nonce` of the signature block.
    pub nonce: Option<String>,
    /// `A2AB-Sig` of the signature block.
    pub signature: Option<String>,
}

impl Envelope {
    /// Number of signature-block headers present: 0 unsigned, 4 signed ([0007-032]).
    #[must_use]
    pub fn block_headers_present(&self) -> usize {
        [
            self.kid.is_some(),
            self.ts.is_some(),
            self.nonce.is_some(),
            self.signature.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count()
    }

    /// The signing input for these headers, the block fields and `payload` ([0007-034]).
    /// `None` when a value contains a line break or a semantic key is invalid or repeated.
    fn signing_input_with(
        &self,
        kid: &str,
        ts: &str,
        nonce: &str,
        payload: &[u8],
    ) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(SIGNING_TAG.as_bytes());
        out.push(b'\n');
        let fixed = [
            ("Subject", self.subject.as_str()),
            ("A2AB-Msg-Id", self.msg_id.as_str()),
            ("A2AB-Sender", self.sender.as_str()),
            ("A2AB-Kid", kid),
            ("A2AB-Ts", ts),
            ("A2AB-Nonce", nonce),
            ("A2A-Version", self.version.as_str()),
            ("A2A-Extensions", self.extensions.as_str()),
            ("A2AB-Origin", self.origin.as_str()),
            ("A2AB-Ttl-Ms", self.ttl_ms.as_str()),
            ("A2AB-Causation-Id", self.causation_id.as_str()),
            ("A2AB-Payload-Type", self.payload_type.as_str()),
            ("A2AB-After-Seq", self.after_seq.as_str()),
            ("Content-Type", self.content_type.as_str()),
        ];
        for (name, value) in fixed {
            push_line(&mut out, name, value)?;
        }
        let mut semantic: Vec<(&str, &str)> = Vec::with_capacity(self.semantic.len());
        for (key, value) in &self.semantic {
            if !valid_sp_key(key) {
                return None;
            }
            semantic.push((key.as_str(), value.as_str()));
        }
        semantic.sort_by(|a, b| a.0.cmp(b.0));
        if semantic.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return None;
        }
        for (key, value) in semantic {
            push_line(&mut out, &format!("A2AB-SP-{key}"), value)?;
        }
        push_line(&mut out, "Payload-SHA256", &b64_encode(&sha256(payload)))?;
        Some(out)
    }
}

fn valid_sp_key(key: &str) -> bool {
    (1..=SP_KEY_MAX).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn push_line(out: &mut Vec<u8>, name: &str, value: &str) -> Option<()> {
    if value.contains(['\r', '\n']) {
        return None;
    }
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(b": ");
    out.extend_from_slice(value.as_bytes());
    out.push(b'\n');
    Some(())
}

/// Signs `envelope` for `payload` as its sender ([0007-030..036]).
///
/// The sender must be the signer's agent and the signature block must be empty. Sets the four
/// block headers. Callers sign the exact bytes they publish ([0007-044]).
///
/// # Errors
///
/// Returns [`SecurityError::Invalid`] for a foreign sender, an existing block, or a header that
/// cannot be signed. Returns [`SecurityError::Random`] or a timestamp error from the system.
pub fn sign_envelope(
    signer: &dyn Signer,
    clock: &dyn Clock,
    envelope: &mut Envelope,
    payload: &[u8],
) -> Result<(), SecurityError> {
    if envelope.sender != signer.kid().agent().as_str() {
        return Err(SecurityError::Invalid("sender is not the signing agent"));
    }
    if envelope.block_headers_present() != 0 {
        return Err(SecurityError::Invalid("signature block already present"));
    }
    let ts = format_ts_ms(clock.now_ms())?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|_| SecurityError::Random)?;
    let nonce = b64_encode(&nonce_bytes);
    let kid = signer.kid().to_string();
    let input = envelope
        .signing_input_with(&kid, &ts, &nonce, payload)
        .ok_or(SecurityError::Invalid(
            "header contains a line break or a semantic key is invalid",
        ))?;
    let signature = b64_encode(&signer.sign_bytes(&input));
    envelope.kid = Some(kid);
    envelope.ts = Some(ts);
    envelope.nonce = Some(nonce);
    envelope.signature = Some(signature);
    Ok(())
}

/// Who the receiving side expects the message to come from.
#[derive(Clone, Copy, Debug)]
pub enum MessageClass<'a> {
    /// A message from a peer broker, published under `publisher` ([0007-038]).
    Peer { publisher: &'a AgentId },
    /// An operator decision on an approval prompt ([0007-095..098]).
    OperatorDecision,
}

/// Inputs to verification that do not change per message.
pub struct VerifyContext<'a> {
    pub trust: &'a TrustStore,
    pub clock: &'a dyn Clock,
    pub policy: Policy,
    /// The tenant the receiving side serves; a key must serve it ([0007-026]).
    pub tenant: &'a TenantId,
}

impl fmt::Debug for VerifyContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifyContext")
            .field("policy", &self.policy)
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// The key and identity that vouched for a verified envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedSender {
    kid: Kid,
    agent: AgentId,
    role: Role,
}

impl VerifiedSender {
    #[must_use]
    pub const fn kid(&self) -> &Kid {
        &self.kid
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }
}

#[cfg(test)]
impl VerifiedSender {
    /// Test-only constructor; production senders come from [`verify_request`] alone.
    pub(crate) fn for_test(kid: Kid, agent: AgentId, role: Role) -> Self {
        Self { kid, agent, role }
    }
}

/// Verifies an incoming envelope ([0007-030..041]).
///
/// Returns `Ok(None)` for an unsigned message the policy allows, and `Ok(Some(sender))` for a
/// verified one. The nonce is recorded only after the signature verifies, so forgeries cannot
/// exhaust the replay cache.
///
/// # Errors
///
/// Returns the [`BusError`] whose reason matches the first failed check in [0007-039] order.
pub fn verify_request(
    ctx: &VerifyContext<'_>,
    class: MessageClass<'_>,
    replay: &ReplayCache,
    envelope: &Envelope,
    payload: &[u8],
) -> Result<Option<VerifiedSender>, BusError> {
    if envelope.block_headers_present() == 0 {
        return unsigned(ctx, class).map(|()| None);
    }
    verify_signed(ctx, class, replay, envelope, payload).map(Some)
}

fn unsigned(ctx: &VerifyContext<'_>, class: MessageClass<'_>) -> Result<(), BusError> {
    match class {
        MessageClass::OperatorDecision => Ok(()),
        MessageClass::Peer { publisher } => {
            if ctx.policy.requires_signature() || ctx.trust.agent_requires_signature(publisher) {
                Err(BusError::new(Reason::SignatureMissing))
            } else {
                Ok(())
            }
        }
    }
}

fn verify_signed(
    ctx: &VerifyContext<'_>,
    class: MessageClass<'_>,
    replay: &ReplayCache,
    env: &Envelope,
    payload: &[u8],
) -> Result<VerifiedSender, BusError> {
    let (Some(kid_text), Some(ts), Some(nonce), Some(signature)) = (
        env.kid.as_deref(),
        env.ts.as_deref(),
        env.nonce.as_deref(),
        env.signature.as_deref(),
    ) else {
        return Err(BusError::new(Reason::SignatureInvalid));
    };
    if Kid::parse(kid_text).is_none() {
        return Err(BusError::new(Reason::SignatureInvalid));
    }
    let entry = ctx
        .trust
        .entry(kid_text)
        .ok_or_else(|| BusError::new(Reason::SignatureKeyUnknown))?;
    if !entry.serves(ctx.tenant) {
        return Err(BusError::new(Reason::SignatureKeyUnknown));
    }
    let now = ctx.clock.now_ms();
    entry.check_validity(now)?;

    let attach = |reason: Reason| BusError::new(reason).with_kid(kid_text);
    let role_ok = match class {
        MessageClass::Peer { .. } => matches!(entry.role(), Role::Agent | Role::Gateway),
        MessageClass::OperatorDecision => entry.role() == Role::Operator,
    };
    if !role_ok {
        return Err(attach(Reason::KeyRoleDenied));
    }
    let publisher_ok = match class {
        MessageClass::Peer { publisher } => publisher == entry.agent(),
        MessageClass::OperatorDecision => true,
    };
    if env.sender != entry.agent().as_str() || !publisher_ok {
        return Err(attach(Reason::SignatureIdentityMismatch));
    }

    let sent = parse_ts_ms(ts).ok_or_else(|| attach(Reason::SignatureInvalid))?;
    let skew = entry.max_skew_ms();
    if sent.saturating_sub(now).saturating_abs() > skew {
        return Err(attach(Reason::SignatureTimestampSkew));
    }
    let nonce_ok = b64_decode_canonical(nonce).is_some_and(|bytes| bytes.len() == NONCE_LEN);
    if !nonce_ok {
        return Err(attach(Reason::SignatureInvalid));
    }
    let replay_key = format!("{kid_text} {nonce}");
    if replay.contains(&replay_key, now) {
        return Err(attach(Reason::SignatureReplayed));
    }

    let input = env
        .signing_input_with(kid_text, ts, nonce, payload)
        .ok_or_else(|| attach(Reason::SignatureInvalid))?;
    let sig = b64_decode_canonical(signature).ok_or_else(|| attach(Reason::SignatureInvalid))?;
    if !entry.verify_bytes(&input, &sig) {
        return Err(attach(Reason::SignatureInvalid));
    }
    // A verified nonce stays live until the latest moment a message carrying it can pass the
    // skew check: one skew window ahead of its timestamp plus one behind.
    if !replay.record(&replay_key, now, skew.saturating_mul(2)) {
        return Err(attach(Reason::SignatureReplayed));
    }
    Ok(VerifiedSender {
        kid: entry.kid().clone(),
        agent: entry.agent().clone(),
        role: entry.role(),
    })
}

/// The origin a gateway vouches for: honored only when the verified signer is a gateway
/// ([0007-046]). Any other signer, or an empty origin, yields `None`.
#[must_use]
pub fn honored_origin<'a>(sender: Option<&VerifiedSender>, origin: &'a str) -> Option<&'a str> {
    match sender {
        Some(sender) if sender.role() == Role::Gateway && !origin.is_empty() => Some(origin),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::clock::{ManualClock, parse_rfc3339_utc_ms};
    use crate::identity::Ed25519Signer;
    use crate::mode::SigningMode;
    use crate::testkit::{agent, key_block, key_block_with_role, signer, store};

    const T0: i64 = 1_735_787_045_006; // 2025-01-02T03:04:05.006Z
    const PAYLOAD: &[u8] = b"abc";
    const ENABLED: Policy = Policy {
        signing: SigningMode::Enabled,
        cross_host: false,
    };

    fn sample(sender: &str) -> Envelope {
        Envelope {
            subject: "a2a.v1.agent.bob.task.send".to_owned(),
            msg_id: "01J00000000000000000000001".to_owned(),
            sender: sender.to_owned(),
            version: "1.0".to_owned(),
            payload_type: "a2a.v1.SendMessageRequest".to_owned(),
            content_type: "application/json".to_owned(),
            ..Envelope::default()
        }
    }

    fn golden() -> Envelope {
        Envelope {
            subject: "a2a.v1.agent.bob.task.send".to_owned(),
            msg_id: "01J00000000000000000000000".to_owned(),
            sender: "alice".to_owned(),
            version: "1.0".to_owned(),
            extensions: "urn:a2a:ext:a,urn:a2a:ext:b".to_owned(),
            ttl_ms: "30000".to_owned(),
            payload_type: "a2a.v1.SendMessageRequest".to_owned(),
            content_type: "application/json".to_owned(),
            semantic: vec![
                ("role".to_owned(), "planner".to_owned()),
                ("ack".to_owned(), "1".to_owned()),
            ],
            ..Envelope::default()
        }
    }

    fn sign_with(mut env: Envelope, who: &str, seed: u8, at: i64) -> Envelope {
        sign_envelope(&signer(who, seed), &ManualClock::new(at), &mut env, PAYLOAD)
            .expect("fixture signs");
        env
    }

    fn signed(who: &str, seed: u8, at: i64) -> Envelope {
        sign_with(sample(who), who, seed, at)
    }

    /// Signs with explicit timestamp and nonce bytes, bypassing the sender's clock and RNG.
    fn forge(who: &Ed25519Signer, env: &mut Envelope, ts: &str, nonce: &[u8], payload: &[u8]) {
        let kid = who.kid().to_string();
        let nonce_text = b64_encode(nonce);
        let input = env
            .signing_input_with(&kid, ts, &nonce_text, payload)
            .expect("fixture is signable");
        env.kid = Some(kid);
        env.ts = Some(ts.to_owned());
        env.nonce = Some(nonce_text);
        env.signature = Some(b64_encode(&who.sign_bytes(&input)));
    }

    fn verify(
        trust: &TrustStore,
        replay: &ReplayCache,
        policy: Policy,
        at: i64,
        publisher: &str,
        env: &Envelope,
        payload: &[u8],
    ) -> Result<Option<VerifiedSender>, BusError> {
        let clock = ManualClock::new(at);
        let tenant = TenantId::parse("local").expect("tenant id");
        let ctx = VerifyContext {
            trust,
            clock: &clock,
            policy,
            tenant: &tenant,
        };
        let publisher = agent(publisher);
        verify_request(
            &ctx,
            MessageClass::Peer {
                publisher: &publisher,
            },
            replay,
            env,
            payload,
        )
    }

    fn run(
        trust: &TrustStore,
        at: i64,
        env: &Envelope,
    ) -> Result<Option<VerifiedSender>, BusError> {
        verify(
            trust,
            &ReplayCache::new(64),
            Policy::default(),
            at,
            "alice",
            env,
            PAYLOAD,
        )
    }

    fn reason_of(out: Result<Option<VerifiedSender>, BusError>) -> Option<Reason> {
        out.err().map(|e| e.reason())
    }

    fn kid_of(out: &Result<Option<VerifiedSender>, BusError>) -> Option<String> {
        out.as_ref().err().and_then(|e| e.kid().map(str::to_owned))
    }

    #[test]
    fn req_0007_034_signing_input_matches_golden_layout() {
        let input = golden()
            .signing_input_with(
                "alice:AAAAAAAAAAA",
                "2025-01-02T03:04:05.006Z",
                "AAAAAAAAAAAAAAAAAAAAAA",
                PAYLOAD,
            )
            .expect("golden envelope is signable");
        let expected = concat!(
            "a2abus-sig/1\n",
            "Subject: a2a.v1.agent.bob.task.send\n",
            "A2AB-Msg-Id: 01J00000000000000000000000\n",
            "A2AB-Sender: alice\n",
            "A2AB-Kid: alice:AAAAAAAAAAA\n",
            "A2AB-Ts: 2025-01-02T03:04:05.006Z\n",
            "A2AB-Nonce: AAAAAAAAAAAAAAAAAAAAAA\n",
            "A2A-Version: 1.0\n",
            "A2A-Extensions: urn:a2a:ext:a,urn:a2a:ext:b\n",
            "A2AB-Origin: \n",
            "A2AB-Ttl-Ms: 30000\n",
            "A2AB-Causation-Id: \n",
            "A2AB-Payload-Type: a2a.v1.SendMessageRequest\n",
            "A2AB-After-Seq: \n",
            "Content-Type: application/json\n",
            "A2AB-SP-ack: 1\n",
            "A2AB-SP-role: planner\n",
            "Payload-SHA256: ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0\n",
        );
        assert_eq!(input.as_slice(), expected.as_bytes());
    }

    #[test]
    fn req_0007_030_unsigned_message_passes_when_signing_is_disabled() {
        let trust = store(&key_block("alice", 7, ""));
        let out = verify(
            &trust,
            &ReplayCache::new(8),
            Policy::default(),
            T0,
            "alice",
            &sample("alice"),
            PAYLOAD,
        );
        assert!(matches!(out, Ok(None)));
    }

    #[test]
    fn req_0007_031_unsigned_message_refused_when_signing_is_enabled() {
        let trust = store(&key_block("alice", 7, ""));
        let out = verify(
            &trust,
            &ReplayCache::new(8),
            ENABLED,
            T0,
            "alice",
            &sample("alice"),
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureMissing));
    }

    #[test]
    fn req_0007_031_unsigned_message_refused_for_cross_host_destination() {
        let trust = store(&key_block("alice", 7, ""));
        let policy = Policy {
            signing: SigningMode::Disabled,
            cross_host: true,
        };
        let out = verify(
            &trust,
            &ReplayCache::new(8),
            policy,
            T0,
            "alice",
            &sample("alice"),
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureMissing));
    }

    #[test]
    fn req_0007_033_unsigned_message_refused_when_peer_key_requires_signature() {
        let trust = store(&key_block("alice", 7, "require_signature = true"));
        let out = verify(
            &trust,
            &ReplayCache::new(8),
            Policy::default(),
            T0,
            "alice",
            &sample("alice"),
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureMissing));
    }

    #[test]
    fn req_0007_032_signed_block_verifies_and_reports_sender() {
        let trust = store(&key_block("alice", 7, ""));
        let env = signed("alice", 7, T0);
        assert_eq!(env.block_headers_present(), 4);
        let sender = run(&trust, T0, &env)
            .expect("genuine signed envelope verifies")
            .expect("signed envelope yields a sender");
        assert_eq!(sender.agent(), &agent("alice"));
        assert_eq!(sender.role(), Role::Agent);
        assert_eq!(
            sender.kid().to_string(),
            signer("alice", 7).kid().to_string()
        );
    }

    #[test]
    fn req_0007_032_partial_signature_block_is_signature_invalid() {
        let trust = store(&key_block("alice", 7, ""));
        let base = signed("alice", 7, T0);
        let clears: [fn(&mut Envelope); 4] = [
            |e| e.kid = None,
            |e| e.ts = None,
            |e| e.nonce = None,
            |e| e.signature = None,
        ];
        for clear in clears {
            let mut env = base.clone();
            clear(&mut env);
            assert_eq!(env.block_headers_present(), 3);
            assert_eq!(
                reason_of(run(&trust, T0, &env)),
                Some(Reason::SignatureInvalid)
            );
        }
    }

    #[test]
    fn req_0007_032_malformed_kid_is_signature_invalid_without_kid_metadata() {
        let trust = store(&key_block("alice", 7, ""));
        let mut env = signed("alice", 7, T0);
        env.kid = Some("alice".to_owned());
        let out = run(&trust, T0, &env);
        assert_eq!(reason_of(out.clone()), Some(Reason::SignatureInvalid));
        assert_eq!(kid_of(&out), None);
    }

    #[test]
    fn req_0007_032_sign_refuses_to_replace_an_existing_block() {
        let mut env = signed("alice", 7, T0);
        let err = sign_envelope(
            &signer("alice", 7),
            &ManualClock::new(T0),
            &mut env,
            PAYLOAD,
        )
        .expect_err("a signed envelope cannot be signed again");
        assert!(matches!(err, SecurityError::Invalid(_)));
    }

    #[test]
    fn req_0007_034_signature_covers_every_signed_header() {
        let trust = store(&key_block("alice", 7, ""));
        let base = signed("alice", 7, T0);
        let tampers: [fn(&mut Envelope); 5] = [
            |e| e.subject.push('x'),
            |e| e.extensions = "urn:a2a:ext:z".to_owned(),
            |e| e.origin = "bob.example".to_owned(),
            |e| e.semantic.push(("x".to_owned(), "1".to_owned())),
            |e| e.msg_id = "01J00000000000000000000002".to_owned(),
        ];
        for tamper in tampers {
            let mut env = base.clone();
            tamper(&mut env);
            assert_eq!(
                reason_of(run(&trust, T0, &env)),
                Some(Reason::SignatureInvalid)
            );
        }
    }

    #[test]
    fn req_0007_034_line_break_in_header_value_cannot_be_signed_or_verified() {
        for bad in ["a\nb", "a\rb"] {
            let mut env = sample("alice");
            env.msg_id = bad.to_owned();
            let err = sign_envelope(
                &signer("alice", 7),
                &ManualClock::new(T0),
                &mut env,
                PAYLOAD,
            )
            .expect_err("line break in a header value is refused");
            assert!(matches!(err, SecurityError::Invalid(_)), "{bad:?}");
        }
        let trust = store(&key_block("alice", 7, ""));
        let mut forged = signed("alice", 7, T0);
        forged.msg_id = "x\ny".to_owned();
        assert_eq!(
            reason_of(run(&trust, T0, &forged)),
            Some(Reason::SignatureInvalid)
        );
    }

    fn with_semantic(pairs: &[(&str, &str)]) -> Envelope {
        let mut env = sample("alice");
        env.semantic = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        env
    }

    #[test]
    fn req_0007_034_semantic_keys_are_validated_and_unique() {
        let long_ok = "a".repeat(SP_KEY_MAX);
        let long_bad = "a".repeat(SP_KEY_MAX + 1);
        let bad: [Vec<(&str, &str)>; 6] = [
            vec![("Upper", "x")],
            vec![("a_b", "x")],
            vec![("", "x")],
            vec![(long_bad.as_str(), "x")],
            vec![("dup", "1"), ("dup", "2")],
            vec![("ok", "line\nbreak")],
        ];
        for pairs in &bad {
            let mut env = with_semantic(pairs);
            let err = sign_envelope(
                &signer("alice", 7),
                &ManualClock::new(T0),
                &mut env,
                PAYLOAD,
            )
            .expect_err("invalid semantic header is refused");
            assert!(matches!(err, SecurityError::Invalid(_)), "{pairs:?}");
        }
        let mut good = with_semantic(&[(long_ok.as_str(), "x"), ("b", "2"), ("a", "1")]);
        let trust = store(&key_block("alice", 7, ""));
        sign_envelope(
            &signer("alice", 7),
            &ManualClock::new(T0),
            &mut good,
            PAYLOAD,
        )
        .expect("64-character key and unsorted keys sign");
        assert!(run(&trust, T0, &good).is_ok());
    }

    #[test]
    fn req_0007_035_payload_digest_covers_the_bytes_as_received() {
        let trust = store(&key_block("alice", 7, ""));
        let env = signed("alice", 7, T0);
        for received in [
            b"abc ".as_slice(),
            b"ab".as_slice(),
            b"{\"a\":1}".as_slice(),
        ] {
            let out = verify(
                &trust,
                &ReplayCache::new(8),
                Policy::default(),
                T0,
                "alice",
                &env,
                received,
            );
            assert_eq!(reason_of(out), Some(Reason::SignatureInvalid));
        }
        assert!(run(&trust, T0, &env).is_ok());
    }

    #[test]
    fn req_0007_036_signature_is_canonical_base64url_of_64_bytes() {
        let trust = store(&key_block("alice", 7, ""));
        let base = signed("alice", 7, T0);
        let sig = base.signature.clone().expect("signed");
        let bytes = b64_decode_canonical(&sig).expect("canonical base64url");
        assert_eq!(bytes.len(), 64);

        let mut flipped = bytes.clone();
        flipped[0] ^= 1;
        let mut env = base.clone();
        env.signature = Some(b64_encode(&flipped));
        assert_eq!(
            reason_of(run(&trust, T0, &env)),
            Some(Reason::SignatureInvalid)
        );

        let mut env = base.clone();
        env.signature = Some(format!("{sig}="));
        assert_eq!(
            reason_of(run(&trust, T0, &env)),
            Some(Reason::SignatureInvalid)
        );

        let mut env = base;
        env.signature = Some(b64_encode(&bytes[..63]));
        assert_eq!(
            reason_of(run(&trust, T0, &env)),
            Some(Reason::SignatureInvalid)
        );
    }

    #[test]
    fn req_0007_038_sender_must_be_the_key_agent() {
        let trust = store(&key_block("alice", 7, ""));
        let mut env = signed("alice", 7, T0);
        env.sender = "mallory".to_owned();
        let out = run(&trust, T0, &env);
        assert_eq!(
            reason_of(out.clone()),
            Some(Reason::SignatureIdentityMismatch)
        );
        assert_eq!(
            kid_of(&out),
            Some(signer("alice", 7).kid().to_string()),
            "identity refusals name the key"
        );
    }

    #[test]
    fn req_0007_038_publisher_must_be_the_key_agent() {
        let trust = store(&format!(
            "{}{}",
            key_block("alice", 7, ""),
            key_block("bob", 8, "")
        ));
        let env = signed("alice", 7, T0);
        let out = verify(
            &trust,
            &ReplayCache::new(8),
            Policy::default(),
            T0,
            "bob",
            &env,
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureIdentityMismatch));
    }

    #[test]
    fn req_0007_038_sign_refuses_a_sender_owned_by_another_agent() {
        let mut env = sample("bob");
        let err = sign_envelope(
            &signer("alice", 7),
            &ManualClock::new(T0),
            &mut env,
            PAYLOAD,
        )
        .expect_err("alice cannot sign as bob");
        assert!(matches!(err, SecurityError::Invalid(_)));
        assert_eq!(env.block_headers_present(), 0);
    }

    #[test]
    fn req_0007_039_revoked_key_is_refused_and_names_the_key() {
        let trust = store(&key_block("alice", 7, "status = \"revoked\""));
        let out = run(&trust, T0, &signed("alice", 7, T0));
        assert_eq!(reason_of(out.clone()), Some(Reason::SignatureKeyRevoked));
        assert_eq!(kid_of(&out), Some(signer("alice", 7).kid().to_string()));
    }

    #[test]
    fn req_0007_039_key_outside_its_validity_window_is_inactive() {
        let future = store(&key_block(
            "alice",
            7,
            "not_before = \"2030-01-01T00:00:00Z\"",
        ));
        assert_eq!(
            reason_of(run(&future, T0, &signed("alice", 7, T0))),
            Some(Reason::SignatureKeyInactive)
        );
        let past = store(&key_block(
            "alice",
            7,
            "not_after = \"2024-01-01T00:00:00Z\"",
        ));
        assert_eq!(
            reason_of(run(&past, T0, &signed("alice", 7, T0))),
            Some(Reason::SignatureKeyInactive)
        );
    }

    #[test]
    fn req_0007_039_retired_key_is_refused_at_its_not_after_instant() {
        let na = parse_rfc3339_utc_ms("2030-01-01T00:00:00Z").expect("date");
        let trust = store(&key_block(
            "alice",
            7,
            "status = \"retired\"\nnot_after = \"2030-01-01T00:00:00Z\"",
        ));
        let before = signed("alice", 7, na - 1);
        assert!(run(&trust, na - 1, &before).is_ok());
        assert_eq!(
            reason_of(run(&trust, na, &before)),
            Some(Reason::SignatureKeyInactive)
        );
    }

    #[test]
    fn req_0007_039_key_must_serve_the_receiving_tenant() {
        let trust = store(&key_block("alice", 7, "tenants = [\"other\"]"));
        let out = run(&trust, T0, &signed("alice", 7, T0));
        assert_eq!(reason_of(out.clone()), Some(Reason::SignatureKeyUnknown));
        assert_eq!(
            kid_of(&out),
            None,
            "tenant mismatch reveals no key metadata"
        );
    }

    #[test]
    fn req_0007_039_unknown_kid_is_key_unknown_without_metadata() {
        let trust = store(&key_block("alice", 7, ""));
        let out = run(&trust, T0, &signed("mallory", 9, T0));
        assert_eq!(reason_of(out.clone()), Some(Reason::SignatureKeyUnknown));
        assert_eq!(kid_of(&out), None);
    }

    #[test]
    fn req_0007_039_operator_key_cannot_send_peer_traffic() {
        let trust = store(&key_block_with_role("operator", 7, "operator", ""));
        let out = run(&trust, T0, &signed("operator", 7, T0));
        assert_eq!(reason_of(out.clone()), Some(Reason::KeyRoleDenied));
        assert_eq!(kid_of(&out), Some(signer("operator", 7).kid().to_string()));
    }

    #[test]
    fn req_0007_040_skew_window_is_inclusive_at_the_default_bound() {
        let trust = store(&key_block("alice", 7, ""));
        let env = signed("alice", 7, T0);
        for at in [T0 + 300_000, T0 - 300_000] {
            assert!(run(&trust, at, &env).is_ok(), "accepted at {at}");
        }
        for at in [T0 + 300_001, T0 - 300_001] {
            assert_eq!(
                reason_of(run(&trust, at, &env)),
                Some(Reason::SignatureTimestampSkew),
                "refused at {at}"
            );
        }
    }

    #[test]
    fn req_0007_040_skew_bound_is_per_key() {
        let trust = store(&key_block("alice", 7, "max_skew_ms = 1000"));
        let env = signed("alice", 7, T0);
        assert!(run(&trust, T0 + 1000, &env).is_ok());
        assert_eq!(
            reason_of(run(&trust, T0 + 1001, &env)),
            Some(Reason::SignatureTimestampSkew)
        );
    }

    #[test]
    fn req_0007_040_timestamp_must_be_rfc3339_with_milliseconds() {
        let trust = store(&key_block("alice", 7, ""));
        let nonce = [1u8; NONCE_LEN];
        for ts in [
            "2025-01-02T03:04:05Z",
            "2025-01-02T03:04:05.006+00:00",
            "2025-01-02 03:04:05.006Z",
        ] {
            let mut env = sample("alice");
            forge(&signer("alice", 7), &mut env, ts, &nonce, PAYLOAD);
            assert_eq!(
                reason_of(run(&trust, T0, &env)),
                Some(Reason::SignatureInvalid),
                "{ts}"
            );
        }
    }

    #[test]
    fn req_0007_041_repeat_inside_window_is_replayed() {
        let trust = store(&key_block("alice", 7, ""));
        let replay = ReplayCache::new(64);
        let env = signed("alice", 7, T0);
        let first = verify(
            &trust,
            &replay,
            Policy::default(),
            T0,
            "alice",
            &env,
            PAYLOAD,
        );
        assert!(first.is_ok());
        let again = verify(
            &trust,
            &replay,
            Policy::default(),
            T0 + 1,
            "alice",
            &env,
            PAYLOAD,
        );
        assert_eq!(reason_of(again), Some(Reason::SignatureReplayed));
    }

    #[test]
    fn req_0007_041_replay_entry_covers_the_full_skew_window_ahead() {
        let trust = store(&key_block("alice", 7, ""));
        let replay = ReplayCache::new(64);
        // Stamped one full skew window ahead of the first verification.
        let env = signed("alice", 7, T0 + 300_000);
        assert!(
            verify(
                &trust,
                &replay,
                Policy::default(),
                T0,
                "alice",
                &env,
                PAYLOAD
            )
            .is_ok()
        );
        let out = verify(
            &trust,
            &replay,
            Policy::default(),
            T0 + 600_000,
            "alice",
            &env,
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureReplayed));
    }

    #[test]
    fn req_0007_041_rejected_forgery_does_not_consume_the_nonce() {
        let trust = store(&key_block("alice", 7, ""));
        let replay = ReplayCache::new(64);
        let genuine = signed("alice", 7, T0);
        let mut forged = genuine.clone();
        forged.msg_id = "01J00000000000000000000009".to_owned();
        let out = verify(
            &trust,
            &replay,
            Policy::default(),
            T0,
            "alice",
            &forged,
            PAYLOAD,
        );
        assert_eq!(reason_of(out), Some(Reason::SignatureInvalid));
        let out = verify(
            &trust,
            &replay,
            Policy::default(),
            T0,
            "alice",
            &genuine,
            PAYLOAD,
        );
        assert!(out.is_ok(), "the genuine message still verifies");
    }

    #[test]
    fn req_0007_041_nonce_must_decode_to_exactly_16_bytes() {
        let trust = store(&key_block("alice", 7, ""));
        for len in [15usize, 17] {
            let mut env = sample("alice");
            forge(
                &signer("alice", 7),
                &mut env,
                "2025-01-02T03:04:05.006Z",
                &vec![1u8; len],
                PAYLOAD,
            );
            assert_eq!(
                reason_of(run(&trust, T0, &env)),
                Some(Reason::SignatureInvalid),
                "{len}-byte nonce"
            );
        }
        let mut env = sample("alice");
        forge(
            &signer("alice", 7),
            &mut env,
            "2025-01-02T03:04:05.006Z",
            &[1u8; NONCE_LEN],
            PAYLOAD,
        );
        assert!(run(&trust, T0, &env).is_ok());
    }

    #[test]
    fn req_0007_046_origin_is_honored_only_from_a_gateway_signer() {
        let trust = store(&format!(
            "{}{}",
            key_block_with_role("gateway", 9, "gateway", ""),
            key_block("alice", 7, "")
        ));
        let mut gate_msg = sample("gateway");
        gate_msg.origin = "bob.example".to_owned();
        let gate_env = sign_with(gate_msg, "gateway", 9, T0);
        let sender = verify(
            &trust,
            &ReplayCache::new(8),
            Policy::default(),
            T0,
            "gateway",
            &gate_env,
            PAYLOAD,
        )
        .expect("gateway envelope verifies");
        assert_eq!(
            honored_origin(sender.as_ref(), &gate_env.origin),
            Some("bob.example")
        );

        let mut agent_msg = sample("alice");
        agent_msg.origin = "bob.example".to_owned();
        let agent_env = sign_with(agent_msg, "alice", 7, T0);
        let sender = run(&trust, T0, &agent_env).expect("agent envelope verifies");
        assert_eq!(honored_origin(sender.as_ref(), &agent_env.origin), None);

        let mut empty = sample("gateway");
        empty.origin = String::new();
        let empty_env = sign_with(empty, "gateway", 9, T0);
        let sender = verify(
            &trust,
            &ReplayCache::new(8),
            Policy::default(),
            T0,
            "gateway",
            &empty_env,
            PAYLOAD,
        )
        .expect("gateway envelope verifies");
        assert_eq!(honored_origin(sender.as_ref(), ""), None);
        assert_eq!(honored_origin(None, "bob.example"), None);
    }

    #[test]
    fn req_0007_032_block_header_count_reflects_presence() {
        let mut env = sample("alice");
        assert_eq!(env.block_headers_present(), 0);
        env.kid = Some("alice:AAAAAAAAAAA".to_owned());
        env.signature = Some(String::new());
        assert_eq!(env.block_headers_present(), 2);
    }
}
