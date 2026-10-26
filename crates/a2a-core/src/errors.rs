//! Wire error model: `google.rpc.Status` replies and their reasons (RFC-0002 section 4.9).
//!
//! Every error reply carries exactly one `ErrorInfo` detail. Messages are short fixed English
//! text; no request body, credentials, host paths or stack traces are ever included.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use a2a_types::proto::rpc;
use bus_api::error::BusError;
use prost::Message as _;

/// Domain of A2A protocol errors (RFC-0002 [0002-091]).
pub const A2A_DOMAIN: &str = "a2a-protocol.org";
/// Domain of A2A-Bus transport errors (RFC-0002 [0002-092]).
pub const BUS_DOMAIN: &str = "a2abus.v1";

/// Type URL of `google.rpc.ErrorInfo` inside `Status.details`.
pub const ERROR_INFO_TYPE_URL: &str = "type.googleapis.com/google.rpc.ErrorInfo";
/// Type URL of `google.rpc.BadRequest` inside `Status.details`.
pub const BAD_REQUEST_TYPE_URL: &str = "type.googleapis.com/google.rpc.BadRequest";
/// Type URL of `google.rpc.RetryInfo` inside `Status.details`.
pub const RETRY_INFO_TYPE_URL: &str = "type.googleapis.com/google.rpc.RetryInfo";

/// A wire-visible error carried in a `google.rpc.Status` reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WireError {
    /// `TASK_NOT_FOUND`, code 5, A2A domain.
    TaskNotFound,
    /// `TASK_NOT_CANCELABLE`, code 9, A2A domain.
    TaskNotCancelable,
    /// `PUSH_NOTIFICATION_NOT_SUPPORTED`, code 9, A2A domain.
    PushNotificationNotSupported,
    /// `UNSUPPORTED_OPERATION`, code 9, A2A domain (operation exists, state or config forbids it).
    UnsupportedOperation,
    /// `INVALID_PARAMS`, code 3, A2A domain.
    InvalidParams,
    /// `CONTENT_TYPE_NOT_SUPPORTED`, code 3, A2A domain.
    ContentTypeNotSupported,
    /// `INVALID_AGENT_RESPONSE`, code 13, A2A domain.
    InvalidAgentResponse,
    /// `EXTENDED_AGENT_CARD_NOT_CONFIGURED`, code 9, A2A domain.
    ExtendedAgentCardNotConfigured,
    /// `EXTENSION_SUPPORT_REQUIRED`, code 9, A2A domain.
    ExtensionSupportRequired,
    /// `VERSION_NOT_SUPPORTED`, code 9, A2A domain.
    VersionNotSupported,
    /// `EXTENSION_DATA_INVALID`, code 3, A2A domain.
    ExtensionDataInvalid,
    /// `UNSUPPORTED_OPERATION`, code 12, A2A domain: the operation token is unknown (RFC-0002 [0002-023]).
    UnknownOperation,
    /// `UNAUTHENTICATED`, code 16, bus domain.
    Unauthenticated,
    /// `PERMISSION_DENIED`, code 7, bus domain.
    PermissionDenied,
    /// `REQUEST_EXPIRED`, code 4, bus domain.
    RequestExpired,
    /// `PAYLOAD_TOO_LARGE`, code 8, bus domain.
    PayloadTooLarge,
    /// `MALFORMED_MESSAGE`, code 3, bus domain.
    MalformedMessage,
    /// `UNSUPPORTED_MEDIA_TYPE`, code 3, bus domain.
    UnsupportedMediaType,
    /// `TENANT_MISMATCH`, code 3, bus domain.
    TenantMismatch,
    /// `OVERLOADED`, code 14, bus domain; carries `RetryInfo`.
    Overloaded,
    /// `DRAINING`, code 14, bus domain.
    Draining,
    /// `DUPLICATE_IN_PROGRESS`, code 10, bus domain; carries `RetryInfo`.
    DuplicateInProgress,
}

