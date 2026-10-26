//! Extension URIs, the `A2A-Extensions` activation header, and URI-keyed metadata access
//! (RFC-0003 sec 4.1 and 4.3).

use std::fmt;

use pbjson_types::{Struct, Value, value::Kind};
use thiserror::Error;

use crate::vocab::BINDING_URI;

/// Reason token for malformed activated extension data (RFC-0003 [0003-013], sec 4.8).
pub const EXTENSION_DATA_INVALID: &str = "EXTENSION_DATA_INVALID";

const EXT_PREFIX: &str = "urn:a2abus:ext:";

/// Errors from extension URI, activation, and data handling.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExtensionError {
    /// The string is not `urn:a2abus:ext:<name>:<major>` ([0003-001]).
    #[error("invalid A2A-Bus extension URI {0:?}")]
    InvalidUri(String),
    /// An activation token contains whitespace or control characters.
    #[error("invalid A2A-Extensions token {0:?}")]
    InvalidToken(String),
    /// A reply tried to echo a URI that was not activated on the request ([0003-011]).
    #[error("{0} is not activated on this request")]
    NotActivated(String),
    /// Data under an activated URI is malformed ([0003-013]).
    #[error("data under activated extension {0} is malformed")]
    DataInvalid(String),
}

impl ExtensionError {
    /// The `ErrorInfo` reason token for this error, when it maps to one (RFC-0003 sec 4.8).
    #[must_use]
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            ExtensionError::DataInvalid(_) => Some(EXTENSION_DATA_INVALID),
            _ => None,
        }
    }
}

/// A validated A2A-Bus extension URI `urn:a2abus:ext:<name>:<major>` (RFC-0003 [0003-001]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExtensionUri {
    uri: String,
    major: u32,
}

impl ExtensionUri {
    /// Parses a registry-form URI. `<name>` is lowercase ASCII letters and `-`. `<major>` is a
    /// positive integer without leading zeros.
    pub fn parse(text: &str) -> Result<Self, ExtensionError> {
        let invalid = || ExtensionError::InvalidUri(text.to_owned());
        let rest = text.strip_prefix(EXT_PREFIX).ok_or_else(invalid)?;
        let (name, major) = rest.split_once(':').ok_or_else(invalid)?;
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            return Err(invalid());
        }
        if major.is_empty()
            || !major.bytes().all(|b| b.is_ascii_digit())
            || (major.len() > 1 && major.starts_with('0'))
        {
            return Err(invalid());
        }
        let major: u32 = major.parse().map_err(|_| invalid())?;
        if major == 0 {
            return Err(invalid());
        }
        Ok(Self {
            uri: text.to_owned(),
            major,
        })
    }

    /// The URI exactly as written. It is also the metadata key ([0003-003]).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.uri
    }

    /// The `<name>` segment.
    #[must_use]
    pub fn name(&self) -> &str {
        self.uri
            .strip_prefix(EXT_PREFIX)
            .and_then(|rest| rest.split(':').next())
            .unwrap_or_default()
    }

    /// The `<major>` segment.
    #[must_use]
    pub fn major(&self) -> u32 {
        self.major
    }
}

impl fmt::Display for ExtensionUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.uri)
    }
}

/// The URIs activated on one request, parsed from `A2A-Extensions` (RFC-0003 [0003-009]).
///
/// The header is a comma-separated list with optional spaces. Empty items are skipped, and so
/// is the binding identifier: it is an unknown non-required extension here ([0003-004]). Other
/// URIs are kept as opaque strings, so an unknown URI is only ignored later ([0003-015]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Activation {
    uris: Vec<String>,
}

impl Activation {
    /// Parses the header value. Repeated header lines are a transport concern and are rejected
    /// before this point.
    pub fn parse(header: &str) -> Result<Self, ExtensionError> {
        let mut uris: Vec<String> = Vec::new();
        for raw in header.split(',') {
            let token = raw.trim_matches(|c| c == ' ' || c == '\t');
            if token.is_empty() {
                continue;
            }
            if token == BINDING_URI {
                // An unknown, non-required extension for this receiver: ignored ([0003-015]).
                continue;
            }
            if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(ExtensionError::InvalidToken(token.to_owned()));
            }
            if !uris.iter().any(|known| known == token) {
                uris.push(token.to_owned());
            }
        }
        Ok(Self { uris })
    }

    /// True when `uri` is activated on this request.
    #[must_use]
    pub fn contains(&self, uri: &str) -> bool {
        self.uris.iter().any(|known| known == uri)
    }

    /// The activated URIs in header order, without duplicates.
    #[must_use]
    pub fn uris(&self) -> &[String] {
        &self.uris
    }

    /// Builds the reply `A2A-Extensions` value listing the URIs the receiver applied
    /// ([0003-011]). Every applied URI must be activated, and the reply lists nothing else.
    /// The result is empty when nothing was applied; the caller then omits the header.
    pub fn echo<'a>(
        &self,
        applied: impl IntoIterator<Item = &'a str>,
    ) -> Result<String, ExtensionError> {
        let mut echoed: Vec<&str> = Vec::new();
        for uri in applied {
            if !self.contains(uri) {
                return Err(ExtensionError::NotActivated(uri.to_owned()));
            }
            if !echoed.contains(&uri) {
                echoed.push(uri);
            }
        }
        Ok(echoed.join(", "))
    }
}

