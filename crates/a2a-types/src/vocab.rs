//! Vocabulary constants fixed by RFC-0001 sec 4.4, RFC-0003 sec 4.1 and RFC-0005 sec 4.9.

/// Extension URIs of the M1 registry (RFC-0003 [0003-002]). Each has the form
/// `urn:a2abus:ext:<name>:<major>` (RFC-0003 [0003-001]).
pub mod ext {
    /// Approval extension (RFC-0006).
    pub const APPROVAL: &str = "urn:a2abus:ext:approval:1";
    /// Budget extension (RFC-0006).
    pub const BUDGET: &str = "urn:a2abus:ext:budget:1";
    /// Delegation extension (RFC-0007).
    pub const DELEGATION: &str = "urn:a2abus:ext:delegation:1";
    /// Provenance extension (RFC-0007).
    pub const PROVENANCE: &str = "urn:a2abus:ext:provenance:1";
    /// Sequence extension (RFC-0005).
    pub const SEQUENCE: &str = "urn:a2abus:ext:sequence:1";
    /// Progress extension (RFC-0009).
    pub const PROGRESS: &str = "urn:a2abus:ext:progress:1";

    /// Every M1 extension URI, in registry order.
    pub const REGISTRY: [&str; 6] = [APPROVAL, BUDGET, DELEGATION, PROVENANCE, SEQUENCE, PROGRESS];
}

/// Protocol binding identifier, carried only in `AgentInterface.protocolBinding`
/// (RFC-0003 [0003-004]). It is not an extension and never appears in `A2A-Extensions`.
pub const BINDING_URI: &str = "urn:a2abus:binding:bus:1";

/// Status metadata key that carries the failure reason (RFC-0005 sec 4.9). It is a plain
/// metadata key, not an extension (RFC-0003 [0003-004]).
pub const FAILURE_KEY: &str = "a2abus.failure";

/// A2A protocol version carried in the `A2A-Version` service parameter (RFC-0002 [0002-080]).
pub const A2A_VERSION: &str = "1.0";

/// Protocol version token used in bus subjects (`a2a.v1.<tenant>...`, RFC-0002 [0002-001]).
pub const SUBJECT_ROOT_VERSION: &str = "v1";
