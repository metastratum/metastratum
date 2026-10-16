//! Secrets at rest: values sealed with a master key the database never sees.
//!
//! A sealed value is `enc:v1:` followed by base64 of a random 24-byte nonce
//! and the XChaCha20-Poly1305 ciphertext. Each value is bound to a context
//! (what it is and which row it belongs to) as associated data, so a sealed
//! value copied into another row does not open.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const PREFIX: &str = "enc:v1:";

#[derive(Debug, thiserror::Error)]
pub enum SealError {
    #[error("the master key must be 32 bytes, base64-encoded")]
    BadKey,
    #[error("a sealed value is damaged, or was sealed with another master key")]
    Open,
}

/// Seals and opens secrets with the master key.
#[derive(Clone)]
pub struct Sealer {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(..)")
    }
}

/// Whether `value` is sealed (as opposed to plaintext from before sealing).
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(PREFIX)
}

impl Sealer {
    /// A new random master key, base64-encoded.
    pub fn generate_key() -> String {
        STANDARD.encode(rand::random::<[u8; 32]>())
    }

    /// From a base64-encoded 32-byte key (surrounding whitespace ignored).
    pub fn from_base64(key: &str) -> Result<Self, SealError> {
        let bytes = STANDARD.decode(key.trim()).map_err(|_| SealError::BadKey)?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| SealError::BadKey)?;
        Ok(Self {
            cipher: XChaCha20Poly1305::new(&key.into()),
        })
    }

    pub fn seal(&self, context: &str, plaintext: &str) -> String {
        let nonce: [u8; 24] = rand::random();
        let ct = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: context.as_bytes(),
                },
            )
            .expect("encrypting in memory cannot fail");
        let mut out = nonce.to_vec();
        out.extend(ct);
        format!("{PREFIX}{}", STANDARD.encode(out))
    }

    /// Opens a sealed value. Plaintext (stored before sealing existed) is
    /// returned as is.
    pub fn open(&self, context: &str, value: &str) -> Result<String, SealError> {
        let Some(b64) = value.strip_prefix(PREFIX) else {
            return Ok(value.to_owned());
        };
        let raw = STANDARD.decode(b64).map_err(|_| SealError::Open)?;
        if raw.len() < 24 {
            return Err(SealError::Open);
        }
        let (nonce, ct) = raw.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().map_err(|_| SealError::Open)?;
        let pt = self
            .cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ct,
                    aad: context.as_bytes(),
                },
            )
            .map_err(|_| SealError::Open)?;
        String::from_utf8(pt).map_err(|_| SealError::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_open_only_with_their_key_and_context() {
        let s = Sealer::from_base64(&Sealer::generate_key()).unwrap();
        let sealed = s.seal("pool:1:key", "AQBsecret==");
        assert!(is_sealed(&sealed));
        assert!(!sealed.contains("AQBsecret"));
        assert_eq!(s.open("pool:1:key", &sealed).unwrap(), "AQBsecret==");
        // Same plaintext, different ciphertext.
        assert_ne!(s.seal("pool:1:key", "AQBsecret=="), sealed);
        assert!(
            s.open("pool:2:key", &sealed).is_err(),
            "moved to another row"
        );
        let other = Sealer::from_base64(&Sealer::generate_key()).unwrap();
        assert!(
            other.open("pool:1:key", &sealed).is_err(),
            "another master key"
        );
        // Plaintext from before sealing passes through.
        assert_eq!(s.open("x", "plain").unwrap(), "plain");
    }

    #[test]
    fn keys_must_be_32_bytes() {
        assert!(Sealer::from_base64("c2hvcnQ=").is_err());
        assert!(Sealer::from_base64("not base64!").is_err());
    }
}
