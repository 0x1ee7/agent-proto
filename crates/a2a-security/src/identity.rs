//! Kids, public-key encoding, Ed25519 signing and verification seams (RFC-0007 §4.3).

use std::fmt;

use a2a_types::ids::AgentId;
use ed25519_dalek::Signer as _;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};

use crate::canon::{b64_decode_canonical, b64_encode, sha256};
use crate::error::SecurityError;

/// Length of the fingerprint `K` in a kid: 8 bytes of SHA-256 in base64url = 11 characters.
const FINGERPRINT_LEN: usize = 11;
const FINGERPRINT_BYTES: usize = 8;
/// Raw Ed25519 public key in base64url without padding (32 bytes = 43 characters).
const PUBLIC_KEY_TEXT_LEN: usize = 43;

/// Key identifier `<agent-id>:<K>` ([0007-002]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kid {
    agent: AgentId,
    fingerprint: String,
}

impl Kid {
    /// Derives the kid of `public_key` for `agent`.
    #[must_use]
    pub fn derive(agent: &AgentId, public_key: &[u8; 32]) -> Self {
        let digest = sha256(public_key);
        Self {
            agent: agent.clone(),
            fingerprint: b64_encode(&digest[..FINGERPRINT_BYTES]),
        }
    }

    /// Parses `<agent-id>:<K>`. Does not check `K` against any key.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (agent, fingerprint) = text.rsplit_once(':')?;
        let agent = AgentId::parse(agent).ok()?;
        if fingerprint.len() != FINGERPRINT_LEN {
            return None;
        }
        let raw = b64_decode_canonical(fingerprint)?;
        if raw.len() != FINGERPRINT_BYTES {
            return None;
        }
        Some(Self {
            agent,
            fingerprint: fingerprint.to_owned(),
        })
    }

    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Display for Kid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.agent.as_str(), self.fingerprint)
    }
}

/// Base64url (no padding) text of a raw Ed25519 public key.
#[must_use]
pub fn encode_public_key(key: &[u8; 32]) -> String {
    b64_encode(key)
}

/// Decodes the 43-character canonical public-key text ([0007-003]).
#[must_use]
pub fn decode_public_key(text: &str) -> Option<[u8; 32]> {
    if text.len() != PUBLIC_KEY_TEXT_LEN {
        return None;
    }
    let raw = b64_decode_canonical(text)?;
    <[u8; 32]>::try_from(raw.as_slice()).ok()
}

/// Produces Ed25519 signatures for one kid. Implemented by [`Ed25519Signer`].
pub trait Signer: Send + Sync {
    fn kid(&self) -> &Kid;
    fn sign_bytes(&self, message: &[u8]) -> [u8; 64];
}

/// Checks Ed25519 signatures against one trusted key. Callers map `false` to their own reason.
pub trait Verifier {
    fn verify_bytes(&self, message: &[u8], signature: &[u8]) -> bool;
}

/// An Ed25519 signing key bound to its kid.
pub struct Ed25519Signer {
    kid: Kid,
    key: SigningKey,
}

impl Ed25519Signer {
    #[must_use]
    pub fn from_seed(agent: &AgentId, seed: &[u8; 32]) -> Self {
        let key = SigningKey::from_bytes(seed);
        let kid = Kid::derive(agent, &key.verifying_key().to_bytes());
        Self { kid, key }
    }

    /// Generates a fresh key from the operating system's CSPRNG.
    pub fn generate(agent: &AgentId) -> Result<Self, SecurityError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| SecurityError::Random)?;
        let signer = Self::from_seed(agent, &seed);
        seed.fill(0);
        Ok(signer)
    }

    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub(crate) const fn signing_key(&self) -> &SigningKey {
        &self.key
    }
}

impl fmt::Debug for Ed25519Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ed25519Signer")
            .field("kid", &self.kid.to_string())
            .finish_non_exhaustive()
    }
}

