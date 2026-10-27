//! Redaction and untrusted-text framing ([0007-124..133], [0009-044]).
//!
//! Inbound text bound for a Backend is validated, redacted and framed as data ([0007-125]).
//! Outbound text that reaches the wire is redacted only and never framed, so the meaning of
//! stored, signed and forwarded bytes is unchanged ([0007-128], [0007-133]).

use crate::error::WorkerError;
use a2a_security::error::SecurityError;
use a2a_security::frame::frame_untrusted;
use a2a_security::redact::redact;
use a2a_types::proto::a2abus::TrustClass;

use crate::backend::InputPart;

/// Redacts credential-shaped substrings of text bound for the wire ([0007-131], [0007-132]).
/// The caller's text is not modified; a new string is returned.
///
/// # Errors
/// Returns [`WorkerError::Invalid`] when the redaction rules cannot run; the caller must not
/// publish the text in that case (fail closed).
pub(crate) fn redact_outbound(text: &str) -> Result<String, WorkerError> {
    redact(text).map_err(|_| WorkerError::Invalid("redaction unavailable"))
}

/// Validates, redacts and frames every text part of a turn's input before it reaches a Backend
/// ([0007-124], [0007-125]). The framing is applied here, adjacent to the model, and only to the
/// copy handed to the Backend ([0007-126], [0007-128]). Every inbound message is treated as
/// untrusted content from its origin ([0007-124]).
///
/// # Errors
/// Returns [`SecurityError`] when a part contains NUL or control characters, exceeds the
/// pre-redaction size limit, or the origin is not a single token; the turn must not start.
pub(crate) fn frame_inputs(
    input: Vec<InputPart>,
    origin: &str,
) -> Result<Vec<InputPart>, SecurityError> {
    input
        .into_iter()
        .map(|part| match part {
            InputPart::Text(text) => Ok(InputPart::Text(frame_untrusted(
                &text,
                TrustClass::RemotePeer,
                origin,
            )?)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use a2a_security::frame::MAX_UNTRUSTED_BYTES;

    use super::*;

    /// High-entropy run of credential shape: at least 32 characters, above 4.5 bits per byte
    /// ([0007-130](f)).
    const SECRET: &str = "aZ3kP9xQ7mL2vB8nR5tY1wE6uI0oH4sJcD9fG";

    fn text_of(parts: &[InputPart]) -> &str {
        match parts.first() {
            Some(InputPart::Text(text)) => text,
            None => "",
        }
    }

    /// [0007-125](a): NUL and control characters are rejected before the Backend sees them.
    #[test]
    fn req_0007_125_rejects_nul_and_control_input() {
        let nul = vec![InputPart::Text("a\0b".to_owned())];
        let bell = vec![InputPart::Text("a\u{7}b".to_owned())];
        assert!(frame_inputs(nul, "peer-a").is_err());
        assert!(frame_inputs(bell, "peer-a").is_err());
    }

    /// [0007-125](b): text over 262144 bytes is rejected, measured before redaction.
    #[test]
    fn req_0007_125_rejects_oversize_input() {
        let over = vec![InputPart::Text("a".repeat(MAX_UNTRUSTED_BYTES + 1))];
        let at_limit = vec![InputPart::Text("a".repeat(MAX_UNTRUSTED_BYTES))];
        assert!(frame_inputs(over, "peer-a").is_err());
        assert!(frame_inputs(at_limit, "peer-a").is_ok());
    }

    /// [0007-125](c), [0007-131]: credentials are replaced before the text is framed.
    #[test]
    fn req_0007_131_redacts_credential_before_framing() {
        let input = vec![InputPart::Text(format!("token {SECRET} end"))];
        let framed = frame_inputs(input, "peer-a").unwrap_or_default();
        let text = text_of(&framed);
        assert!(text.contains("[REDACTED:high-entropy]"));
        assert!(!text.contains(SECRET));
    }

    /// [0007-124], [0007-125](d): inbound text is wrapped as untrusted data and keeps its content.
    #[test]
    fn req_0007_124_inbound_text_is_framed_as_data() {
        let input = vec![InputPart::Text("summarise the report".to_owned())];
        let framed = frame_inputs(input, "peer-a").unwrap_or_default();
        let text = text_of(&framed);
        assert!(text.contains("untrusted data"));
        assert!(text.contains("summarise the report"));
    }

    /// [0007-132], [0007-133]: outbound text is redacted before it reaches the wire.
    #[test]
    fn req_0007_132_outbound_text_is_redacted() {
        let out = redact_outbound(&format!("a {SECRET} b")).unwrap_or_default();
        assert_eq!(out, "a [REDACTED:high-entropy] b");
    }

    /// [0007-128]: outbound text is redacted only, never framed, and clean text is unchanged.
    #[test]
    fn req_0007_128_outbound_text_is_not_framed() {
        let out = redact_outbound("plain artifact text").unwrap_or_default();
        assert_eq!(out, "plain artifact text");
    }
}