impl WireError {
    /// Every wire error, in table order (RFC-0002 [0002-091], [0002-092]).
    pub const ALL: [WireError; 22] = [
        WireError::TaskNotFound,
        WireError::TaskNotCancelable,
        WireError::PushNotificationNotSupported,
        WireError::UnsupportedOperation,
        WireError::InvalidParams,
        WireError::ContentTypeNotSupported,
        WireError::InvalidAgentResponse,
        WireError::ExtendedAgentCardNotConfigured,
        WireError::ExtensionSupportRequired,
        WireError::VersionNotSupported,
        WireError::ExtensionDataInvalid,
        WireError::UnknownOperation,
        WireError::Unauthenticated,
        WireError::PermissionDenied,
        WireError::RequestExpired,
        WireError::PayloadTooLarge,
        WireError::MalformedMessage,
        WireError::UnsupportedMediaType,
        WireError::TenantMismatch,
        WireError::Overloaded,
        WireError::Draining,
        WireError::DuplicateInProgress,
    ];

    /// The `google.rpc` code of this error.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            WireError::TaskNotFound => 5,
            WireError::TaskNotCancelable
            | WireError::PushNotificationNotSupported
            | WireError::UnsupportedOperation
            | WireError::ExtendedAgentCardNotConfigured
            | WireError::ExtensionSupportRequired
            | WireError::VersionNotSupported => 9,
            WireError::InvalidParams
            | WireError::ContentTypeNotSupported
            | WireError::ExtensionDataInvalid
            | WireError::MalformedMessage
            | WireError::UnsupportedMediaType
            | WireError::TenantMismatch => 3,
            WireError::InvalidAgentResponse => 13,
            WireError::UnknownOperation => 12,
            WireError::Unauthenticated => 16,
            WireError::PermissionDenied => 7,
            WireError::RequestExpired => 4,
            WireError::PayloadTooLarge => 8,
            WireError::Overloaded | WireError::Draining => 14,
            WireError::DuplicateInProgress => 10,
        }
    }

    /// The `ErrorInfo.reason` token.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            WireError::TaskNotFound => "TASK_NOT_FOUND",
            WireError::TaskNotCancelable => "TASK_NOT_CANCELABLE",
            WireError::PushNotificationNotSupported => "PUSH_NOTIFICATION_NOT_SUPPORTED",
            WireError::UnsupportedOperation | WireError::UnknownOperation => {
                "UNSUPPORTED_OPERATION"
            }
            WireError::InvalidParams => "INVALID_PARAMS",
            WireError::ContentTypeNotSupported => "CONTENT_TYPE_NOT_SUPPORTED",
            WireError::InvalidAgentResponse => "INVALID_AGENT_RESPONSE",
            WireError::ExtendedAgentCardNotConfigured => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            WireError::ExtensionSupportRequired => "EXTENSION_SUPPORT_REQUIRED",
            WireError::VersionNotSupported => "VERSION_NOT_SUPPORTED",
            WireError::ExtensionDataInvalid => "EXTENSION_DATA_INVALID",
            WireError::Unauthenticated => "UNAUTHENTICATED",
            WireError::PermissionDenied => "PERMISSION_DENIED",
            WireError::RequestExpired => "REQUEST_EXPIRED",
            WireError::PayloadTooLarge => "PAYLOAD_TOO_LARGE",
            WireError::MalformedMessage => "MALFORMED_MESSAGE",
            WireError::UnsupportedMediaType => "UNSUPPORTED_MEDIA_TYPE",
            WireError::TenantMismatch => "TENANT_MISMATCH",
            WireError::Overloaded => "OVERLOADED",
            WireError::Draining => "DRAINING",
            WireError::DuplicateInProgress => "DUPLICATE_IN_PROGRESS",
        }
    }

    /// The `ErrorInfo.domain` value.
    #[must_use]
    pub const fn domain(self) -> &'static str {
        match self {
            WireError::TaskNotFound
            | WireError::TaskNotCancelable
            | WireError::PushNotificationNotSupported
            | WireError::UnsupportedOperation
            | WireError::InvalidParams
            | WireError::ContentTypeNotSupported
            | WireError::InvalidAgentResponse
            | WireError::ExtendedAgentCardNotConfigured
            | WireError::ExtensionSupportRequired
            | WireError::VersionNotSupported
            | WireError::ExtensionDataInvalid
            | WireError::UnknownOperation => A2A_DOMAIN,
            WireError::Unauthenticated
            | WireError::PermissionDenied
            | WireError::RequestExpired
            | WireError::PayloadTooLarge
            | WireError::MalformedMessage
            | WireError::UnsupportedMediaType
            | WireError::TenantMismatch
            | WireError::Overloaded
            | WireError::Draining
            | WireError::DuplicateInProgress => BUS_DOMAIN,
        }
    }

    /// The fixed, short `Status.message` text.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            WireError::TaskNotFound => "task not found",
            WireError::TaskNotCancelable => "task cannot be canceled",
            WireError::PushNotificationNotSupported => "push notifications are not supported",
            WireError::UnsupportedOperation => "operation is not supported in this state",
            WireError::InvalidParams => "invalid parameters",
            WireError::ContentTypeNotSupported => "content type is not supported",
            WireError::InvalidAgentResponse => "agent response is invalid",
            WireError::ExtendedAgentCardNotConfigured => "extended agent card is not configured",
            WireError::ExtensionSupportRequired => "required extension is not supported",
            WireError::VersionNotSupported => "protocol version is not supported",
            WireError::ExtensionDataInvalid => "extension data is invalid",
            WireError::UnknownOperation => "operation is not known",
            WireError::Unauthenticated => "request is not authenticated",
            WireError::PermissionDenied => "permission denied",
            WireError::RequestExpired => "request has expired",
            WireError::PayloadTooLarge => "payload is too large",
            WireError::MalformedMessage => "message is malformed",
            WireError::UnsupportedMediaType => "media type is not supported",
            WireError::TenantMismatch => "tenant does not match subject",
            WireError::Overloaded => "worker is overloaded",
            WireError::Draining => "worker is draining",
            WireError::DuplicateInProgress => "request is already in progress",
        }
    }

    /// Finds the error for a received `(code, domain, reason)` triple. The code is part of the
    /// key because `UNSUPPORTED_OPERATION` appears with code 9 and code 12.
    #[must_use]
    pub fn from_reason(code: i32, domain: &str, reason: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|error| {
            error.code() == code && error.domain() == domain && error.reason() == reason
        })
    }

    /// Builds the `google.rpc.Status` for this error with no extra details.
    #[must_use]
    pub fn status(self) -> rpc::Status {
        self.status_with(&StatusExtras::default())
    }

    /// Builds the `google.rpc.Status` for this error with extra details.
    ///
    /// The status carries exactly one `ErrorInfo`; `BadRequest` and `RetryInfo` are added only when
    /// `extras` supplies field violations or a retry delay.
    #[must_use]
    pub fn status_with(self, extras: &StatusExtras) -> rpc::Status {
        let info = rpc::ErrorInfo {
            reason: self.reason().to_owned(),
            domain: self.domain().to_owned(),
            metadata: extras
                .metadata
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        };
        let mut details = vec![any(ERROR_INFO_TYPE_URL, info.encode_to_vec())];
        if !extras.field_violations.is_empty() {
            let bad = rpc::BadRequest {
                field_violations: extras
                    .field_violations
                    .iter()
                    .map(|violation| rpc::bad_request::FieldViolation {
                        field: violation.field.clone(),
                        description: violation.description.clone(),
                        reason: String::new(),
                        localized_message: None,
                    })
                    .collect(),
            };
            details.push(any(BAD_REQUEST_TYPE_URL, bad.encode_to_vec()));
        }
        if let Some(delay) = extras.retry_after {
            let retry = rpc::RetryInfo {
                retry_delay: Some(pbjson_types::Duration {
                    seconds: i64::try_from(delay.as_secs()).unwrap_or(i64::MAX),
                    nanos: i32::try_from(delay.subsec_nanos()).unwrap_or(0),
                }),
            };
            details.push(any(RETRY_INFO_TYPE_URL, retry.encode_to_vec()));
        }
        rpc::Status {
            code: self.code(),
            message: self.message().to_owned(),
            details,
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for WireError {}

fn any(type_url: &str, value: Vec<u8>) -> pbjson_types::Any {
    pbjson_types::Any {
        type_url: type_url.to_owned(),
        value: value.into(),
    }
}

/// Optional details attached to an error reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusExtras {
    /// `ErrorInfo.metadata` entries, for example `taskId` or `detail`. Values are short strings.
    pub metadata: BTreeMap<String, String>,
    /// Field violations for a `BadRequest` detail (RFC-0002 [0002-096]).
    pub field_violations: Vec<FieldViolation>,
    /// Retry delay for a `RetryInfo` detail (RFC-0002 [0002-094]).
    pub retry_after: Option<Duration>,
}

/// One entry of a `BadRequest` detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldViolation {
    /// Field path, for example `A2AB-Ttl-Ms`.
    pub field: String,
    /// Short English description of the violation.
    pub description: String,
}