impl Signer for Ed25519Signer {
    fn kid(&self) -> &Kid {
        &self.kid
    }

    fn sign_bytes(&self, message: &[u8]) -> [u8; 64] {
        self.key.sign(message).to_bytes()
    }
}

/// `verify_strict` over raw bytes. Malformed lengths verify as `false`.
#[must_use]
pub(crate) fn verify_ed25519(key: &VerifyingKey, message: &[u8], signature: &[u8]) -> bool {
    match <[u8; 64]>::try_from(signature) {
        Ok(bytes) => key
            .verify_strict(message, &Signature::from_bytes(&bytes))
            .is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn agent(name: &str) -> AgentId {
        AgentId::parse(name).expect("valid agent id")
    }

    #[test]
    fn req_0007_002_kid_is_agent_colon_eleven_char_fingerprint() {
        let pk = [7u8; 32];
        let kid = Kid::derive(&agent("alpha"), &pk);
        let text = kid.to_string();
        assert!(text.starts_with("alpha:"));
        assert_eq!(kid.fingerprint().len(), FINGERPRINT_LEN);
        assert_eq!(Kid::parse(&text), Some(kid));
    }

    #[test]
    fn req_0007_002_kid_parser_rejects_malformed_fingerprints() {
        assert!(Kid::parse("alpha:AAAAAAAAAAA").is_some());
        assert_eq!(Kid::parse("alpha:AAAAAAAAAA"), None, "too short");
        assert_eq!(Kid::parse("alpha:AAAAAAAAAAAA"), None, "too long");
        assert_eq!(Kid::parse("alpha:AAAAAAAAAA+"), None, "standard alphabet");
        assert_eq!(Kid::parse("alpha"), None, "missing fingerprint");
        assert_eq!(Kid::parse("Alpha:AAAAAAAAAAA"), None, "invalid agent id");
    }

    #[test]
    fn req_0007_003_public_key_text_is_43_canonical_chars() {
        let key = [0xabu8; 32];
        let text = encode_public_key(&key);
        assert_eq!(text.len(), PUBLIC_KEY_TEXT_LEN);
        assert_eq!(decode_public_key(&text), Some(key));
        assert_eq!(decode_public_key(&format!("{text}A")), None);
        assert_eq!(decode_public_key(&text[..42]), None);
    }

    #[test]
    fn req_0007_001_signature_round_trips_and_wrong_key_fails() {
        let signer = Ed25519Signer::from_seed(&agent("alpha"), &[3u8; 32]);
        let other = Ed25519Signer::from_seed(&agent("alpha"), &[4u8; 32]);
        let sig = signer.sign_bytes(b"message");
        let key = VerifyingKey::from_bytes(&signer.public_key()).expect("valid point");
        assert!(verify_ed25519(&key, b"message", &sig));
        assert!(!verify_ed25519(&key, b"other", &sig));
        let wrong = VerifyingKey::from_bytes(&other.public_key()).expect("valid point");
        assert!(!verify_ed25519(&wrong, b"message", &sig));
        assert!(!verify_ed25519(&key, b"message", &sig[..63]));
    }

    #[test]
    fn req_0007_001_fixed_seed_derives_stable_kid() {
        let a = Ed25519Signer::from_seed(&agent("alpha"), &[9u8; 32]);
        let b = Ed25519Signer::from_seed(&agent("alpha"), &[9u8; 32]);
        assert_eq!(a.kid(), b.kid());
        assert_eq!(a.sign_bytes(b"x"), b.sign_bytes(b"x"));
    }

    #[test]
    fn req_0007_001_debug_output_hides_key_material() {
        let signer = Ed25519Signer::from_seed(&agent("alpha"), &[9u8; 32]);
        let rendered = format!("{signer:?}");
        assert!(rendered.contains("alpha:"));
        assert!(!rendered.contains(&encode_public_key(&signer.public_key())));
    }
}
