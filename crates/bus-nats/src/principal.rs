//! Principal derivation (RFC-0010 0010-078). The NATS binding has no
//! authenticated sender field. A request's principal is the identity in its inbox.
//! A stream message's principal is the identity the binding stamped on it under
//! [`PRINCIPAL_HEADER`] when it was published; a message without that header falls
//! back to the subject, where the server permission model places the identity.

use bus_api::name::Principal;

/// Reserved header carrying the publisher's identity on stream messages. The binding
/// stamps it on every stream publish and append, replacing any caller value, and
/// strips it before delivery. The server does not verify it: a raw NATS client can
/// write it, so it is the binding's own claim about its publishes.
pub(crate) const PRINCIPAL_HEADER: &str = "a2a-principal";

/// Inbox prefix of an identity: `_INBOX.<identity>`.
pub(crate) fn inbox_prefix(identity: &str) -> String {
    format!("_INBOX.{identity}")
}

/// Principal of a request, taken from its reply subject `_INBOX.<identity>.<rest>`.
/// Returns `None` when the reply subject is absent or does not carry an identity.
pub(crate) fn principal_from_inbox(reply: &str) -> Option<Principal> {
    let rest = reply.strip_prefix("_INBOX.")?;
    let identity = rest.split('.').next()?;
    if identity.is_empty() || rest.len() == identity.len() {
        return None;
    }
    Principal::named(identity).ok()
}

/// Principal of a stream message, taken from subject token 4 of
/// `a2a.v1.<tenant>.ev.<agent>.<task>` and `a2a.v1.<tenant>.audit.<identity>`.
/// Returns `None` for any other subject shape.
pub(crate) fn principal_from_subject(subject: &str) -> Option<Principal> {
    let mut tokens = subject.split('.');
    if tokens.next()? != "a2a" || tokens.next()? != "v1" {
        return None;
    }
    let _tenant = tokens.next()?;
    let kind = tokens.next()?;
    if kind != "ev" && kind != "audit" {
        return None;
    }
    let who = tokens.next()?;
    Principal::named(who).ok()
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // 0010-078: the principal of a request is the identity in its inbox.
    #[test]
    fn req_0010_078_principal_comes_from_inbox() {
        let p = principal_from_inbox("_INBOX.omp.abc123").expect("principal");
        assert_eq!(p.name(), Some("omp"));
    }

    // 0010-078: an inbox without an identity yields no principal.
    #[test]
    fn req_0010_078_inbox_without_identity_is_refused() {
        assert!(principal_from_inbox("_INBOX.").is_none());
        assert!(principal_from_inbox("_INBOX.omp").is_none());
        assert!(principal_from_inbox("reply.omp.abc").is_none());
        assert!(principal_from_inbox("_INBOX.Bad Name.x").is_none());
    }

    // 0010-078 covers request principals only. Stream deliveries carry the stamped
    // `a2a-principal` header first (0011-040); subject token 4 is the fallback. This test
    // guards the fallback reading (RFC feedback: principal source for stream deliveries).
    #[test]
    fn req_0010_078_subject_principal_for_events_and_audit() {
        let ev =
            principal_from_subject("a2a.v1.local.ev.claude.0190f3a2-0000-7000-8000-000000000000");
        assert_eq!(
            ev.and_then(|p| p.name().map(str::to_owned)).as_deref(),
            Some("claude")
        );
        let audit = principal_from_subject("a2a.v1.local.audit.tui");
        assert_eq!(
            audit.and_then(|p| p.name().map(str::to_owned)).as_deref(),
            Some("tui")
        );
    }

    #[test]
    fn req_0010_078_other_subjects_have_no_principal() {
        assert!(principal_from_subject("a2a.v1.local.rpc.omp.send").is_none());
        assert!(principal_from_subject("a2a.v1.local.ev").is_none());
        assert!(principal_from_subject("a2a.v1.local.audit.").is_none());
    }
}
