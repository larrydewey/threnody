//! Long-term identities, fingerprints and safety numbers (spec §4).

use core::fmt;
use core::str::FromStr;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use getrandom::SysRng;
use zeroize::Zeroizing;

use crate::crypto::kdf::{derive, label};
use crate::error::{Error, Result};

/// A device's private signing identity (pseudonymous mode, spec §4.2).
pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        use rand_core::UnwrapErr;
        Self {
            key: SigningKey::generate(&mut UnwrapErr(SysRng)),
        }
    }

    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(seed),
        }
    }

    pub fn seed(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.key.to_bytes())
    }

    pub fn public(&self) -> PublicIdentity {
        PublicIdentity(self.key.verifying_key())
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.key.sign(msg).to_bytes()
    }
}

/// A peer's long-term public verification key: the primary identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicIdentity(VerifyingKey);

impl PublicIdentity {
    pub fn from_bytes(b: &[u8; 32]) -> Result<Self> {
        let vk = VerifyingKey::from_bytes(b).map_err(|_| Error::InvalidKey)?;
        if vk.is_weak() {
            return Err(Error::InvalidKey);
        }
        Ok(Self(vk))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    /// Strict (non-malleable, canonical) Ed25519 verification.
    pub fn verify(&self, msg: &[u8], sig: &[u8; 64]) -> Result<()> {
        self.0
            .verify_strict(msg, &Signature::from_bytes(sig))
            .map_err(|_| Error::BadSignature)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint(derive(label::FINGERPRINT, &[self.as_bytes()]))
    }
}

impl fmt::Debug for PublicIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicIdentity({})", self.fingerprint())
    }
}

pub(crate) const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 160-bit identity fingerprint, shown as 32 Crockford Base32 characters
/// (spec §4.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 20]);

impl Fingerprint {
    /// Plain 32-character form, no separators.
    pub fn compact(&self) -> String {
        let mut out = String::with_capacity(32);
        let mut acc: u64 = 0;
        let mut bits = 0;
        for &b in &self.0 {
            acc = (acc << 8) | u64::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
            }
        }
        out
    }
}

impl fmt::Display for Fingerprint {
    /// Grouped in fours for reading aloud: `ABCD-EFGH-...`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.compact();
        for (i, chunk) in c.as_bytes().chunks(4).enumerate() {
            if i > 0 {
                f.write_str("-")?;
            }
            f.write_str(core::str::from_utf8(chunk).map_err(|_| fmt::Error)?)?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// Decodes one Crockford symbol, folding the usual confusables.
pub(crate) fn crockford_value(c: u8) -> Option<u8> {
    let c = match c.to_ascii_uppercase() {
        b'O' => b'0',
        b'I' | b'L' => b'1',
        c => c,
    };
    CROCKFORD.iter().position(|&x| x == c).map(|p| p as u8)
}

impl FromStr for Fingerprint {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let syms = parse_symbols(s)?;
        if syms.len() != 32 {
            return Err(Error::InvalidFingerprint);
        }
        let mut out = [0u8; 20];
        let mut acc: u64 = 0;
        let mut bits = 0;
        let mut i = 0;
        for v in syms {
            acc = (acc << 5) | u64::from(v);
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out[i] = (acc >> bits) as u8;
                i += 1;
            }
        }
        Ok(Self(out))
    }
}

fn parse_symbols(s: &str) -> Result<Vec<u8>> {
    s.bytes()
        .filter(|b| !matches!(b, b'-' | b' '))
        .map(|b| crockford_value(b).ok_or(Error::InvalidFingerprint))
        .collect()
}

/// Returns true when `prefix` (separators ignored) is a prefix of `fp`.
pub fn fingerprint_matches_prefix(fp: &Fingerprint, prefix: &str) -> bool {
    let Ok(syms) = parse_symbols(prefix) else {
        return false;
    };
    let canon: String = syms
        .iter()
        .map(|&v| CROCKFORD[v as usize] as char)
        .collect();
    !canon.is_empty() && fp.compact().starts_with(&canon)
}

/// Symmetric 60-digit safety number for a pair of identities (spec §5.2).
/// Both sides compute the same digits regardless of argument order.
pub fn safety_number(a: &PublicIdentity, b: &PublicIdentity) -> String {
    let (lo, hi) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    let raw: [u8; 60] = derive(label::SAFETY_NUMBER, &[lo.as_bytes(), hi.as_bytes()]);
    raw.chunks(5)
        .map(|c| {
            let v = c.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b));
            format!("{:05}", v % 100_000)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_round_trips_and_tolerates_confusables() {
        let id = Identity::generate().public();
        let fp = id.fingerprint();
        let text = fp.to_string();
        assert_eq!(text.len(), 32 + 7);
        assert_eq!(text.parse::<Fingerprint>().unwrap(), fp);
        let lower = fp
            .compact()
            .to_lowercase()
            .replace('0', "o")
            .replace('1', "l");
        assert_eq!(lower.parse::<Fingerprint>().unwrap(), fp);
        assert!(fingerprint_matches_prefix(&fp, &text[..6]));
        assert!(!fingerprint_matches_prefix(&fp, ""));
    }

    #[test]
    fn safety_number_is_symmetric() {
        let a = Identity::generate().public();
        let b = Identity::generate().public();
        let n = safety_number(&a, &b);
        assert_eq!(n, safety_number(&b, &a));
        assert_eq!(n.split(' ').count(), 12);
    }

    #[test]
    fn signatures_verify() {
        let id = Identity::generate();
        let sig = id.sign(b"m");
        id.public().verify(b"m", &sig).unwrap();
        assert!(id.public().verify(b"n", &sig).is_err());
    }
}
