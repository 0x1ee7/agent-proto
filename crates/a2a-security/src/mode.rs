//! Operator policy inputs that the verifiers consult ([0007-031], [0007-095], [0007-111]).

/// Whether signing is enforced for a tenant ([0007-095]). M1 default is `Disabled`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SigningMode {
    #[default]
    Disabled,
    Enabled,
}

/// Per-tenant policy consulted at verification time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    pub signing: SigningMode,
    /// Destination is on another host ([0007-031]); signatures are then mandatory.
    pub cross_host: bool,
}

impl Policy {
    /// True when an unsigned peer message must be refused ([0007-031]).
    #[must_use]
    pub fn requires_signature(self) -> bool {
        self.signing == SigningMode::Enabled || self.cross_host
    }
}

/// How an MCP tool call reaches its server ([0007-111]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum McpMode {
    /// Calls go through the supervisor proxy. Default.
    #[default]
    Proxy,
    /// Calls go straight to the MCP server. Requires `mcp_direct` on the recipient.
    Direct,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn req_0007_031_cross_host_forces_signatures_even_when_signing_disabled() {
        let local = Policy::default();
        assert!(!local.requires_signature());
        let remote = Policy {
            signing: SigningMode::Disabled,
            cross_host: true,
        };
        assert!(remote.requires_signature());
        let enabled = Policy {
            signing: SigningMode::Enabled,
            cross_host: false,
        };
        assert!(enabled.requires_signature());
    }

    #[test]
    fn req_0007_111_mcp_defaults_to_proxy() {
        assert_eq!(McpMode::default(), McpMode::Proxy);
    }
}
