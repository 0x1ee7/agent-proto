//! Wire reasons of RFC-0007 [0007-140..143] and local (non-wire) failures.

use std::collections::HashMap;
use std::fmt;

use a2a_types::proto::rpc;
use pbjson_types::Any;
use prost::Message;

/// Domain of bus-level reasons ([0007-140]).
pub const BUS_DOMAIN: &str = "a2abus.v1";
/// Domain used by RFC-0006 approval refusals ([0006-013]).
pub const PROTOCOL_DOMAIN: &str = "a2a-protocol.org";
const ERROR_INFO_TYPE_URL: &str = "type.googleapis.com/google.rpc.ErrorInfo";

const UNAUTHENTICATED: i32 = 16;
const PERMISSION_DENIED: i32 = 7;
const INVALID_ARGUMENT: i32 = 3;

/// Every reason this crate can put on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    SignatureMissing,
    SignatureInvalid,
    SignatureKeyUnknown,
    SignatureKeyInactive,
    SignatureKeyRevoked,
    SignatureTimestampSkew,
    SignatureReplayed,
    SignatureIdentityMismatch,
    KeyRoleDenied,
    CapabilityInvalid,
    CapabilityExpired,
    CapabilityNotYetValid,
    CapabilityRevoked,
    CapabilityChainInvalid,
    CapabilityScopeDenied,
    CapabilityAudienceMismatch,
    CapabilityTaskMismatch,
    CapabilityUsed,
    ProvenanceInvalid,
    ProvenanceTooDeep,
    ApprovalNotApplicable,
    ApprovalAlreadyResolved,
}

impl Reason {
    /// Wire name of the reason (the `ErrorInfo.reason` value for bus reasons).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SignatureMissing => "SIGNATURE_MISSING",
            Self::SignatureInvalid => "SIGNATURE_INVALID",
            Self::SignatureKeyUnknown => "SIGNATURE_KEY_UNKNOWN",
            Self::SignatureKeyInactive => "SIGNATURE_KEY_INACTIVE",
            Self::SignatureKeyRevoked => "SIGNATURE_KEY_REVOKED",
            Self::SignatureTimestampSkew => "SIGNATURE_TIMESTAMP_SKEW",
            Self::SignatureReplayed => "SIGNATURE_REPLAYED",
            Self::SignatureIdentityMismatch => "SIGNATURE_IDENTITY_MISMATCH",
            Self::KeyRoleDenied => "KEY_ROLE_DENIED",
            Self::CapabilityInvalid => "CAPABILITY_INVALID",
            Self::CapabilityExpired => "CAPABILITY_EXPIRED",
            Self::CapabilityNotYetValid => "CAPABILITY_NOT_YET_VALID",
            Self::CapabilityRevoked => "CAPABILITY_REVOKED",
            Self::CapabilityChainInvalid => "CAPABILITY_CHAIN_INVALID",
            Self::CapabilityScopeDenied => "CAPABILITY_SCOPE_DENIED",
            Self::CapabilityAudienceMismatch => "CAPABILITY_AUDIENCE_MISMATCH",
            Self::CapabilityTaskMismatch => "CAPABILITY_TASK_MISMATCH",
            Self::CapabilityUsed => "CAPABILITY_USED",
            Self::ProvenanceInvalid => "PROVENANCE_INVALID",
            Self::ProvenanceTooDeep => "PROVENANCE_TOO_DEEP",
            Self::ApprovalNotApplicable => "APPROVAL_NOT_APPLICABLE",
            Self::ApprovalAlreadyResolved => "APPROVAL_ALREADY_RESOLVED",
        }
    }

    /// gRPC status code of [0007-141..143].
    #[must_use]
    pub const fn grpc_code(self) -> i32 {
        match self {
            Self::SignatureMissing
            | Self::SignatureInvalid
            | Self::SignatureKeyUnknown
            | Self::SignatureKeyInactive
            | Self::SignatureKeyRevoked
            | Self::SignatureTimestampSkew
            | Self::SignatureReplayed
            | Self::CapabilityInvalid
            | Self::CapabilityExpired
            | Self::CapabilityNotYetValid
            | Self::CapabilityRevoked
            | Self::CapabilityChainInvalid => UNAUTHENTICATED,
            Self::SignatureIdentityMismatch
            | Self::KeyRoleDenied
            | Self::CapabilityScopeDenied
            | Self::CapabilityAudienceMismatch
            | Self::CapabilityTaskMismatch
            | Self::CapabilityUsed => PERMISSION_DENIED,
            Self::ProvenanceInvalid
            | Self::ProvenanceTooDeep
            | Self::ApprovalNotApplicable
            | Self::ApprovalAlreadyResolved => INVALID_ARGUMENT,
        }
    }

    pub const fn is_approval(self) -> bool {
        matches!(
            self,
            Self::ApprovalNotApplicable | Self::ApprovalAlreadyResolved
        )
    }

    /// `(ErrorInfo.reason, ErrorInfo.domain)` per [0007-140] and [0006-013].
    const fn info_reason_domain(self) -> (&'static str, &'static str) {
        if self.is_approval() {
            ("INVALID_PARAMS", PROTOCOL_DOMAIN)
        } else {
            (self.as_str(), BUS_DOMAIN)
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A wire-level refusal. Carries only identifiers that are safe to echo ([0007-137]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusError {
    reason: Reason,
    kid: Option<String>,
    token_id: Option<String>,
    request_id: Option<String>,
}

impl BusError {
    #[must_use]
    pub const fn new(reason: Reason) -> Self {
        Self {
            reason,
            kid: None,
            token_id: None,
            request_id: None,
        }
    }

    #[must_use]
    pub fn with_kid(mut self, kid: impl Into<String>) -> Self {
        self.kid = Some(kid.into());
        self
    }

    #[must_use]
    pub fn with_token_id(mut self, token_id: impl Into<String>) -> Self {
        self.token_id = Some(token_id.into());
        self
    }

    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    #[must_use]
    pub const fn reason(&self) -> Reason {
        self.reason
    }

    /// The kid this refusal concerns, when one was identified ([0007-137]).
    #[must_use]
    pub fn kid(&self) -> Option<&str> {
        self.kid.as_deref()
    }

    /// The `google.rpc.Status` carried on the wire ([0007-140..143]).
    #[must_use]
    pub fn to_status(&self) -> rpc::Status {
        let (reason, domain) = self.reason.info_reason_domain();
        let mut metadata = HashMap::new();
        if let Some(kid) = &self.kid {
            metadata.insert("kid".to_owned(), kid.clone());
        }
        if let Some(token_id) = &self.token_id {
            metadata.insert("tokenId".to_owned(), token_id.clone());
        }
        if let Some(request_id) = &self.request_id {
            metadata.insert("requestId".to_owned(), request_id.clone());
        }
        if self.reason.is_approval() {
            metadata.insert("detail".to_owned(), self.reason.as_str().to_owned());
        }
        let info = rpc::ErrorInfo {
            reason: reason.to_owned(),
            domain: domain.to_owned(),
            metadata,
        };
        let detail = Any {
            type_url: ERROR_INFO_TYPE_URL.to_owned(),
            value: info.encode_to_vec().into(),
        };
        rpc::Status {
            code: self.reason.grpc_code(),
            message: self.reason.as_str().to_owned(),
            details: vec![detail],
        }
    }
}

impl fmt::Display for BusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason.as_str())
    }
}

