//! Operator-decision signatures at the worker ([0010-079], [0007-095]).
//!
//! With signing enabled, an approval decision is applied only when its envelope verifies under
//! an operator-role key and was relayed by `tui` or `cli` ([0007-038]). The decision is then
//! attributed to the verified operator identity ([0006-014]). With signing disabled the bus
//! sender stands, as before; see `AGENTS.md` for the operator-channel gap that remains.

use a2a_core::headers as hdr;
use a2a_security::clock::Clock;
use a2a_security::envelope::{Envelope, MessageClass, VerifyContext, verify_request};
use a2a_security::error::{BusError, Reason};
use a2a_security::mode::{Policy, SigningMode};
use a2a_security::replay::ReplayCache;
use a2a_security::trust::TrustStore;
use a2a_types::ids::{AgentId, TenantId};
use bus_api::headers::Headers;
use bus_api::message::Request;
use bytes::Bytes;

/// Nonces remembered for verified operator decisions ([0007-041]).
pub(crate) const REPLAY_CAPACITY: usize = 4096;

/// A bus request as the signature check reads it: its envelope fields and payload.
pub(crate) struct Received {
    envelope: Envelope,
    payload: Bytes,
}

impl Received {
    /// Captures the signed view of `request`. Taken before the request is consumed.
    pub(crate) fn of(request: &Request) -> Self {
        Self {
            envelope: envelope_of(request.subject.as_str(), &request.headers),
            payload: request.payload.clone(),
        }
    }
}

/// Maps the bus headers onto the signed envelope fields, as the sending side does
/// ([0007-030]). The signature block is read from the headers that carry it.
pub(crate) fn envelope_of(subject: &str, headers: &Headers) -> Envelope {
    let value = |name: &str| headers.get(name).unwrap_or_default().to_owned();
    let semantic = headers
        .iter()
        .filter_map(|(name, text)| {
            name.strip_prefix(hdr::SP_PREFIX)
                .map(|key| (key.to_owned(), text.to_owned()))
        })
        .collect();
    Envelope {
        subject: subject.to_owned(),
        msg_id: value(hdr::MSG_ID),
        sender: value(hdr::SENDER),
        version: value(hdr::A2A_VERSION),
        extensions: value(hdr::A2A_EXTENSIONS),
        origin: value(hdr::ORIGIN),
        ttl_ms: value(hdr::TTL_MS),
        causation_id: value(hdr::CAUSATION_ID),
        payload_type: value(hdr::PAYLOAD_TYPE),
        after_seq: value(hdr::AFTER_SEQ),
        content_type: value(hdr::CONTENT_TYPE),
        semantic,
        kid: headers.get(hdr::KID).map(str::to_owned),
        ts: headers.get(hdr::TS).map(str::to_owned),
        nonce: headers.get(hdr::NONCE).map(str::to_owned),
        signature: headers.get(hdr::SIG).map(str::to_owned),
    }
}

/// Clock fixed at the worker's current time, so verification and the worker agree.
struct Now(i64);

impl Clock for Now {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// Verifies an operator decision relayed by `publisher` under `trust` and returns the verified
/// operator identity it is attributed to ([0006-014]).
///
/// # Errors
/// Returns the RFC-0007 refusal: `SIGNATURE_MISSING` for an unsigned decision, and the
/// verification reason for a bad signature, a non-operator key, or a relay mismatch.
pub(crate) fn verify_operator_decision(
    trust: &TrustStore,
    replay: &ReplayCache,
    now: u64,
    tenant: &TenantId,
    publisher: &AgentId,
    received: &Received,
) -> Result<String, BusError> {
    // A2AB-Ts rides on every message ([0002-034]), so a message is unsigned when it carries
    // none of the other signature headers. A partial block is left to the verifier.
    let envelope = &received.envelope;
    if envelope.kid.is_none() && envelope.nonce.is_none() && envelope.signature.is_none() {
        return Err(BusError::new(Reason::SignatureMissing));
    }
    let clock = Now(i64::try_from(now).unwrap_or(i64::MAX));
    let ctx = VerifyContext {
        trust,
        clock: &clock,
        policy: Policy {
            signing: SigningMode::Enabled,
            cross_host: false,
        },
        tenant,
    };
    let verified = verify_request(
        &ctx,
        MessageClass::OperatorDecision { publisher },
        replay,
        &received.envelope,
        &received.payload,
    )?;
    verified
        .map(|verified| verified.agent().as_str().to_owned())
        .ok_or_else(|| BusError::new(Reason::SignatureMissing))
}
