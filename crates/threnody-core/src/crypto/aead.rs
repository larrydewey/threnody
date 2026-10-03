//! AEAD suites (spec §3.2: ChaCha20-Poly1305 and AES-256-GCM, both MUST be
//! supported).

use aes_gcm::Aes256Gcm;
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};

use crate::error::{Error, Result};

/// Negotiated symmetric suite. Wire values are stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Suite {
    ChaCha20Poly1305 = 1,
    Aes256Gcm = 2,
}

impl Suite {
    /// Suites this implementation offers, in preference order.
    pub const SUPPORTED: [Suite; 2] = [Suite::ChaCha20Poly1305, Suite::Aes256Gcm];

    pub fn from_wire(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::ChaCha20Poly1305),
            2 => Some(Self::Aes256Gcm),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ChaCha20Poly1305 => "ChaCha20-Poly1305",
            Self::Aes256Gcm => "AES-256-GCM",
        }
    }

    pub fn seal(self, key: &[u8; 32], nonce: &[u8; 12], ad: &[u8], pt: &[u8]) -> Vec<u8> {
        let payload = Payload { msg: pt, aad: ad };
        let out = match self {
            Self::ChaCha20Poly1305 => {
                ChaCha20Poly1305::new(key.into()).encrypt(nonce.into(), payload)
            }
            Self::Aes256Gcm => Aes256Gcm::new(key.into()).encrypt(nonce.into(), payload),
        };
        // Encryption only fails for inputs beyond the AEAD's length limit
        // (2^38 bytes), far above anything the protocol frames carry.
        out.expect("plaintext within AEAD length limit")
    }

    pub fn open(self, key: &[u8; 32], nonce: &[u8; 12], ad: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
        let payload = Payload { msg: ct, aad: ad };
        match self {
            Self::ChaCha20Poly1305 => {
                ChaCha20Poly1305::new(key.into()).decrypt(nonce.into(), payload)
            }
            Self::Aes256Gcm => Aes256Gcm::new(key.into()).decrypt(nonce.into(), payload),
        }
        .map_err(|_| Error::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_both_suites() {
        for s in Suite::SUPPORTED {
            let key = [7u8; 32];
            let nonce = [1u8; 12];
            let ct = s.seal(&key, &nonce, b"ad", b"hello");
            assert_eq!(s.open(&key, &nonce, b"ad", &ct).unwrap(), b"hello");
            assert!(s.open(&key, &nonce, b"other", &ct).is_err());
        }
    }
}
