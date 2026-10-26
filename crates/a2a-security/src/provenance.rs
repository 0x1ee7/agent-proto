//! Provenance metadata for messages and artifacts ([0007-120..124]). Hops are append-only;
//! content digests cover the JCS form of the `parts` array as received.

use a2a_types::proto::a2abus::{HopRecord, Provenance, ReceivedVia, TrustClass};
use serde_json::Value;

use crate::canon::{b64_encode, jcs, sha256};
use crate::error::{BusError, Reason, SecurityError};

/// Maximum hop count a message may carry; a ninth hop is refused ([0007-122]).
pub const MAX_HOPS: usize = 8;

/// Appends one hop. Earlier entries are never touched ([0007-121]). Refuses the hop that would
/// make the chain longer than [`MAX_HOPS`] ([0007-122]).
pub fn append_hop(provenance: &mut Provenance, hop: HopRecord) -> Result<(), BusError> {
    if provenance.hops.len() >= MAX_HOPS {
        return Err(BusError::new(Reason::ProvenanceTooDeep));
    }
    provenance.hops.push(hop);
    Ok(())
}

/// `"sha-256:" + base64url(SHA-256(JCS(parts)))` ([0007-123]).
pub fn content_digest(parts: &Value) -> Result<String, SecurityError> {
    let bytes = jcs(parts)?;
    Ok(format!("sha-256:{}", b64_encode(&sha256(&bytes))))
}

/// Builds the origin fields for newly produced content with no hops yet. The producing
/// component appends its own hop with [`append_hop`].
pub fn originate(
    origin_agent: &str,
    origin_kid: &str,
    trust_class: TrustClass,
    received_via: ReceivedVia,
    parts: &Value,
) -> Result<Provenance, SecurityError> {
    if origin_agent.is_empty() {
        return Err(SecurityError::Invalid("origin agent is empty"));
    }
    if trust_class == TrustClass::Unspecified {
        return Err(SecurityError::Invalid("trust class is unspecified"));
    }
    Ok(Provenance {
        origin_agent: origin_agent.to_owned(),
        origin_kid: origin_kid.to_owned(),
        trust_class: trust_class.into(),
        received_via: received_via.into(),
        content_digest: content_digest(parts)?,
        hops: Vec::new(),
    })
}

/// Trust class the gateway assigns to inbound content ([0007-121]).
#[must_use]
pub const fn gateway_trust_class(authenticated_peer: bool) -> TrustClass {
    if authenticated_peer {
        TrustClass::RemotePeer
    } else {
        TrustClass::ExternalHttp
    }
}

/// Anything other than `TRUST_CLASS_OPERATOR` is untrusted input ([0007-124]).
#[must_use]
pub const fn is_untrusted(class: TrustClass) -> bool {
    !matches!(class, TrustClass::Operator)
}