/// Returns the single `ErrorInfo` of a status, or `None` when there is not exactly one.
#[must_use]
pub fn error_info(status: &rpc::Status) -> Option<rpc::ErrorInfo> {
    let mut found = None;
    for detail in &status.details {
        if detail.type_url == ERROR_INFO_TYPE_URL {
            if found.is_some() {
                return None;
            }
            found = Some(rpc::ErrorInfo::decode(detail.value.as_ref()).ok()?);
        }
    }
    found
}

/// Classifies a received status as a known [`WireError`], or `None` for an unknown or
/// malformed status.
#[must_use]
pub fn classify(status: &rpc::Status) -> Option<WireError> {
    let info = error_info(status)?;
    WireError::from_reason(status.code, &info.domain, &info.reason)
}

/// Returns the `RetryInfo` delay of a status, when present.
#[must_use]
pub fn retry_after(status: &rpc::Status) -> Option<Duration> {
    for detail in &status.details {
        if detail.type_url == RETRY_INFO_TYPE_URL {
            let retry = rpc::RetryInfo::decode(detail.value.as_ref()).ok()?;
            let delay = retry.retry_delay?;
            let seconds = u64::try_from(delay.seconds).ok()?;
            let nanos = u32::try_from(delay.nanos).ok()?;
            return Some(Duration::new(seconds, nanos));
        }
    }
    None
}

