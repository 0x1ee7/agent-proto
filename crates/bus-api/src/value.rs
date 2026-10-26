//! Ordinals and content digests shared by the bus operations.

use std::fmt;

use thiserror::Error;

/// Position of a message in a durable stream. Starts at 1 in each stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamSeq(u64);

impl StreamSeq {
    /// Wraps a raw sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for StreamSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Revision of a KV entry. Revisions are strictly increasing within a bucket
/// and are never reused, so a tombstone revision fences stale writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revision(u64);

impl Revision {
    /// Wraps a raw revision number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw revision number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// SHA-256 digest of blob content (RFC-0002 [0002-111]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest([u8; 32]);

/// A digest string that is not 64 lowercase hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("digest must be 64 lowercase hexadecimal characters")]
pub struct InvalidDigest;

impl Digest {
    /// Wraps raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses 64 lowercase hexadecimal characters.
    ///
    /// # Errors
    /// Returns [`InvalidDigest`] for any other input.
    pub fn parse(hex: &str) -> Result<Self, InvalidDigest> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidDigest);
        }
        let mut bytes = [0u8; 32];
        for (index, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
            let high = nibble(pair[0])?;
            let low = nibble(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
}

const fn nibble(byte: u8) -> Result<u8, InvalidDigest> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(InvalidDigest),
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test setup failures abort the test")]
mod tests {
    use super::*;

    #[test]
    fn digest_hex_round_trip() {
        let text = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        match Digest::parse(text) {
            Ok(digest) => assert_eq!(digest.to_string(), text),
            Err(error) => panic!("valid digest rejected: {error}"),
        }
    }

    #[test]
    fn digest_rejects_uppercase_short_and_non_hex() {
        assert!(Digest::parse(&"E".repeat(64)).is_err());
        assert!(Digest::parse(&"a".repeat(63)).is_err());
        assert!(Digest::parse(&"g".repeat(64)).is_err());
    }
}