/// Verifies received provenance against the message `parts` and returns its trust class.
/// Rejects more than [`MAX_HOPS`] hops ([0007-122]), unknown or unspecified classes, an empty
/// origin, and a content digest mismatch ([0007-123]).
pub fn verify_provenance(provenance: &Provenance, parts: &Value) -> Result<TrustClass, BusError> {
    if provenance.hops.len() > MAX_HOPS {
        return Err(BusError::new(Reason::ProvenanceTooDeep));
    }
    let trust_class = TrustClass::try_from(provenance.trust_class)
        .map_err(|_| BusError::new(Reason::ProvenanceInvalid))?;
    if trust_class == TrustClass::Unspecified {
        return Err(BusError::new(Reason::ProvenanceInvalid));
    }
    ReceivedVia::try_from(provenance.received_via)
        .map_err(|_| BusError::new(Reason::ProvenanceInvalid))?;
    if provenance.origin_agent.is_empty() {
        return Err(BusError::new(Reason::ProvenanceInvalid));
    }
    if !provenance.content_digest.is_empty() {
        let expected =
            content_digest(parts).map_err(|_| BusError::new(Reason::ProvenanceInvalid))?;
        if provenance.content_digest != expected {
            return Err(BusError::new(Reason::ProvenanceInvalid));
        }
    }
    Ok(trust_class)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use a2a_types::proto::a2abus::HopRecord;
    use serde_json::json;

    use super::*;

    fn hop(n: usize) -> HopRecord {
        HopRecord {
            agent: format!("agent{n}"),
            kid: String::new(),
            ts: "2026-01-01T00:00:00.000Z".to_owned(),
            task_id: "task-1".to_owned(),
        }
    }

    fn parts() -> Value {
        json!([{"text": "hi"}])
    }

    fn provenance_with(hops: usize) -> Provenance {
        let mut provenance = originate(
            "alice",
            "alice:K",
            TrustClass::LocalAgent,
            ReceivedVia::Bus,
            &parts(),
        )
        .expect("originate");
        for n in 0..hops {
            append_hop(&mut provenance, hop(n)).expect("hop");
        }
        provenance
    }

    fn reason_of(result: Result<TrustClass, BusError>) -> Reason {
        result.expect_err("must be rejected").reason()
    }

    #[test]
    fn req_0007_120_originate_sets_origin_fields_and_digest() {
        let provenance = provenance_with(0);
        assert_eq!(provenance.origin_agent, "alice");
        assert_eq!(provenance.origin_kid, "alice:K");
        assert_eq!(provenance.trust_class, TrustClass::LocalAgent as i32);
        assert_eq!(provenance.received_via, ReceivedVia::Bus as i32);
        assert!(provenance.hops.is_empty());
        assert!(provenance.content_digest.starts_with("sha-256:"));
        assert_eq!(provenance.content_digest.len(), "sha-256:".len() + 43);
    }

    #[test]
    fn req_0007_120_originate_refuses_empty_origin_and_unspecified_class() {
        assert!(matches!(
            originate("", "", TrustClass::LocalAgent, ReceivedVia::Bus, &parts()),
            Err(SecurityError::Invalid(_))
        ));
        assert!(matches!(
            originate(
                "alice",
                "",
                TrustClass::Unspecified,
                ReceivedVia::Bus,
                &parts()
            ),
            Err(SecurityError::Invalid(_))
        ));
    }

    #[test]
    fn req_0007_120_verify_rejects_unspecified_and_unknown_trust_class() {
        let mut provenance = provenance_with(0);
        provenance.trust_class = TrustClass::Unspecified as i32;
        assert_eq!(
            reason_of(verify_provenance(&provenance, &parts())),
            Reason::ProvenanceInvalid
        );
        provenance.trust_class = 99;
        assert_eq!(
            reason_of(verify_provenance(&provenance, &parts())),
            Reason::ProvenanceInvalid
        );
    }

    #[test]
    fn req_0007_120_verify_rejects_unknown_received_via_and_empty_origin() {
        let mut provenance = provenance_with(0);
        provenance.received_via = 42;
        assert_eq!(
            reason_of(verify_provenance(&provenance, &parts())),
            Reason::ProvenanceInvalid
        );
        let mut provenance = provenance_with(0);
        provenance.origin_agent.clear();
        assert_eq!(
            reason_of(verify_provenance(&provenance, &parts())),
            Reason::ProvenanceInvalid
        );
    }

    #[test]
    fn req_0007_121_append_hop_keeps_earlier_entries_unchanged() {
        let mut provenance = provenance_with(2);
        let before = provenance.hops.clone();
        append_hop(&mut provenance, hop(9)).expect("third hop");
        assert_eq!(provenance.hops.len(), 3);
        assert_eq!(&provenance.hops[..2], before.as_slice());
        assert_eq!(provenance.hops[2].agent, "agent9");
    }

    #[test]
    fn req_0007_122_ninth_hop_is_refused_with_too_deep() {
        let mut provenance = provenance_with(MAX_HOPS);
        assert_eq!(provenance.hops.len(), MAX_HOPS);
        let error = append_hop(&mut provenance, hop(99)).expect_err("ninth hop");
        assert_eq!(error.reason(), Reason::ProvenanceTooDeep);
        assert_eq!(provenance.hops.len(), MAX_HOPS);
    }

    #[test]
    fn req_0007_122_eight_hops_are_accepted_and_nine_are_rejected_on_receipt() {
        let eight = provenance_with(MAX_HOPS);
        assert!(verify_provenance(&eight, &parts()).is_ok());
        let mut nine = provenance_with(MAX_HOPS);
        nine.hops.push(hop(99));
        assert_eq!(
            reason_of(verify_provenance(&nine, &parts())),
            Reason::ProvenanceTooDeep
        );
    }

    #[test]
    fn req_0007_123_matching_digest_verifies_and_returns_class() {
        let provenance = provenance_with(1);
        assert_eq!(
            verify_provenance(&provenance, &parts()).expect("verify"),
            TrustClass::LocalAgent
        );
    }

    #[test]
    fn req_0007_123_digest_mismatch_is_rejected() {
        let provenance = provenance_with(0);
        assert_eq!(
            reason_of(verify_provenance(
                &provenance,
                &json!([{"text": "changed"}])
            )),
            Reason::ProvenanceInvalid
        );
    }

    #[test]
    fn req_0007_123_empty_digest_is_not_checked() {
        let mut provenance = provenance_with(0);
        provenance.content_digest.clear();
        assert_eq!(
            verify_provenance(&provenance, &json!([{"text": "changed"}])).expect("verify"),
            TrustClass::LocalAgent
        );
    }

    #[test]
    fn req_0007_123_digest_is_over_jcs_form_not_source_bytes() {
        let a = content_digest(&json!([{"a": 2, "b": 1}])).expect("digest");
        let b: Value = serde_json::from_str("[ { \"b\" : 1 ,  \"a\":2 } ]").expect("json");
        assert_eq!(a, content_digest(&b).expect("digest"));
    }

    #[test]
    fn req_0007_123_digest_matches_known_encoding_of_canonical_bytes() {
        let expected = format!("sha-256:{}", b64_encode(&sha256(br#"[{"text":"hi"}]"#)));
        assert_eq!(content_digest(&parts()).expect("digest"), expected);
    }

    #[test]
    fn req_0007_121_gateway_assigns_remote_peer_or_external_http() {
        assert_eq!(gateway_trust_class(true), TrustClass::RemotePeer);
        assert_eq!(gateway_trust_class(false), TrustClass::ExternalHttp);
    }

    #[test]
    fn req_0007_124_only_operator_class_is_trusted() {
        assert!(!is_untrusted(TrustClass::Operator));
        for class in [
            TrustClass::Unspecified,
            TrustClass::LocalAgent,
            TrustClass::RemotePeer,
            TrustClass::ExternalHttp,
            TrustClass::ToolOutput,
            TrustClass::Artifact,
        ] {
            assert!(is_untrusted(class), "{class:?} must be untrusted");
        }
    }
}