impl std::error::Error for BusError {}

/// Local failures: storage, trust files, framing, signing. Never sent to peers as-is.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    #[error("i/o failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("key storage: {0}")]
    Storage(String),
    #[error("trust file: {0}")]
    Trust(String),
    #[error("key material: {0}")]
    Key(String),
    #[error("secure randomness unavailable")]
    Random,
    #[error("canonical JSON: {0}")]
    Canonical(&'static str),
    #[error("refused: {0}")]
    Invalid(&'static str),
    #[error("framing refused: {0}")]
    Framing(&'static str),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0007_140_capability_reasons_map_to_grpc_codes() {
        for reason in [
            Reason::CapabilityInvalid,
            Reason::CapabilityExpired,
            Reason::CapabilityNotYetValid,
            Reason::CapabilityRevoked,
            Reason::CapabilityChainInvalid,
        ] {
            assert_eq!(reason.grpc_code(), UNAUTHENTICATED, "{reason:?}");
        }
        for reason in [
            Reason::CapabilityScopeDenied,
            Reason::CapabilityAudienceMismatch,
            Reason::CapabilityTaskMismatch,
            Reason::CapabilityUsed,
        ] {
            assert_eq!(reason.grpc_code(), PERMISSION_DENIED, "{reason:?}");
        }
    }

    #[test]
    fn req_0007_140_identity_mismatch_is_authorization_not_authentication() {
        assert_eq!(
            Reason::SignatureIdentityMismatch.grpc_code(),
            PERMISSION_DENIED
        );
        assert_eq!(Reason::SignatureInvalid.grpc_code(), UNAUTHENTICATED);
    }
}
