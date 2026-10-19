//! Handles: short names others can type to find us (Appendix S).
//!
//! A handle is `name-XXXX-XXXX`: a name its owner picks, then eight
//! Crockford symbols (40 bits) that commit to the owner's identity key:
//!
//! ```text
//! code       = Argon2id(id || version, "threnody handle v1" || name, 64 MiB, t 3, p 1)[0..5]
//! base       = KDF("… handle", normalized handle)
//! seed, salt, enc_key  from base and the hour's epoch     -- the owner's record
//! knock slot i: seed, salt, enc_key  from base, epoch, i  -- a finder's knock
//! value      = nonce (24) || XChaCha20-Poly1305(enc_key, nonce, pad(record), aad)
//! Record     = { 0: id bstr(32), 1: version uint, 2: Candidates }
//! Knock      = { 0: id bstr(32), 1: Candidates }
//! ```
//!
//! Anyone who knows the handle can read the record, and so learn the
//! owner's fingerprint and addresses: that's what a handle is for. No one
//! else can find it, and the code stops a key made up to match someone
//! else's handle (2^40 Argon2id runs per try).

use std::fmt;
use std::str::FromStr;

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::kdf::derive;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::{CROCKFORD, PublicIdentity, crockford_value};
use crate::rendezvous::{Candidates, RecordKeys};

/// Longest name, in characters.
pub const MAX_NAME: usize = 24;
/// Symbols in the code, 5 bits each.
const CODE_SYMBOLS: usize = 8;
/// Knock slots per epoch: finders pick one at random.
pub const KNOCK_SLOTS: u8 = 4;
const PADDED_LEN: usize = 448;
const NONCE_LEN: usize = 24;
/// Length of every handle record and knock: under BEP 44's 1,000 bytes.
pub const RECORD_LEN: usize = NONCE_LEN + PADDED_LEN + 16;

const COMMIT_SALT: &[u8] = b"threnody handle v1";
const BASE: &str = "threnody v1 2026-10-18 handle";
const SIGNING: &str = "threnody v1 2026-10-18 handle signing key";
const SALT: &str = "threnody v1 2026-10-18 handle salt";
const ENCRYPTION: &str = "threnody v1 2026-10-18 handle encryption";
const PROBE_KEY: &str = "threnody v1 2026-10-18 handle probe key";
const RECORD_AAD: &[u8] = b"threnody handle record v1";
const KNOCK_AAD: &[u8] = b"threnody handle knock v1";

/// A handle: its owner's name and the code that binds it to their key.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Handle {
    name: String,
    code: [u8; CODE_SYMBOLS],
}

impl Handle {
    /// The handle `id` gets for `name` (lower-cased) at `version`.
    /// Slow on purpose: an Argon2id run.
    pub fn new(id: &PublicIdentity, name: &str, version: u64) -> Result<Self> {
        let name = normalize_name(name)?;
        let code = code(id, &name, version)?;
        Ok(Self { name, code })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether this handle belongs to `id` at `version`: what a finder
    /// checks before trusting a record. Slow, like [`Handle::new`].
    pub fn belongs_to(&self, id: &PublicIdentity, version: u64) -> bool {
        code(id, &self.name, version).is_ok_and(|c| c == self.code)
    }

    fn base(&self) -> [u8; 32] {
        derive(BASE, &[self.to_string().as_bytes()])
    }

    /// Keys for the owner's record in `epoch`.
    pub fn record_keys(&self, epoch: u64) -> RecordKeys {
        let base = self.base();
        keys(&[&base, &epoch.to_le_bytes()])
    }

    /// Keys for knock slot `slot` in `epoch`.
    pub fn knock_keys(&self, epoch: u64, slot: u8) -> RecordKeys {
        let base = self.base();
        keys(&[&base, &epoch.to_le_bytes(), b"knock", &[slot]])
    }

    /// The key hole-punching probes between the owner and a finder use, in
    /// place of a contact's discovery key.
    pub fn probe_key(&self) -> [u8; 32] {
        derive(PROBE_KEY, &[&self.base()])
    }
}

fn keys(parts: &[&[u8]]) -> RecordKeys {
    RecordKeys {
        signing_seed: derive(SIGNING, parts),
        salt: derive(SALT, parts),
        enc_key: derive(ENCRYPTION, parts),
    }
}

fn normalize_name(name: &str) -> Result<String> {
    let name = name.trim().to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        && !name.starts_with('.')
        && !name.ends_with('.');
    if ok {
        Ok(name)
    } else {
        Err(Error::Malformed(
            "handle name: 1-24 letters, digits, '_' or '.'",
        ))
    }
}

fn code(id: &PublicIdentity, name: &str, version: u64) -> Result<[u8; CODE_SYMBOLS]> {
    let params = Params::new(64 * 1024, 3, 1, Some(32))
        .map_err(|_| Error::Malformed("handle KDF parameters"))?;
    let mut pw = id.as_bytes().to_vec();
    pw.extend_from_slice(&version.to_le_bytes());
    let mut salt = COMMIT_SALT.to_vec();
    salt.extend_from_slice(name.as_bytes());
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(&pw, &salt, &mut out)
        .map_err(|_| Error::Malformed("handle KDF failed"))?;
    // The first 40 bits, five at a time.
    let bits = u64::from_be_bytes([0, 0, 0, out[0], out[1], out[2], out[3], out[4]]);
    let mut c = [0u8; CODE_SYMBOLS];
    for (i, s) in c.iter_mut().enumerate() {
        *s = ((bits >> (35 - 5 * i)) & 31) as u8;
    }
    Ok(c)
}

impl fmt::Display for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sym = |i: usize| CROCKFORD[self.code[i] as usize] as char;
        write!(f, "{}-", self.name)?;
        for i in 0..4 {
            write!(f, "{}", sym(i))?;
        }
        f.write_str("-")?;
        for i in 4..8 {
            write!(f, "{}", sym(i))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Handle({self})")
    }
}