/// Failures that stay on the requester side and never appear on the wire (RFC-0002 [0002-093]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LocalError {
    /// No Worker serves the subject (`UNAVAILABLE`, code 14).
    Unavailable,
    /// The reply did not arrive before the deadline (`DEADLINE_EXCEEDED`, code 4).
    DeadlineExceeded,
    /// The requested stream position is below the retention window (RFC-0002 [0002-065]).
    RetentionExceeded,
}

impl LocalError {
    /// The gRPC-style code used for local reporting.
    #[must_use]
    pub const fn code(self) -> i32 {
        match self {
            LocalError::Unavailable => 14,
            LocalError::DeadlineExceeded => 4,
            LocalError::RetentionExceeded => 14,
        }
    }

    /// Maps a bus-level failure to its local meaning. Returns `None` for failures that are
    /// reported through their own bus error.
    #[must_use]
    pub fn from_bus(error: &BusError) -> Option<Self> {
        match error {
            BusError::NoResponder => Some(LocalError::Unavailable),
            BusError::Timeout => Some(LocalError::DeadlineExceeded),
            BusError::Unavailable { reason } if *reason == "RETENTION_EXCEEDED" => {
                Some(LocalError::RetentionExceeded)
            }
            _ => None,
        }
    }
}

impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LocalError::Unavailable => "no responder for subject",
            LocalError::DeadlineExceeded => "deadline exceeded",
            LocalError::RetentionExceeded => "retention exceeded",
        })
    }
}

