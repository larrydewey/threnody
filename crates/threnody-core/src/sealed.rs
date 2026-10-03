//! Passphrase sealing for secrets at rest.
//!
//! ```text
//! Sealed = { 0: 1 (Argon2id), 1: m_cost KiB, 2: t_cost, 3: lanes, 4: salt (16), 5: ciphertext }
//! key    = Argon2id(passphrase, salt, m, t, p) -> 32 bytes
//! ct     = ChaCha20-Poly1305(key, nonce = 0, ad = purpose, plaintext)
//! ```
//!
//! Every seal draws a fresh salt and so a fresh key, which makes the fixed
//! nonce safe. The purpose string binds a sealed blob to its role.

use argon2::{Algorithm, Argon2, Params, Version};
use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};

const KDF_ARGON2ID: u64 = 1;
const NONCE: [u8; 12] = [0; 12];

/// Argon2id cost parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub lanes: u32,
}

impl KdfParams {
    /// RFC 9106 §4 second recommended option: 64 MiB, 3 passes, 4 lanes.
    pub const DEFAULT: Self = Self {
        m_kib: 64 * 1024,
        t: 3,
        lanes: 4,
    };

    /// Upper bounds accepted when opening, so a tampered file cannot make
    /// us allocate unbounded memory or spin for hours.
    const MAX: Self = Self {
        m_kib: 1024 * 1024,
        t: 32,
        lanes: 16,
    };

    fn derive(&self, passphrase: &[u8], salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
        if self.m_kib > Self::MAX.m_kib || self.t > Self::MAX.t || self.lanes > Self::MAX.lanes {
            return Err(Error::Malformed("passphrase KDF cost out of range"));
        }
        let params = Params::new(self.m_kib, self.t, self.lanes, Some(32))
            .map_err(|_| Error::Malformed("passphrase KDF parameters"))?;
        let mut key = Zeroizing::new([0u8; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase, salt, &mut key[..])
            .map_err(|_| Error::Malformed("passphrase KDF failed"))?;
        Ok(key)
    }
}

pub fn seal(passphrase: &[u8], purpose: &str, plaintext: &[u8], p: KdfParams) -> Result<Vec<u8>> {
    let salt: [u8; 16] = random_bytes();
    let key = p.derive(passphrase, &salt)?;
    let ct = Suite::ChaCha20Poly1305.seal(&key, &NONCE, purpose.as_bytes(), plaintext);
    cbor::to_vec(ct.len() + 64, |e| {
        e.map_len(6)?;
        e.u8(0)?.uint(KDF_ARGON2ID)?;
        e.u8(1)?.u32(p.m_kib)?;
        e.u8(2)?.u32(p.t)?;
        e.u8(3)?.u32(p.lanes)?;
        e.u8(4)?.bytes(&salt)?;
        e.u8(5)?.bytes(&ct)?;
        Ok(())
    })
}

/// Fails with [`Error::Decrypt`] on a wrong passphrase or tampering.
pub fn open(passphrase: &[u8], purpose: &str, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let mut dec = Decoder::new(sealed);
    let (mut kdf, mut m, mut t, mut p, mut salt, mut ct) = (None, None, None, None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => kdf = Some(d.u64()?),
            1 => m = Some(d.u32()?),
            2 => t = Some(d.u32()?),
            3 => p = Some(d.u32()?),
            4 => salt = Some(fixed_bytes::<16>(d)?),
            5 => ct = Some(d.bytes()?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    if required(kdf, "kdf id")? != KDF_ARGON2ID {
        return Err(Error::Malformed("unknown passphrase KDF"));
    }
    let params = KdfParams {
        m_kib: required(m, "m_cost")?,
        t: required(t, "t_cost")?,
        lanes: required(p, "lanes")?,
    };
    let key = params.derive(passphrase, &required(salt, "salt")?)?;
    let pt = Suite::ChaCha20Poly1305.open(&key, &NONCE, purpose.as_bytes(), required(ct, "ct")?)?;
    Ok(Zeroizing::new(pt))
}

#[cfg(test)]
pub(crate) const TEST_PARAMS: KdfParams = KdfParams {
    m_kib: 64,
    t: 1,
    lanes: 1,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_and_reject() {
        let s = seal(b"pw", "purpose", b"secret", TEST_PARAMS).unwrap();
        assert_eq!(&open(b"pw", "purpose", &s).unwrap()[..], b"secret");
        assert!(matches!(open(b"nope", "purpose", &s), Err(Error::Decrypt)));
        assert!(matches!(open(b"pw", "other", &s), Err(Error::Decrypt)));
        assert_ne!(s, seal(b"pw", "purpose", b"secret", TEST_PARAMS).unwrap());
    }

    #[test]
    fn default_params_work() {
        let s = seal(b"pw", "p", b"x", KdfParams::DEFAULT).unwrap();
        assert_eq!(&open(b"pw", "p", &s).unwrap()[..], b"x");
    }
}