/// Reads the data under `uri` in `metadata`, gated by activation ([0003-010], [0003-013]).
///
/// Returns `Ok(None)` when the URI is not activated or no data is present. Data under a
/// non-activated URI is ignored and never fails ([0003-010]). An activated URI whose value is
/// not a `Struct` returns [`ExtensionError::DataInvalid`]. Schema checks belong to the owner
/// RFC and must run before any data is applied ([0003-013]).
pub fn read_activated<'a>(
    metadata: &'a Struct,
    uri: &str,
    activation: &Activation,
) -> Result<Option<&'a Struct>, ExtensionError> {
    if !activation.contains(uri) {
        return Ok(None);
    }
    match metadata.fields.get(uri) {
        None => Ok(None),
        Some(Value {
            kind: Some(Kind::StructValue(data)),
        }) => Ok(Some(data)),
        Some(_) => Err(ExtensionError::DataInvalid(uri.to_owned())),
    }
}

/// Writes `data` under the URI key of `uri` ([0003-003]), replacing any earlier value.
pub fn write_data(metadata: &mut Struct, uri: &ExtensionUri, data: Struct) {
    metadata.fields.insert(
        uri.as_str().to_owned(),
        Value {
            kind: Some(Kind::StructValue(data)),
        },
    );
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::vocab::ext;
    use serde_json::json;

    fn struct_of(value: serde_json::Value) -> Struct {
        serde_json::from_value(value).unwrap()
    }

    // [0003-001] every M1 registry URI parses, with the name and major from its form.
    #[test]
    fn req_0003_001_registry_uris_parse_with_name_and_major() {
        for text in ext::REGISTRY {
            let uri = ExtensionUri::parse(text).unwrap();
            assert_eq!(uri.as_str(), text);
            assert_eq!(uri.major(), 1);
            assert!(text.contains(uri.name()), "{text}");
        }
        let approval = ExtensionUri::parse(ext::APPROVAL).unwrap();
        assert_eq!(approval.name(), "approval");
    }

    // [0003-001] names are lowercase letters and '-'; the major is a positive integer.
    #[test]
    fn req_0003_001_malformed_uris_are_rejected() {
        let bad = [
            "",
            "urn:a2abus:ext:approval",
            "urn:a2abus:ext:approval:",
            "urn:a2abus:ext:Approval:1",
            "urn:a2abus:ext:app_roval:1",
            "urn:a2abus:ext::1",
            "urn:a2abus:ext:approval:0",
            "urn:a2abus:ext:approval:01",
            "urn:a2abus:ext:approval:-1",
            "urn:a2abus:ext:approval:1.0",
            "urn:a2abus:ext:approval:99999999999999999999",
            "urn:other:ext:approval:1",
            "urn:a2abus:ext:approval:1:extra",
        ];
        for text in bad {
            assert!(
                matches!(
                    ExtensionUri::parse(text),
                    Err(ExtensionError::InvalidUri(_))
                ),
                "{text:?} must be rejected"
            );
        }
    }

    // [0003-001] a higher major is a distinct, valid URI.
    #[test]
    fn req_0003_001_higher_major_is_a_distinct_uri() {
        let v2 = ExtensionUri::parse("urn:a2abus:ext:budget:2").unwrap();
        assert_eq!(v2.major(), 2);
        assert_ne!(v2, ExtensionUri::parse(ext::BUDGET).unwrap());
    }

    // [0003-009] the header is comma-separated with optional spaces; empty items are skipped.
    #[test]
    fn req_0003_009_header_accepts_commas_and_optional_spaces() {
        let header =
            " urn:a2abus:ext:approval:1 ,urn:a2abus:ext:budget:1,, urn:a2abus:ext:budget:1";
        let activation = Activation::parse(header).unwrap();
        assert_eq!(
            activation.uris(),
            ["urn:a2abus:ext:approval:1", "urn:a2abus:ext:budget:1"]
        );
        assert!(activation.contains(ext::BUDGET));
        assert!(!activation.contains(ext::PROVENANCE));
    }

    // [0003-009] a token holding whitespace or control characters is malformed.
    #[test]
    fn req_0003_009_header_rejects_internal_whitespace_and_controls() {
        assert!(matches!(
            Activation::parse("urn:a2abus:ext:approval 1"),
            Err(ExtensionError::InvalidToken(_))
        ));
        assert!(matches!(
            Activation::parse("urn:a2abus:ext:approval:1\u{7}"),
            Err(ExtensionError::InvalidToken(_))
        ));
    }

    // [0003-004] a binding identifier in A2A-Extensions is an unknown non-required extension:
    // it is ignored, not an error, and never activated.
    #[test]
    fn req_0003_004_binding_uri_in_header_is_ignored() {
        let header = format!("{ext_budget}, {BINDING_URI}", ext_budget = ext::BUDGET);
        let activation = Activation::parse(&header).unwrap();
        assert!(activation.contains(ext::BUDGET));
        assert!(!activation.contains(BINDING_URI));
        assert_eq!(activation.uris(), [ext::BUDGET.to_owned()]);
        // The binding URI is also not a valid extension URI.
        assert!(matches!(
            ExtensionUri::parse(BINDING_URI),
            Err(ExtensionError::InvalidUri(_))
        ));
    }

    // [0003-014] activation comes only from the header; an empty header activates nothing.
    #[test]
    fn req_0003_014_empty_header_activates_nothing() {
        let activation = Activation::parse("").unwrap();
        assert!(activation.uris().is_empty());
        assert!(!activation.contains(ext::APPROVAL));
    }

    // [0003-010] data under a non-activated URI is ignored and never fails.
    #[test]
    fn req_0003_010_non_activated_data_is_ignored() {
        let metadata = struct_of(json!({ext::BUDGET: "not even an object"}));
        let activation = Activation::parse(ext::APPROVAL).unwrap();
        assert_eq!(
            read_activated(&metadata, ext::BUDGET, &activation),
            Ok(None)
        );
    }

    // [0003-010] activated data that is absent reads as None.
    #[test]
    fn req_0003_010_activated_but_absent_data_reads_as_none() {
        let metadata = Struct::default();
        let activation = Activation::parse(ext::BUDGET).unwrap();
        assert_eq!(
            read_activated(&metadata, ext::BUDGET, &activation),
            Ok(None)
        );
    }

    // [0003-013] activated data that is not a Struct is malformed: EXTENSION_DATA_INVALID.
    #[test]
    fn req_0003_013_activated_non_struct_data_is_invalid() {
        let metadata = struct_of(json!({ext::BUDGET: "oops"}));
        let activation = Activation::parse(ext::BUDGET).unwrap();
        let error = read_activated(&metadata, ext::BUDGET, &activation).unwrap_err();
        assert_eq!(error, ExtensionError::DataInvalid(ext::BUDGET.to_owned()));
        assert_eq!(error.reason(), Some("EXTENSION_DATA_INVALID"));
    }

    // [0003-003] the metadata key is the extension URI exactly.
    #[test]
    fn req_0003_003_written_data_is_keyed_by_the_uri() {
        let uri = ExtensionUri::parse(ext::SEQUENCE).unwrap();
        let mut metadata = Struct::default();
        write_data(&mut metadata, &uri, struct_of(json!({"seq": "3"})));
        assert_eq!(metadata.fields.len(), 1);
        assert!(metadata.fields.contains_key(ext::SEQUENCE));
        let activation = Activation::parse(ext::SEQUENCE).unwrap();
        let read = read_activated(&metadata, ext::SEQUENCE, &activation)
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::to_value(read).unwrap(), json!({"seq": "3"}));
    }

    // [0003-011] the reply echoes exactly the applied URIs, each once, in the given order.
    #[test]
    fn req_0003_011_echo_lists_applied_uris_only() {
        let activation = Activation::parse(&format!("{}, {}", ext::APPROVAL, ext::BUDGET)).unwrap();
        let echoed = activation
            .echo([ext::BUDGET, ext::APPROVAL, ext::BUDGET])
            .unwrap();
        assert_eq!(echoed, format!("{}, {}", ext::BUDGET, ext::APPROVAL));
        assert_eq!(activation.echo(std::iter::empty()).unwrap(), "");
    }

    // [0003-011] the reply must not list a URI the request did not activate.
    #[test]
    fn req_0003_011_echo_refuses_a_non_activated_uri() {
        let activation = Activation::parse(ext::APPROVAL).unwrap();
        assert_eq!(
            activation.echo([ext::BUDGET]),
            Err(ExtensionError::NotActivated(ext::BUDGET.to_owned()))
        );
    }

    // [0003-015] an unknown URI in the header is kept as an opaque, ignorable token.
    #[test]
    fn req_0003_015_unknown_uri_is_opaque_and_ignored() {
        let activation =
            Activation::parse("urn:a2abus:ext:example:9, urn:a2abus:ext:approval:1").unwrap();
        assert!(activation.contains("urn:a2abus:ext:example:9"));
        // Only the activated, understood URI is echoed; the unknown one is not applied.
        assert_eq!(
            activation.echo([ext::APPROVAL]).unwrap(),
            ext::APPROVAL.to_owned()
        );
    }
}
