//! Signed bus cards ([0007-050..057], [0003-040..043], [0004-039]). A card carries JWS-style
//! signatures under `signatures`. Each signature is Ed25519 over
//! `BASE64URL(protected) "." BASE64URL(JCS(card without signatures))`.

use a2a_types::ids::AgentId;
use serde_json::{Map, Value, json};

use crate::canon::{b64_decode_canonical, b64_encode, jcs, jcs_without};
use crate::clock::Clock;
use crate::error::{BusError, Reason, SecurityError};
use crate::identity::{Kid, Signer, Verifier};
use crate::trust::{Role, TrustStore};

const SIGNATURES: &str = "signatures";
const FORBIDDEN_HEADER_MEMBERS: [&str; 4] = ["jku", "jwk", "x5c", "x5u"];

/// A signature that verified ([0007-053]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedCard {
    index: usize,
    kid: Kid,
}

impl VerifiedCard {
    /// Position of the verified entry in `signatures`.
    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Key that produced the verified signature.
    #[must_use]
    pub const fn kid(&self) -> &Kid {
        &self.kid
    }
}

/// One signature that did not verify. Ignored when another signature verifies ([0007-054]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRejection {
    pub index: usize,
    pub reason: BusError,
}

/// Result of verifying every signature on a card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardOutcome {
    verified: Option<VerifiedCard>,
    rejected: Vec<CardRejection>,
}

impl CardOutcome {
    /// First signature that verified, if any.
    #[must_use]
    pub const fn verified(&self) -> Option<&VerifiedCard> {
        self.verified.as_ref()
    }

    /// Every signature that failed, in card order.
    #[must_use]
    pub fn rejected(&self) -> &[CardRejection] {
        &self.rejected
    }

    /// Gate for routing and trust decisions ([0007-053]). Without a verified signature this
    /// returns the first rejection reason, or `SIGNATURE_MISSING` when the card has none.
    pub fn require_verified(&self) -> Result<&VerifiedCard, BusError> {
        match &self.verified {
            Some(verified) => Ok(verified),
            None => Err(self.rejected.first().map_or_else(
                || BusError::new(Reason::SignatureMissing),
                |rejection| rejection.reason.clone(),
            )),
        }
    }
}

/// RFC 8785 bytes of the card with `signatures` removed ([0007-052]).
pub fn card_payload(card: &Value) -> Result<Vec<u8>, SecurityError> {
    let object = card
        .as_object()
        .ok_or(SecurityError::Invalid("card is not a JSON object"))?;
    jcs_without(object, SIGNATURES)
}