impl FromStr for Handle {
    type Err = Error;

    /// Takes `name-XXXX-XXXX`, also with spaces, an `@` in front, or the
    /// code's middle dash left out; case and Crockford confusables don't matter.
    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::Malformed("not a handle (name-XXXX-XXXX)");
        let s = s.trim().trim_start_matches('@').replace(' ', "-");
        // Names have no dashes: the code is everything after the first.
        let (name, rest) = s.split_once('-').ok_or_else(bad)?;
        let code = rest
            .bytes()
            .filter(|&b| b != b'-')
            .map(|b| crockford_value(b).ok_or_else(bad))
            .collect::<Result<Vec<u8>>>()?;
        if code.len() != CODE_SYMBOLS {
            return Err(bad());
        }
        let name = normalize_name(name)?;
        let mut c = [0u8; CODE_SYMBOLS];
        c.copy_from_slice(&code);
        Ok(Self { name, code: c })
    }
}

/// What the owner publishes: who they are, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleRecord {
    pub id: PublicIdentity,
    pub version: u64,
    pub candidates: Candidates,
}

/// A finder asking the owner to punch toward it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Knock {
    pub id: PublicIdentity,
    pub candidates: Candidates,
}

impl HandleRecord {
    pub fn seal(&self, keys: &RecordKeys) -> Result<Vec<u8>> {
        let cand = self.candidates.encode()?;
        let pt = cbor::to_vec(PADDED_LEN, |e| {
            e.map_len(3)?;
            e.u8(0)?.bytes(self.id.as_bytes())?;
            e.u8(1)?.uint(self.version)?;
            e.u8(2)?.bytes(&cand)?;
            Ok(())
        })?;
        seal(keys, pt, RECORD_AAD)
    }

    pub fn open(keys: &RecordKeys, value: &[u8]) -> Result<Self> {
        let pt = open(keys, value, RECORD_AAD)?;
        let mut dec = Decoder::new(&pt);
        let (mut id, mut version, mut cand) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => id = Some(fixed_bytes::<32>(d)?),
                1 => version = Some(d.u64()?),
                2 => cand = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            id: PublicIdentity::from_bytes(&required(id, "handle owner")?)?,
            version: required(version, "handle version")?,
            candidates: Candidates::decode(&required(cand, "handle candidates")?)?,
        })
    }
}

impl Knock {
    pub fn seal(&self, keys: &RecordKeys) -> Result<Vec<u8>> {
        let cand = self.candidates.encode()?;
        let pt = cbor::to_vec(PADDED_LEN, |e| {
            e.map_len(2)?;
            e.u8(0)?.bytes(self.id.as_bytes())?;
            e.u8(1)?.bytes(&cand)?;
            Ok(())
        })?;
        seal(keys, pt, KNOCK_AAD)
    }

    pub fn open(keys: &RecordKeys, value: &[u8]) -> Result<Self> {
        let pt = open(keys, value, KNOCK_AAD)?;
        let mut dec = Decoder::new(&pt);
        let (mut id, mut cand) = (None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => id = Some(fixed_bytes::<32>(d)?),
                1 => cand = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            id: PublicIdentity::from_bytes(&required(id, "knocker")?)?,
            candidates: Candidates::decode(&required(cand, "knock candidates")?)?,
        })
    }
}