impl std::error::Error for LocalError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_info(status: &rpc::Status) -> rpc::ErrorInfo {
        let info = error_info(status);
        assert!(info.is_some(), "status must carry exactly one ErrorInfo");
        info.unwrap_or_default()
    }

    #[test]
    fn req_0002_091_a2a_table_codes_and_reasons() {
        let rows = [
            (WireError::TaskNotFound, 5, "TASK_NOT_FOUND"),
            (WireError::TaskNotCancelable, 9, "TASK_NOT_CANCELABLE"),
            (
                WireError::PushNotificationNotSupported,
                9,
                "PUSH_NOTIFICATION_NOT_SUPPORTED",
            ),
            (WireError::UnsupportedOperation, 9, "UNSUPPORTED_OPERATION"),
            (WireError::InvalidParams, 3, "INVALID_PARAMS"),
            (
                WireError::ContentTypeNotSupported,
                3,
                "CONTENT_TYPE_NOT_SUPPORTED",
            ),
            (
                WireError::InvalidAgentResponse,
                13,
                "INVALID_AGENT_RESPONSE",
            ),
            (
                WireError::ExtendedAgentCardNotConfigured,
                9,
                "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
            ),
            (
                WireError::ExtensionSupportRequired,
                9,
                "EXTENSION_SUPPORT_REQUIRED",
            ),
            (WireError::VersionNotSupported, 9, "VERSION_NOT_SUPPORTED"),
            (WireError::ExtensionDataInvalid, 3, "EXTENSION_DATA_INVALID"),
        ];
        for (error, code, reason) in rows {
            assert_eq!(error.code(), code, "{reason} code");
            assert_eq!(error.reason(), reason);
            assert_eq!(error.domain(), A2A_DOMAIN);
        }
    }

    #[test]
    fn req_0002_092_bus_table_codes_and_reasons() {
        let rows = [
            (WireError::Unauthenticated, 16, "UNAUTHENTICATED"),
            (WireError::PermissionDenied, 7, "PERMISSION_DENIED"),
            (WireError::RequestExpired, 4, "REQUEST_EXPIRED"),
            (WireError::PayloadTooLarge, 8, "PAYLOAD_TOO_LARGE"),
            (WireError::MalformedMessage, 3, "MALFORMED_MESSAGE"),
            (WireError::UnsupportedMediaType, 3, "UNSUPPORTED_MEDIA_TYPE"),
            (WireError::TenantMismatch, 3, "TENANT_MISMATCH"),
            (WireError::Overloaded, 14, "OVERLOADED"),
            (WireError::Draining, 14, "DRAINING"),
            (WireError::DuplicateInProgress, 10, "DUPLICATE_IN_PROGRESS"),
        ];
        for (error, code, reason) in rows {
            assert_eq!(error.code(), code, "{reason} code");
            assert_eq!(error.reason(), reason);
            assert_eq!(error.domain(), BUS_DOMAIN);
        }
    }

    #[test]
    fn req_0002_023_unknown_operation_is_unimplemented_in_a2a_domain() {
        assert_eq!(WireError::UnknownOperation.code(), 12);
        assert_eq!(
            WireError::UnknownOperation.reason(),
            "UNSUPPORTED_OPERATION"
        );
        assert_eq!(WireError::UnknownOperation.domain(), A2A_DOMAIN);
    }

    #[test]
    fn req_0002_090_status_carries_exactly_one_error_info() {
        for error in WireError::ALL {
            let status = error.status();
            assert_eq!(status.code, error.code());
            assert_eq!(status.message, error.message());
            let infos = status
                .details
                .iter()
                .filter(|detail| detail.type_url == ERROR_INFO_TYPE_URL)
                .count();
            assert_eq!(infos, 1, "{:?}", error);
            let info = status_info(&status);
            assert_eq!(info.reason, error.reason());
            assert_eq!(info.domain, error.domain());
            assert!(info.metadata.is_empty());
        }
    }

    #[test]
    fn req_0002_090_round_trip_through_status_classifies_every_error() {
        for error in WireError::ALL {
            assert_eq!(classify(&error.status()), Some(error), "{:?}", error);
        }
    }

    #[test]
    fn req_0002_091_unsupported_operation_is_told_apart_by_code() {
        // Same domain and reason, different code: the code decides.
        let a2a = WireError::UnsupportedOperation.status();
        let unknown = WireError::UnknownOperation.status();
        assert_eq!(classify(&a2a), Some(WireError::UnsupportedOperation));
        assert_eq!(classify(&unknown), Some(WireError::UnknownOperation));
    }

    #[test]
    fn req_0002_090_unknown_or_wrong_domain_status_does_not_classify() {
        let mut status = WireError::TaskNotFound.status();
        status.code = 3;
        assert_eq!(classify(&status), None, "code must match the table");

        let mut status = WireError::TaskNotFound.status();
        status.details.clear();
        assert_eq!(classify(&status), None, "no ErrorInfo means unclassified");

        let mut status = WireError::TaskNotFound.status();
        let copy = status.details[0].clone();
        status.details.push(copy);
        assert_eq!(
            classify(&status),
            None,
            "two ErrorInfo details are malformed"
        );
        assert!(error_info(&status).is_none());
    }

    #[test]
    fn req_0002_094_overloaded_carries_retry_info() {
        let extras = StatusExtras {
            retry_after: Some(Duration::from_millis(1_500)),
            ..StatusExtras::default()
        };
        let status = WireError::Overloaded.status_with(&extras);
        assert_eq!(retry_after(&status), Some(Duration::from_millis(1_500)));
        assert_eq!(classify(&status), Some(WireError::Overloaded));
    }

    #[test]
    fn req_0002_094_status_without_retry_has_no_retry_detail() {
        assert_eq!(retry_after(&WireError::Overloaded.status()), None);
    }

    #[test]
    fn req_0002_096_validation_error_carries_bad_request_fields() {
        let extras = StatusExtras {
            field_violations: vec![FieldViolation {
                field: "A2AB-Ttl-Ms".to_owned(),
                description: "must be between 1 and 3600000".to_owned(),
            }],
            ..StatusExtras::default()
        };
        let status = WireError::MalformedMessage.status_with(&extras);
        let bad = status
            .details
            .iter()
            .find(|detail| detail.type_url == BAD_REQUEST_TYPE_URL)
            .and_then(|detail| rpc::BadRequest::decode(detail.value.as_ref()).ok());
        assert!(bad.is_some(), "BadRequest detail missing");
        let bad = bad.unwrap_or_default();
        assert_eq!(bad.field_violations.len(), 1);
        assert_eq!(bad.field_violations[0].field, "A2AB-Ttl-Ms");
        assert_eq!(classify(&status), Some(WireError::MalformedMessage));
    }

    #[test]
    fn req_0002_090_metadata_is_carried_in_error_info() {
        let mut extras = StatusExtras::default();
        extras.metadata.insert(
            "taskId".to_owned(),
            "0196f3a2-7c1e-7d4a-9b2e-5f1c8a3d6e21".to_owned(),
        );
        let status = WireError::TaskNotCancelable.status_with(&extras);
        let info = status_info(&status);
        assert_eq!(
            info.metadata.get("taskId").map(String::as_str),
            Some("0196f3a2-7c1e-7d4a-9b2e-5f1c8a3d6e21")
        );
    }

    #[test]
    fn req_0002_095_messages_are_fixed_short_text() {
        for error in WireError::ALL {
            let message = error.message();
            assert!(!message.is_empty());
            assert!(message.len() <= 48, "{message}");
            assert!(message.is_ascii());
            assert!(!message.contains('\n'));
        }
    }

    #[test]
    fn req_0002_093_local_errors_map_from_bus_failures() {
        assert_eq!(
            LocalError::from_bus(&BusError::NoResponder),
            Some(LocalError::Unavailable)
        );
        assert_eq!(
            LocalError::from_bus(&BusError::Timeout),
            Some(LocalError::DeadlineExceeded)
        );
        assert_eq!(
            LocalError::from_bus(&BusError::Unavailable {
                reason: "RETENTION_EXCEEDED"
            }),
            Some(LocalError::RetentionExceeded)
        );
        assert_eq!(LocalError::from_bus(&BusError::PermissionDenied), None);
        assert_eq!(LocalError::Unavailable.code(), 14);
        assert_eq!(LocalError::DeadlineExceeded.code(), 4);
    }
}