/// Verifies every signature on `card` against the pinned `expected` agent ([0004-039]).
/// A card without `signatures` yields an empty outcome. Malformed card structure is an `Err`;
/// signature failures are recorded in the outcome.
pub fn verify_card(
    card: &Value,
    expected: &AgentId,
    trust: &TrustStore,
    clock: &dyn Clock,
) -> Result<CardOutcome, SecurityError> {
    let object = card
        .as_object()
        .ok_or(SecurityError::Invalid("card is not a JSON object"))?;
    let signatures = match object.get(SIGNATURES) {
        None => return Ok(CardOutcome::default()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(SecurityError::Invalid("signatures is not an array")),
    };
    let payload = card_payload(card)?;
    let now = clock.now_ms();
    let mut outcome = CardOutcome::default();
    for (index, item) in signatures.iter().enumerate() {
        match verify_one(item, &payload, expected, trust, now) {
            Ok(kid) => {
                if outcome.verified.is_none() {
                    outcome.verified = Some(VerifiedCard { index, kid });
                }
            }
            Err(reason) => outcome.rejected.push(CardRejection { index, reason }),
        }
    }
    Ok(outcome)
}

/// Appends one signature to `card` using `signer`. The publisher's key must be an active agent
/// key in `trust` at `clock` ([0003-041]); expired or revoked keys are refused.
pub fn sign_card(
    card: &mut Value,
    signer: &dyn Signer,
    trust: &TrustStore,
    clock: &dyn Clock,
) -> Result<(), SecurityError> {
    let kid = signer.kid().to_string();
    let entry = trust.entry(&kid).ok_or(SecurityError::Invalid(
        "signing key is not in the trust file",
    ))?;
    if entry.role() != Role::Agent {
        return Err(SecurityError::Invalid("signing key role is not agent"));
    }
    entry
        .check_validity(clock.now_ms())
        .map_err(|error| SecurityError::Invalid(error.reason().as_str()))?;
    let payload = card_payload(card)?;
    let header = json!({"alg": "EdDSA", "kid": kid, "typ": "JOSE"});
    let protected = b64_encode(&jcs(&header)?);
    let input = signing_input(&protected, &payload);
    let signature = b64_encode(&signer.sign_bytes(&input));
    let object = card
        .as_object_mut()
        .ok_or(SecurityError::Invalid("card is not a JSON object"))?;
    let slot = object
        .entry(SIGNATURES)
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(items) = slot else {
        return Err(SecurityError::Invalid("signatures is not an array"));
    };
    items.push(json!({"protected": protected, "signature": signature}));
    Ok(())
}

fn verify_one(
    item: &Value,
    payload: &[u8],
    expected: &AgentId,
    trust: &TrustStore,
    now_ms: i64,
) -> Result<Kid, BusError> {
    let invalid = || BusError::new(Reason::SignatureInvalid);
    let member = item.as_object().ok_or_else(invalid)?;
    let protected = member
        .get("protected")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    let signature_text = member
        .get("signature")
        .and_then(Value::as_str)
        .ok_or_else(invalid)?;
    if let Some(unprotected) = member.get("header") {
        let unprotected = unprotected.as_object().ok_or_else(invalid)?;
        if has_forbidden_member(unprotected) {
            return Err(invalid());
        }
    }
    let header = protected_header(protected).ok_or_else(invalid)?;
    if has_forbidden_member(&header) {
        return Err(invalid());
    }
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err(invalid());
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .and_then(Kid::parse)
        .ok_or_else(invalid)?;
    let kid_text = kid.to_string();
    let with_kid = |reason: Reason| BusError::new(reason).with_kid(kid_text.clone());
    let entry = trust
        .entry(&kid_text)
        .ok_or_else(|| with_kid(Reason::SignatureKeyUnknown))?;
    entry.check_validity(now_ms)?;
    if entry.role() != Role::Agent {
        return Err(with_kid(Reason::KeyRoleDenied));
    }
    if entry.agent() != expected || kid.agent() != expected {
        return Err(with_kid(Reason::SignatureIdentityMismatch));
    }
    let signature = b64_decode_canonical(signature_text)
        .filter(|bytes| bytes.len() == 64)
        .ok_or_else(|| with_kid(Reason::SignatureInvalid))?;
    let input = signing_input(protected, payload);
    if !entry.verify_bytes(&input, &signature) {
        return Err(with_kid(Reason::SignatureInvalid));
    }
    Ok(kid)
}

fn signing_input(protected: &str, payload: &[u8]) -> Vec<u8> {
    let mut input = protected.as_bytes().to_vec();
    input.push(b'.');
    input.extend_from_slice(b64_encode(payload).as_bytes());
    input
}

fn protected_header(protected: &str) -> Option<Map<String, Value>> {
    let bytes = b64_decode_canonical(protected)?;
    match serde_json::from_slice::<Value>(&bytes).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

fn has_forbidden_member(object: &Map<String, Value>) -> bool {
    FORBIDDEN_HEADER_MEMBERS
        .iter()
        .any(|name| object.contains_key(*name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::clock::ManualClock;
    use crate::testkit::{agent, key_block, key_block_with_role, signer, store};

    const NOW: i64 = 1_800_000_000_000;
    const PAYLOAD: &[u8] = br#"{"a":"x","agent":"alice","b":1,"name":"alice"}"#;

    fn clock() -> ManualClock {
        ManualClock::new(NOW)
    }

    fn card() -> Value {
        json!({"name": "alice", "agent": "alice", "b": 1, "a": "x"})
    }

    fn alice_store() -> TrustStore {
        store(&key_block("alice", 7, ""))
    }

    fn alice_kid() -> String {
        signer("alice", 7).kid().to_string()
    }

    fn sign_alice(card: &mut Value) {
        sign_card(card, &signer("alice", 7), &alice_store(), &clock()).expect("sign");
    }

    fn header(kid: &str) -> Value {
        json!({"alg": "EdDSA", "kid": kid, "typ": "JOSE"})
    }

    /// Builds a signature entry over `card` with an arbitrary protected header, bypassing
    /// `sign_card` so tests can produce headers the publisher must never emit.
    fn forged_entry(card: &Value, header: &Value, key: &dyn Signer) -> Value {
        let payload = card_payload(card).expect("payload");
        let protected = b64_encode(&jcs(header).expect("header"));
        let input = signing_input(&protected, &payload);
        let signature = b64_encode(&key.sign_bytes(&input));
        json!({"protected": protected, "signature": signature})
    }

    fn push(card: &mut Value, entry: Value) {
        let object = card.as_object_mut().expect("object");
        let slot = object
            .entry(SIGNATURES)
            .or_insert_with(|| Value::Array(Vec::new()));
        slot.as_array_mut().expect("array").push(entry);
    }

    fn verify_as(card: &Value, expected: &str, trust: &TrustStore) -> CardOutcome {
        verify_card(card, &agent(expected), trust, &clock()).expect("well-formed card")
    }

    #[test]
    fn req_0007_052_payload_is_jcs_of_card_without_signatures() {
        let mut card = card();
        push(&mut card, json!({"protected": "p", "signature": "s"}));
        assert_eq!(card_payload(&card).expect("payload"), PAYLOAD);
    }

    #[test]
    fn req_0007_052_signing_input_golden_literal_with_fixed_seed() {
        let mut card = card();
        sign_alice(&mut card);
        let entry = &card[SIGNATURES][0];
        let protected = entry["protected"].as_str().expect("protected");
        let decoded = b64_decode_canonical(protected).expect("canonical protected header");
        let kid = alice_kid();
        let literal = format!(r#"{{"alg":"EdDSA","kid":"{kid}","typ":"JOSE"}}"#);
        assert_eq!(decoded, literal.as_bytes());
        let input = format!("{protected}.{}", b64_encode(PAYLOAD));
        let signature = b64_decode_canonical(entry["signature"].as_str().expect("sig"))
            .expect("canonical signature");
        assert_eq!(signature.len(), 64);
        let trust = alice_store();
        let key = trust.entry(&kid).expect("entry");
        assert!(key.verify_bytes(input.as_bytes(), &signature));
    }

    #[test]
    fn req_0003_041_protected_header_has_alg_kid_and_typ() {
        let mut card = card();
        sign_alice(&mut card);
        let protected = card[SIGNATURES][0]["protected"]
            .as_str()
            .expect("protected");
        let decoded = b64_decode_canonical(protected).expect("decode");
        let parsed: Value = serde_json::from_slice(&decoded).expect("json");
        assert_eq!(parsed, header(&alice_kid()));
    }

    #[test]
    fn req_0007_053_valid_signature_verifies_for_pinned_agent() {
        let mut card = card();
        sign_alice(&mut card);
        let outcome = verify_as(&card, "alice", &alice_store());
        let verified = outcome.require_verified().expect("verified");
        assert_eq!(verified.index(), 0);
        assert_eq!(verified.kid().to_string(), alice_kid());
        assert!(outcome.rejected().is_empty());
    }

    #[test]
    fn req_0003_042_unsigned_card_has_no_verified_signature() {
        let outcome = verify_as(&card(), "alice", &alice_store());
        assert!(outcome.verified().is_none());
        assert!(outcome.rejected().is_empty());
        let error = outcome.require_verified().expect_err("unsigned");
        assert_eq!(error.reason(), Reason::SignatureMissing);
    }

    #[test]
    fn req_0007_052_any_change_to_the_card_breaks_the_signature() {
        let mut card = card();
        sign_alice(&mut card);
        card["a"] = json!("tampered");
        let outcome = verify_as(&card, "alice", &alice_store());
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureInvalid
        );
    }

    #[test]
    fn req_0007_053_signature_from_unpinned_agent_is_identity_mismatch() {
        let mut card = card();
        sign_alice(&mut card);
        let outcome = verify_as(&card, "bob", &alice_store());
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureIdentityMismatch
        );
    }

    #[test]
    fn req_0007_053_signature_from_unknown_kid_is_key_unknown() {
        let mut card = card();
        let carol = signer("carol", 3);
        let entry = forged_entry(&card, &header(&carol.kid().to_string()), &carol);
        push(&mut card, entry);
        let outcome = verify_as(&card, "carol", &alice_store());
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureKeyUnknown
        );
    }

    #[test]
    fn req_0007_053_operator_key_on_card_is_key_role_denied() {
        let trust = store(&key_block_with_role("operator", 9, "operator", ""));
        let ops = signer("operator", 9);
        let mut card = json!({"name": "operator"});
        let entry = forged_entry(&card, &header(&ops.kid().to_string()), &ops);
        push(&mut card, entry);
        let outcome = verify_as(&card, "operator", &trust);
        assert!(outcome.verified().is_none());
        assert_eq!(outcome.rejected()[0].reason.reason(), Reason::KeyRoleDenied);
    }

    #[test]
    fn req_0007_055_revoked_key_signature_is_refused() {
        let trust = store(&key_block("alice", 7, "status = \"revoked\""));
        let mut card = card();
        let entry = forged_entry(&card, &header(&alice_kid()), &signer("alice", 7));
        push(&mut card, entry);
        let outcome = verify_as(&card, "alice", &trust);
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureKeyRevoked
        );
    }

    #[test]
    fn req_0007_055_retired_key_past_not_after_is_refused() {
        let body = "status = \"retired\"\nnot_after = \"2020-01-01T00:00:00Z\"";
        let trust = store(&key_block("alice", 7, body));
        let mut card = card();
        let entry = forged_entry(&card, &header(&alice_kid()), &signer("alice", 7));
        push(&mut card, entry);
        let outcome = verify_as(&card, "alice", &trust);
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureKeyInactive
        );
    }

    #[test]
    fn req_0007_054_failing_signature_is_ignored_when_another_verifies() {
        let mut card = card();
        push(&mut card, json!({"protected": "e30", "signature": "AAAA"}));
        sign_alice(&mut card);
        let outcome = verify_as(&card, "alice", &alice_store());
        let verified = outcome
            .require_verified()
            .expect("second signature verifies");
        assert_eq!(verified.index(), 1);
        assert_eq!(outcome.rejected().len(), 1);
        assert_eq!(outcome.rejected()[0].index, 0);
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureInvalid
        );
    }

    #[test]
    fn req_0007_051_forbidden_header_members_reject_the_signature() {
        for member in ["jku", "jwk", "x5c", "x5u"] {
            let mut bad = header(&alice_kid());
            bad[member] = json!("https://example.invalid/key");
            let mut card = card();
            let entry = forged_entry(&card, &bad, &signer("alice", 7));
            push(&mut card, entry);
            let outcome = verify_as(&card, "alice", &alice_store());
            assert!(outcome.verified().is_none(), "{member} accepted");
            assert_eq!(
                outcome.rejected()[0].reason.reason(),
                Reason::SignatureInvalid,
                "{member}"
            );
        }
    }

    #[test]
    fn req_0007_051_forbidden_member_in_unprotected_header_rejects_the_signature() {
        let mut card = card();
        let mut entry = forged_entry(&card, &header(&alice_kid()), &signer("alice", 7));
        entry["header"] = json!({"x5u": "https://example.invalid/chain"});
        push(&mut card, entry);
        let outcome = verify_as(&card, "alice", &alice_store());
        assert!(outcome.verified().is_none());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureInvalid
        );
    }

    #[test]
    fn req_0007_051_alg_must_be_eddsa_and_kid_is_required() {
        let mut bad_alg = header(&alice_kid());
        bad_alg["alg"] = json!("HS256");
        let mut card_alg = card();
        let entry = forged_entry(&card_alg, &bad_alg, &signer("alice", 7));
        push(&mut card_alg, entry);
        let outcome = verify_as(&card_alg, "alice", &alice_store());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureInvalid
        );

        let no_kid = json!({"alg": "EdDSA", "typ": "JOSE"});
        let mut card_kid = card();
        let entry = forged_entry(&card_kid, &no_kid, &signer("alice", 7));
        push(&mut card_kid, entry);
        let outcome = verify_as(&card_kid, "alice", &alice_store());
        assert_eq!(
            outcome.rejected()[0].reason.reason(),
            Reason::SignatureInvalid
        );
    }

    #[test]
    fn req_0007_050_malformed_structure_is_an_error_not_an_outcome() {
        let trust = alice_store();
        assert!(verify_card(&json!([1, 2]), &agent("alice"), &trust, &clock()).is_err());
        assert!(
            verify_card(
                &json!({"signatures": "x"}),
                &agent("alice"),
                &trust,
                &clock()
            )
            .is_err()
        );
    }

    #[test]
    fn req_0007_057_signing_changes_only_the_signatures_member() {
        let before = card();
        let mut after = before.clone();
        sign_alice(&mut after);
        let strip = |value: &Value| {
            let mut copy = value.clone();
            copy.as_object_mut().expect("object").remove(SIGNATURES);
            copy
        };
        assert_eq!(strip(&before), strip(&after));
        assert_eq!(after[SIGNATURES].as_array().expect("array").len(), 1);
    }

    #[test]
    fn req_0003_040_publisher_signs_with_its_agent_key() {
        let mut card = card();
        sign_alice(&mut card);
        let outcome = verify_as(&card, "alice", &alice_store());
        assert_eq!(
            outcome
                .require_verified()
                .expect("verified")
                .kid()
                .agent()
                .as_str(),
            "alice"
        );
    }

    #[test]
    fn req_0003_041_publisher_refuses_revoked_or_expired_key() {
        let revoked = store(&key_block("alice", 7, "status = \"revoked\""));
        let mut card = card();
        assert!(matches!(
            sign_card(&mut card, &signer("alice", 7), &revoked, &clock()),
            Err(SecurityError::Invalid("SIGNATURE_KEY_REVOKED"))
        ));
        let expired = store(&key_block(
            "alice",
            7,
            "status = \"retired\"\nnot_after = \"2020-01-01T00:00:00Z\"",
        ));
        assert!(matches!(
            sign_card(&mut card, &signer("alice", 7), &expired, &clock()),
            Err(SecurityError::Invalid("SIGNATURE_KEY_INACTIVE"))
        ));
        assert!(card.get(SIGNATURES).is_none());
    }

    #[test]
    fn req_0004_039_card_is_checked_against_the_pinned_agent_only() {
        let mut card = card();
        sign_alice(&mut card);
        assert!(
            verify_as(&card, "alice", &alice_store())
                .require_verified()
                .is_ok()
        );
        assert!(
            verify_as(&card, "bob", &alice_store())
                .require_verified()
                .is_err()
        );
    }
}