fn seal(keys: &RecordKeys, mut pt: Vec<u8>, aad: &[u8]) -> Result<Vec<u8>> {
    if pt.len() >= PADDED_LEN {
        return Err(Error::Malformed("handle record too large"));
    }
    pt.push(0x80);
    pt.resize(PADDED_LEN, 0);
    let nonce: [u8; NONCE_LEN] = random_bytes();
    let ct = XChaCha20Poly1305::new((&keys.enc_key).into())
        .encrypt((&nonce).into(), Payload { msg: &pt, aad })
        .map_err(|_| Error::Malformed("handle record encryption"))?;
    let mut out = Vec::with_capacity(RECORD_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn open(keys: &RecordKeys, value: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if value.len() != RECORD_LEN {
        return Err(Error::Malformed("handle record length"));
    }
    let (nonce, ct) = value.split_at(NONCE_LEN);
    let nonce: &[u8; NONCE_LEN] = nonce
        .try_into()
        .map_err(|_| Error::Malformed("handle record nonce"))?;
    let pt = XChaCha20Poly1305::new((&keys.enc_key).into())
        .decrypt(nonce.into(), Payload { msg: ct, aad })
        .map_err(|_| Error::Malformed("handle record authentication"))?;
    crate::message::unpad(pt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::rendezvous::{Candidate, CandidateKind};

    fn candidates() -> Candidates {
        Candidates {
            list: vec![Candidate {
                kind: CandidateKind::Reflexive,
                addr: "203.0.113.7:7450".parse().unwrap(),
            }],
            issued_ms: 42,
            flags: 0,
            seeking_until_ms: None,
        }
    }

    #[test]
    fn handles_print_parse_and_belong_to_their_owner() {
        let (a, b) = (Identity::generate().public(), Identity::generate().public());
        let h = Handle::new(&a, "Larry", 0).unwrap();
        let s = h.to_string();
        assert!(
            s.starts_with("larry-") && s.len() == "larry-".len() + 9,
            "{s}"
        );
        assert_eq!(s.parse::<Handle>().unwrap(), h);
        // Forgiving input: an @, spaces, lower case, confusables, no dash.
        let code = &s[6..];
        let loose = format!(
            "@LARRY {}",
            code.replace('-', "")
                .to_lowercase()
                .replace('1', "l")
                .replace('0', "o")
        );
        assert_eq!(loose.parse::<Handle>().unwrap(), h);
        assert!(h.belongs_to(&a, 0));
        assert!(!h.belongs_to(&a, 1));
        assert!(!h.belongs_to(&b, 0));
        assert_ne!(Handle::new(&a, "larry", 1).unwrap(), h);
    }

    #[test]
    fn bad_handles_are_refused() {
        for s in [
            "",
            "larry",
            "-ABCD-EFGH",
            "la rry!-ABCD-EFGH",
            "larry-ABCD-EFG",
            ".x-ABCD-EFGH",
        ] {
            assert!(s.parse::<Handle>().is_err(), "{s}");
        }
        let a = Identity::generate().public();
        assert!(Handle::new(&a, &"x".repeat(MAX_NAME + 1), 0).is_err());
    }

    #[test]
    fn records_and_knocks_round_trip_and_need_the_handle() {
        let a = Identity::generate().public();
        let h = Handle::new(&a, "larry", 3).unwrap();
        let other = Handle::new(&a, "larrz", 3).unwrap();
        let r = HandleRecord {
            id: a,
            version: 3,
            candidates: candidates(),
        };
        let v = r.seal(&h.record_keys(7)).unwrap();
        assert_eq!(v.len(), RECORD_LEN);
        assert_eq!(HandleRecord::open(&h.record_keys(7), &v).unwrap(), r);
        assert!(HandleRecord::open(&h.record_keys(8), &v).is_err());
        assert!(HandleRecord::open(&other.record_keys(7), &v).is_err());

        let k = Knock {
            id: Identity::generate().public(),
            candidates: candidates(),
        };
        let v = k.seal(&h.knock_keys(7, 2)).unwrap();
        assert_eq!(v.len(), RECORD_LEN);
        assert_eq!(Knock::open(&h.knock_keys(7, 2), &v).unwrap(), k);
        assert!(Knock::open(&h.knock_keys(7, 1), &v).is_err());
        // A knock never opens as a record, or the other way round.
        assert!(HandleRecord::open(&h.knock_keys(7, 2), &v).is_err());
        assert_ne!(h.record_keys(7).salt, h.knock_keys(7, 0).salt);
    }
}
