//! ML-DSA-65 (FIPS 204) signatures, used beside Ed25519 wherever a
//! signature must stay unforgeable against a quantum adversary: issuer keys,
//! credential receipts and relay directories (Appendices O and P).
//!
//! Keys are kept as their 32-byte seed; signing is deterministic.

use ml_dsa::{
    EncodedSignature, EncodedVerifyingKey, ExpandedSigningKey, MlDsa65, Signature, VerifyingKey,
};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

/// Encoded ML-DSA-65 verifying key.
pub const PUBLIC_LEN: usize = 1952;
/// Encoded ML-DSA-65 signature.
pub const SIGNATURE_LEN: usize = 3309;

pub struct PqSigner {
    key: ExpandedSigningKey<MlDsa65>,
}

impl PqSigner {
    pub fn from_seed(seed: &Zeroizing<[u8; 32]>) -> Self {
        Self {
            key: ExpandedSigningKey::<MlDsa65>::from_seed(&(**seed).into()),
        }
    }

    pub fn public(&self) -> Vec<u8> {
        self.key.verifying_key().encode().to_vec()
    }

    /// Signs `msg` under the context string `ctx` (FIPS 204 domain separation).
    pub fn sign(&self, msg: &[u8], ctx: &[u8]) -> Result<Vec<u8>> {
        self.key
            .sign_deterministic(msg, ctx)
            .map(|s| s.encode().to_vec())
            .map_err(|_| Error::Malformed("ml-dsa context too long"))
    }
}

/// Verifies an ML-DSA-65 signature by the encoded key `public`.
pub fn verify(public: &[u8], msg: &[u8], ctx: &[u8], sig: &[u8]) -> Result<()> {
    let pk = EncodedVerifyingKey::<MlDsa65>::try_from(public).map_err(|_| Error::InvalidKey)?;
    let pk = VerifyingKey::<MlDsa65>::decode(&pk);
    let sig = EncodedSignature::<MlDsa65>::try_from(sig).map_err(|_| Error::BadSignature)?;
    let sig = Signature::<MlDsa65>::decode(&sig).ok_or(Error::BadSignature)?;
    if pk.verify_with_context(msg, ctx, &sig) {
        Ok(())
    } else {
        Err(Error::BadSignature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_and_verifies_with_context() {
        let s = PqSigner::from_seed(&Zeroizing::new([7u8; 32]));
        let pk = s.public();
        assert_eq!(pk.len(), PUBLIC_LEN);
        let sig = s.sign(b"msg", b"ctx").unwrap();
        assert_eq!(sig.len(), SIGNATURE_LEN);
        verify(&pk, b"msg", b"ctx", &sig).unwrap();
        assert!(verify(&pk, b"msg", b"other", &sig).is_err());
        assert!(verify(&pk, b"msh", b"ctx", &sig).is_err());
        assert!(verify(&pk[1..], b"msg", b"ctx", &sig).is_err());
        let mut bad = sig.clone();
        bad[100] ^= 1;
        assert!(verify(&pk, b"msg", b"ctx", &bad).is_err());
    }
}
