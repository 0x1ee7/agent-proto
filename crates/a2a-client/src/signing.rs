//! Envelope signing for outgoing requests (RFC-0007 [0007-030..036]).
//!
//! The hook maps the binding headers of an [`Outgoing`] message onto the security layer's
//! [`Envelope`], signs it with the caller's key, and writes the four signature-block headers
//! (`A2AB-Kid`, `A2AB-Ts`, `A2AB-Nonce`, `A2AB-Sig`) back onto the message.

use std::sync::Arc;

use a2a_core::envelope::{Clock, Outgoing};
use a2a_core::headers as hdr;
use a2a_security::clock::Clock as SecurityClock;
use a2a_security::envelope::{Envelope, sign_envelope};
use a2a_security::identity::Signer;

use crate::config::SigningHook;
use crate::error::ClientError;

/// Builds a signing hook that signs every outgoing message as the signer's agent.
///
/// The clock is read once per message; an unreadable clock refuses the message
/// ([`ClientError::Clock`]) instead of signing a bad timestamp.
#[must_use]
pub fn envelope_hook(signer: Arc<dyn Signer>, clock: Arc<dyn Clock>) -> Arc<dyn SigningHook> {
    Arc::new(EnvelopeSigner { signer, clock })
}

struct EnvelopeSigner {
    signer: Arc<dyn Signer>,
    clock: Arc<dyn Clock>,
}

impl SigningHook for EnvelopeSigner {
    fn before_send(&self, outgoing: &mut Outgoing) -> Result<(), ClientError> {
        let now = self.clock.now_ms().ok_or(ClientError::Clock)?;
        let now = i64::try_from(now).map_err(|_| ClientError::Clock)?;
        let mut envelope = envelope_of(outgoing);
        sign_envelope(
            self.signer.as_ref(),
            &FixedClock(now),
            &mut envelope,
            &outgoing.payload,
        )
        .map_err(ClientError::Security)?;
        let (Some(kid), Some(ts), Some(nonce), Some(sig)) = (
            envelope.kid,
            envelope.ts,
            envelope.nonce,
            envelope.signature,
        ) else {
            return Err(ClientError::Hook("signature block incomplete"));
        };
        outgoing
            .headers
            .insert(hdr::KID, &kid)
            .map_err(ClientError::Name)?;
        outgoing
            .headers
            .insert(hdr::TS, &ts)
            .map_err(ClientError::Name)?;
        outgoing
            .headers
            .insert(hdr::NONCE, &nonce)
            .map_err(ClientError::Name)?;
        outgoing
            .headers
            .insert(hdr::SIG, &sig)
            .map_err(ClientError::Name)?;
        Ok(())
    }
}

/// Maps the binding headers of `outgoing` onto the signed fields; absent headers are empty.
fn envelope_of(outgoing: &Outgoing) -> Envelope {
    let headers = &outgoing.headers;
    let value = |name: &str| headers.get(name).unwrap_or_default().to_owned();
    let semantic = headers
        .iter()
        .filter_map(|(name, text)| {
            name.strip_prefix(hdr::SP_PREFIX)
                .map(|key| (key.to_owned(), text.to_owned()))
        })
        .collect();
    Envelope {
        subject: outgoing.subject.as_str().to_owned(),
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
        ..Envelope::default()
    }
}

/// The time sampled once for one message, as the security layer reads it.
struct FixedClock(i64);

impl SecurityClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use a2a_security::identity::Ed25519Signer;
    use a2a_types::ids::AgentId;
    use bus_api::headers::Headers;
    use bus_api::name::Subject;
    use bytes::Bytes;

    struct Frozen(Option<u64>);

    impl Clock for Frozen {
        fn now_ms(&self) -> Option<u64> {
            self.0
        }
    }

    fn outgoing(sender: &str) -> Outgoing {
        let mut headers = Headers::new();
        headers
            .insert(hdr::MSG_ID, "0190f000-0000-7000-8000-000000000001")
            .unwrap();
        headers.insert(hdr::SENDER, sender).unwrap();
        headers.insert("A2AB-SP-tenant-hint", "t1").unwrap();
        Outgoing {
            subject: Subject::parse("a2a.v1.t1.rpc.worker-a.send").unwrap(),
            headers,
            payload: Bytes::from_static(b"payload"),
        }
    }

    fn signer(agent: &str) -> Arc<dyn Signer> {
        let agent = AgentId::parse(agent).unwrap();
        Arc::new(Ed25519Signer::from_seed(&agent, &[7u8; 32]))
    }

    /// [0007-032] A signed message carries exactly the four signature-block headers.
    #[test]
    fn req_0007_032_signing_sets_block_headers() {
        let hook = envelope_hook(signer("cli"), Arc::new(Frozen(Some(1_700_000_000_000))));
        let mut message = outgoing("cli");
        hook.before_send(&mut message).unwrap();
        for name in [hdr::KID, hdr::TS, hdr::NONCE, hdr::SIG] {
            assert!(message.headers.get(name).is_some(), "missing {name}");
        }
        assert_eq!(
            message.headers.get(hdr::KID).unwrap().split(':').next(),
            Some("cli")
        );
    }

    /// [0007-035] The signature covers the payload digest, so a different payload gives a different signature.
    #[test]
    fn req_0007_035_signature_depends_on_payload() {
        let hook = envelope_hook(signer("cli"), Arc::new(Frozen(Some(1))));
        let mut first = outgoing("cli");
        let mut second = outgoing("cli");
        second.payload = Bytes::from_static(b"other");
        hook.before_send(&mut first).unwrap();
        hook.before_send(&mut second).unwrap();
        assert_ne!(first.headers.get(hdr::SIG), second.headers.get(hdr::SIG));
    }

    /// [0007-038] A sender that is not the signing agent is refused before anything is sent.
    #[test]
    fn req_0007_038_foreign_sender_is_refused() {
        let hook = envelope_hook(signer("cli"), Arc::new(Frozen(Some(1))));
        let mut message = outgoing("someone-else");
        let err = hook.before_send(&mut message).unwrap_err();
        assert!(matches!(err, ClientError::Security(_)), "{err:?}");
        assert!(message.headers.get(hdr::SIG).is_none());
    }

    /// [0007-040] An unreadable clock refuses the message rather than signing a bad time.
    #[test]
    fn req_0007_040_unreadable_clock_is_refused() {
        let hook = envelope_hook(signer("cli"), Arc::new(Frozen(None)));
        let mut message = outgoing("cli");
        let err = hook.before_send(&mut message).unwrap_err();
        assert!(matches!(err, ClientError::Clock), "{err:?}");
        assert!(message.headers.get(hdr::SIG).is_none());
    }
}
