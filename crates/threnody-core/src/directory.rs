//! Volunteer relay directories (Appendix P, spec §5.1 and §9 layer 2).
//!
//! A volunteer relay describes itself in a descriptor it signs. A
//! directory lists the descriptors it vouches for in a document signed with
//! both of its issuer keys (Ed25519 and ML-DSA-65; `credential::Issuer`).
//! Users subscribe to directories by link, which pins the directory's
//! issuer id and so both of its keys, and use a relay only when at least
//! `k` of their directories list it.
//!
//! ```text
//! Descriptor = { 0: 1, 1: identity (32), 2: [* addr tstr], 3: published_ms,
//!                4: expires_ms, 5: sig (64) }
//! Document   = { 0: 1, 1: issuer_id (32), 2: published_ms, 3: valid_until_ms,
//!                4: [* Descriptor bstr], 5: sig_ed (64), 6: sig_mldsa }
//! link       = "threnody-dir://" base32(issuer_id) "@" host ":" port
//! ```

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::credential::{Issuer, IssuerKey};
use crate::crypto::kdf::{derive, label};
use crate::error::{Error, Result};
use crate::identity::{CROCKFORD, Identity, PublicIdentity, crockford_value};

/// Addresses one descriptor may list.
pub const MAX_ADDRS: usize = 8;
pub const MAX_ADDR_LEN: usize = 64;
/// A descriptor lives at most this long; relays republish well before.
pub const MAX_DESCRIPTOR_LIFE_MS: u64 = 48 * 3_600_000;
/// Relays one document may list.
pub const MAX_RELAYS: usize = 1024;
/// A document is valid at most this long after it was published.
pub const MAX_DOCUMENT_LIFE_MS: u64 = 48 * 3_600_000;
/// Clock skew tolerated on publication times.
const SKEW_MS: u64 = 10 * 60_000;
pub const LINK_SCHEME: &str = "threnody-dir://";

/// A volunteer relay's self-signed description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayDescriptor {
    pub identity: PublicIdentity,
    pub addrs: Vec<String>,
    pub published_ms: u64,
    pub expires_ms: u64,
    sig: [u8; 64],
}

fn descriptor_digest(
    identity: &PublicIdentity,
    addrs: &[String],
    published: u64,
    expires: u64,
) -> [u8; 64] {
    let count = (addrs.len() as u64).to_le_bytes();
    let (p, e) = (published.to_le_bytes(), expires.to_le_bytes());
    let mut parts: Vec<&[u8]> = vec![identity.as_bytes(), &p, &e, &count];
    parts.extend(addrs.iter().map(String::as_bytes));
    derive(label::SIG_RELAY_DESCRIPTOR, &parts)
}

fn check_addrs(addrs: &[String]) -> Result<()> {
    if addrs.is_empty()
        || addrs.len() > MAX_ADDRS
        || addrs.iter().any(|a| {
            a.is_empty()
                || a.len() > MAX_ADDR_LEN
                || a.chars().any(|c| c.is_control() || c.is_whitespace())
        })
    {
        return Err(Error::Malformed("relay addresses"));
    }
    Ok(())
}

impl RelayDescriptor {
    pub fn new(identity: &Identity, addrs: Vec<String>, now_ms: u64, life_ms: u64) -> Result<Self> {
        check_addrs(&addrs)?;
        let expires_ms = now_ms + life_ms.min(MAX_DESCRIPTOR_LIFE_MS);
        let sig = identity.sign(&descriptor_digest(
            &identity.public(),
            &addrs,
            now_ms,
            expires_ms,
        ));
        Ok(Self {
            identity: identity.public(),
            addrs,
            published_ms: now_ms,
            expires_ms,
            sig,
        })
    }

    /// Checks the signature and the times (not whether it has expired yet).
    pub fn verify(&self) -> Result<()> {
        check_addrs(&self.addrs)?;
        if self.expires_ms <= self.published_ms
            || self.expires_ms - self.published_ms > MAX_DESCRIPTOR_LIFE_MS
        {
            return Err(Error::Malformed("descriptor lifetime"));
        }
        self.identity.verify(
            &descriptor_digest(
                &self.identity,
                &self.addrs,
                self.published_ms,
                self.expires_ms,
            ),
            &self.sig,
        )
    }

    pub fn live(&self, now_ms: u64) -> bool {
        self.published_ms <= now_ms + SKEW_MS && now_ms < self.expires_ms
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(256 + self.addrs.len() * MAX_ADDR_LEN, |e| {
            e.map_len(6)?.u8(0)?.u8(1)?;
            e.u8(1)?.bytes(self.identity.as_bytes())?;
            e.u8(2)?.array_len(self.addrs.len())?;
            for a in &self.addrs {
                e.str(a)?;
            }
            e.u8(3)?.u64(self.published_ms)?;
            e.u8(4)?.u64(self.expires_ms)?;
            e.u8(5)?.bytes(&self.sig)?;
            Ok(())
        })
    }

    /// Decodes and verifies a descriptor.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut v, mut id, mut addrs, mut p, mut x, mut sig) =
            (None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => v = Some(d.u64()?),
                1 => id = Some(fixed_bytes::<32>(d)?),
                2 => {
                    let n = d.array_len()?;
                    if n > MAX_ADDRS {
                        return Err(Error::Malformed("too many relay addresses"));
                    }
                    let mut a = Vec::with_capacity(n);
                    for _ in 0..n {
                        a.push(d.str()?.to_owned());
                    }
                    addrs = Some(a);
                }
                3 => p = Some(d.u64()?),
                4 => x = Some(d.u64()?),
                5 => sig = Some(fixed_bytes::<64>(d)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if required(v, "descriptor version")? != 1 {
            return Err(Error::UnsupportedVersion(v.unwrap_or(0)));
        }
        let desc = Self {
            identity: PublicIdentity::from_bytes(&required(id, "relay identity")?)?,
            addrs: required(addrs, "relay addresses")?,
            published_ms: required(p, "published")?,
            expires_ms: required(x, "expires")?,
            sig: required(sig, "relay signature")?,
        };
        desc.verify()?;
        Ok(desc)
    }
}

/// A directory's signed list of relays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryDoc {
    pub issuer: [u8; 32],
    pub published_ms: u64,
    pub valid_until_ms: u64,
    pub relays: Vec<RelayDescriptor>,
    raw: Vec<Vec<u8>>,
    sig_ed: [u8; 64],
    sig_pq: Vec<u8>,
}

fn document_digest(
    issuer: &[u8; 32],
    published: u64,
    valid_until: u64,
    raw: &[Vec<u8>],
) -> [u8; 64] {
    let (p, v) = (published.to_le_bytes(), valid_until.to_le_bytes());
    let count = (raw.len() as u64).to_le_bytes();
    let mut parts: Vec<&[u8]> = vec![issuer, &p, &v, &count];
    parts.extend(raw.iter().map(Vec::as_slice));
    derive(label::SIG_DIRECTORY, &parts)
}

impl DirectoryDoc {
    /// Publishes `relays` (live ones only), valid for `life_ms`.
    pub fn sign(
        issuer: &Issuer,
        relays: &[RelayDescriptor],
        now_ms: u64,
        life_ms: u64,
    ) -> Result<Self> {
        let relays: Vec<RelayDescriptor> = relays
            .iter()
            .filter(|r| r.live(now_ms))
            .take(MAX_RELAYS)
            .cloned()
            .collect();
        let raw = relays
            .iter()
            .map(RelayDescriptor::encode)
            .collect::<Result<Vec<_>>>()?;
        let id = issuer.key().id();
        let valid_until_ms = now_ms + life_ms.min(MAX_DOCUMENT_LIFE_MS);
        let (sig_ed, sig_pq) =
            issuer.sign_document(&document_digest(&id, now_ms, valid_until_ms, &raw))?;
        Ok(Self {
            issuer: id,
            published_ms: now_ms,
            valid_until_ms,
            relays,
            raw,
            sig_ed,
            sig_pq,
        })
    }

    /// Checks the document against its directory's (pinned) key, and that
    /// it is current.
    pub fn verify(&self, key: &IssuerKey, now_ms: u64) -> Result<()> {
        if key.id() != self.issuer {
            return Err(Error::Malformed("document from another directory"));
        }
        key.verify_document(
            &document_digest(
                &self.issuer,
                self.published_ms,
                self.valid_until_ms,
                &self.raw,
            ),
            &self.sig_ed,
            &self.sig_pq,
        )?;
        if self.published_ms > now_ms + SKEW_MS
            || self.valid_until_ms <= now_ms
            || self.valid_until_ms - self.published_ms > MAX_DOCUMENT_LIFE_MS
        {
            return Err(Error::Malformed("directory document out of date"));
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let size = 4096 + self.raw.iter().map(Vec::len).sum::<usize>();
        cbor::to_vec(size, |e| {
            e.map_len(7)?.u8(0)?.u8(1)?;
            e.u8(1)?.bytes(&self.issuer)?;
            e.u8(2)?.u64(self.published_ms)?;
            e.u8(3)?.u64(self.valid_until_ms)?;
            e.u8(4)?.array_len(self.raw.len())?;
            for r in &self.raw {
                e.bytes(r)?;
            }
            e.u8(5)?.bytes(&self.sig_ed)?;
            e.u8(6)?.bytes(&self.sig_pq)?;
            Ok(())
        })
    }

    /// Decodes a document; every descriptor in it must verify. Call
    /// [`DirectoryDoc::verify`] before trusting it.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut v, mut is, mut p, mut u, mut raw, mut se, mut sp) =
            (None, None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => v = Some(d.u64()?),
                1 => is = Some(fixed_bytes::<32>(d)?),
                2 => p = Some(d.u64()?),
                3 => u = Some(d.u64()?),
                4 => {
                    let n = d.array_len()?;
                    if n > MAX_RELAYS {
                        return Err(Error::Malformed("too many relays"));
                    }
                    let mut r = Vec::with_capacity(n);
                    for _ in 0..n {
                        r.push(d.bytes()?.to_vec());
                    }
                    raw = Some(r);
                }
                5 => se = Some(fixed_bytes::<64>(d)?),
                6 => sp = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if required(v, "document version")? != 1 {
            return Err(Error::UnsupportedVersion(v.unwrap_or(0)));
        }
        let raw = required(raw, "relays")?;
        let relays = raw
            .iter()
            .map(|r| RelayDescriptor::decode(r))
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            issuer: required(is, "issuer id")?,
            published_ms: required(p, "published")?,
            valid_until_ms: required(u, "valid until")?,
            relays,
            raw,
            sig_ed: required(se, "directory signature")?,
            sig_pq: required(sp, "directory ml-dsa signature")?,
        })
    }
}

/// The relays at least `k` of the given (verified) documents list, each
/// with its most recently published live descriptor.
pub fn select(docs: &[DirectoryDoc], k: usize, now_ms: u64) -> Vec<RelayDescriptor> {
    let k = k.max(1);
    let mut seen: HashMap<PublicIdentity, (usize, RelayDescriptor)> = HashMap::new();
    for doc in docs {
        let mut in_doc: Vec<PublicIdentity> = Vec::new();
        for r in doc.relays.iter().filter(|r| r.live(now_ms)) {
            if in_doc.contains(&r.identity) {
                continue;
            }
            in_doc.push(r.identity);
            let entry = seen.entry(r.identity).or_insert((0, r.clone()));
            entry.0 += 1;
            if r.published_ms > entry.1.published_ms {
                entry.1 = r.clone();
            }
        }
    }
    let mut out: Vec<RelayDescriptor> = seen
        .into_values()
        .filter(|(n, _)| *n >= k)
        .map(|(_, r)| r)
        .collect();
    out.sort_by_key(|r| *r.identity.as_bytes());
    out
}

/// Directory messages (`AppMessage::Directory`), over anonymous links.
///
/// ```text
/// DirMsg = { 0: op, 1: id u64, ? 2: IssuerKey, ? 3: Document, ? 4: epoch,
///            ? 5: commitment, ? 6: Issued, ? 7: Descriptor, ? 8: status, ? 9: reason }
/// op: 1 get key, 2 key, 3 get document, 4 document, 5 token request,
///     6 token, 7 register, 8 registered, 9 refused
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirMsg {
    GetKey {
        id: u64,
    },
    Key {
        id: u64,
        key: Vec<u8>,
    },
    GetDocument {
        id: u64,
    },
    Document {
        id: u64,
        doc: Vec<u8>,
    },
    TokenRequest {
        id: u64,
        epoch: u32,
        commitment: Vec<u8>,
    },
    Token {
        id: u64,
        issued: Vec<u8>,
    },
    Register {
        id: u64,
        descriptor: Vec<u8>,
    },
    Registered {
        id: u64,
        status: RegisterStatus,
    },
    Refused {
        id: u64,
        reason: String,
    },
}

/// What a directory did with a relay's registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RegisterStatus {
    /// The relay is listed from the next document on.
    Listed = 1,
    /// The directory's operator reviews relays before listing them.
    Pending = 2,
}

impl DirMsg {
    pub fn id(&self) -> u64 {
        match self {
            Self::GetKey { id }
            | Self::Key { id, .. }
            | Self::GetDocument { id }
            | Self::Document { id, .. }
            | Self::TokenRequest { id, .. }
            | Self::Token { id, .. }
            | Self::Register { id, .. }
            | Self::Registered { id, .. }
            | Self::Refused { id, .. } => *id,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let size = match self {
            Self::Document { doc, .. } => doc.len() + 64,
            _ => 8192,
        };
        cbor::to_vec(size, |e| {
            match self {
                Self::GetKey { id } => {
                    e.map_len(2)?.u8(0)?.u8(1)?.u8(1)?.u64(*id)?;
                }
                Self::Key { id, key } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(2)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(2)?
                        .bytes(key)?;
                }
                Self::GetDocument { id } => {
                    e.map_len(2)?.u8(0)?.u8(3)?.u8(1)?.u64(*id)?;
                }
                Self::Document { id, doc } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(4)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(3)?
                        .bytes(doc)?;
                }
                Self::TokenRequest {
                    id,
                    epoch,
                    commitment,
                } => {
                    e.map_len(4)?.u8(0)?.u8(5)?.u8(1)?.u64(*id)?;
                    e.u8(4)?.u32(*epoch)?.u8(5)?.bytes(commitment)?;
                }
                Self::Token { id, issued } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(6)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(6)?
                        .bytes(issued)?;
                }
                Self::Register { id, descriptor } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(7)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(7)?
                        .bytes(descriptor)?;
                }
                Self::Registered { id, status } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(8)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(8)?
                        .u8(*status as u8)?;
                }
                Self::Refused { id, reason } => {
                    e.map_len(3)?
                        .u8(0)?
                        .u8(9)?
                        .u8(1)?
                        .u64(*id)?
                        .u8(9)?
                        .str(reason)?;
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut id, mut key, mut doc, mut epoch, mut commit) =
            (None, None, None, None, None, None);
        let (mut issued, mut desc, mut status, mut reason) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => id = Some(d.u64()?),
                2 => key = Some(d.bytes()?.to_vec()),
                3 => doc = Some(d.bytes()?.to_vec()),
                4 => epoch = Some(d.u32()?),
                5 => {
                    let c = d.bytes()?;
                    if c.len() > 512 {
                        return Err(Error::Malformed("commitment too long"));
                    }
                    commit = Some(c.to_vec());
                }
                6 => issued = Some(d.bytes()?.to_vec()),
                7 => desc = Some(d.bytes()?.to_vec()),
                8 => status = Some(d.u8()?),
                9 => {
                    let r = d.str()?;
                    reason = Some(r.chars().take(200).collect::<String>());
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let id = required(id, "request id")?;
        Ok(match required(op, "directory op")? {
            1 => Self::GetKey { id },
            2 => Self::Key {
                id,
                key: required(key, "issuer key")?,
            },
            3 => Self::GetDocument { id },
            4 => Self::Document {
                id,
                doc: required(doc, "document")?,
            },
            5 => Self::TokenRequest {
                id,
                epoch: required(epoch, "epoch")?,
                commitment: required(commit, "commitment")?,
            },
            6 => Self::Token {
                id,
                issued: required(issued, "token")?,
            },
            7 => Self::Register {
                id,
                descriptor: required(desc, "descriptor")?,
            },
            8 => Self::Registered {
                id,
                status: match required(status, "status")? {
                    1 => RegisterStatus::Listed,
                    2 => RegisterStatus::Pending,
                    other => return Err(Error::UnexpectedType(u64::from(other))),
                },
            },
            9 => Self::Refused {
                id,
                reason: required(reason, "reason")?,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// How a user subscribes to a directory: its issuer id and where to reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryLink {
    pub issuer: [u8; 32],
    pub addr: String,
}

fn base32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
        }
        acc &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(CROCKFORD[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn unbase32<const N: usize>(s: &str) -> Result<[u8; N]> {
    let mut out = [0u8; N];
    let (mut acc, mut bits, mut i) = (0u32, 0, 0);
    for c in s.bytes().filter(|b| *b != b'-') {
        let v = crockford_value(c).ok_or(Error::Malformed("directory link"))?;
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if i == N {
                return Err(Error::Malformed("directory link"));
            }
            out[i] = (acc >> bits) as u8;
            i += 1;
        }
        acc &= (1 << bits) - 1;
    }
    // All bytes present, and the padding bits are zero.
    if i != N || acc != 0 {
        return Err(Error::Malformed("directory link"));
    }
    Ok(out)
}

impl fmt::Display for DirectoryLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{LINK_SCHEME}{}@{}", base32(&self.issuer), self.addr)
    }
}

impl FromStr for DirectoryLink {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let rest = s
            .trim()
            .strip_prefix(LINK_SCHEME)
            .ok_or(Error::Malformed("directory link"))?;
        let (id, addr) = rest
            .split_once('@')
            .ok_or(Error::Malformed("directory link"))?;
        check_addrs(&[addr.to_owned()])?;
        Ok(Self {
            issuer: unbase32::<32>(id)?,
            addr: addr.to_owned(),
        })
    }
}

/// A short form of an issuer id for display (its first 8 symbols).
pub fn short_id(issuer: &[u8; 32]) -> String {
    base32(issuer).chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(now: u64) -> (Identity, RelayDescriptor) {
        let id = Identity::generate();
        let d =
            RelayDescriptor::new(&id, vec!["192.0.2.1:7450".into()], now, 24 * 3_600_000).unwrap();
        (id, d)
    }

    #[test]
    fn descriptors_round_trip_and_reject_tampering() {
        let now = crate::now_ms();
        let (_, d) = relay(now);
        let b = d.encode().unwrap();
        assert_eq!(RelayDescriptor::decode(&b).unwrap(), d);
        let mut forged = d.clone();
        forged.addrs = vec!["198.51.100.9:7450".into()];
        assert!(RelayDescriptor::decode(&forged.encode().unwrap()).is_err());
        let id = Identity::generate();
        assert!(RelayDescriptor::new(&id, vec![], now, 1).is_err());
        assert!(RelayDescriptor::new(&id, vec!["a b".into()], now, 1).is_err());
    }

    #[test]
    fn documents_need_both_signatures_and_the_pinned_key() {
        let now = crate::now_ms();
        let dir = Issuer::new(&Identity::generate(), 0).unwrap();
        let relays: Vec<_> = (0..3).map(|_| relay(now).1).collect();
        let doc = DirectoryDoc::sign(&dir, &relays, now, 3_600_000).unwrap();
        let doc = DirectoryDoc::decode(&doc.encode().unwrap()).unwrap();
        doc.verify(dir.key(), now).unwrap();
        assert_eq!(doc.relays.len(), 3);
        // Another directory's key, or an expired document, fails.
        let other = Issuer::new(&Identity::generate(), 0).unwrap();
        assert!(doc.verify(other.key(), now).is_err());
        assert!(doc.verify(dir.key(), now + 2 * 3_600_000).is_err());
        // Dropping a relay breaks the signatures.
        let mut cut = doc.clone();
        cut.raw.pop();
        cut.relays.pop();
        assert!(cut.verify(dir.key(), now).is_err());
        // So does a valid Ed25519 signature with a wrong ML-DSA one.
        let mut half = doc.clone();
        half.sig_pq[10] ^= 1;
        assert!(half.verify(dir.key(), now).is_err());
    }

    #[test]
    fn selection_needs_k_directories() {
        let now = crate::now_ms();
        let (a, b) = (
            Issuer::new(&Identity::generate(), 0).unwrap(),
            Issuer::new(&Identity::generate(), 0).unwrap(),
        );
        let (_, r1) = relay(now);
        let (_, r2) = relay(now);
        let da = DirectoryDoc::sign(&a, &[r1.clone(), r2.clone()], now, 3_600_000).unwrap();
        let db = DirectoryDoc::sign(&b, std::slice::from_ref(&r1), now, 3_600_000).unwrap();
        let docs = [da, db];
        assert_eq!(select(&docs, 1, now).len(), 2);
        let both = select(&docs, 2, now);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].identity, r1.identity);
        assert!(select(&docs, 3, now).is_empty());
    }

    #[test]
    fn messages_round_trip() {
        let msgs = [
            DirMsg::GetKey { id: 1 },
            DirMsg::Key {
                id: 2,
                key: vec![1],
            },
            DirMsg::GetDocument { id: 3 },
            DirMsg::Document {
                id: 4,
                doc: vec![2; 300],
            },
            DirMsg::TokenRequest {
                id: 5,
                epoch: 7,
                commitment: vec![3; 176],
            },
            DirMsg::Token {
                id: 6,
                issued: vec![4],
            },
            DirMsg::Register {
                id: 7,
                descriptor: vec![5],
            },
            DirMsg::Registered {
                id: 8,
                status: RegisterStatus::Pending,
            },
            DirMsg::Refused {
                id: 9,
                reason: "full".into(),
            },
        ];
        for m in msgs {
            assert_eq!(DirMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(DirMsg::decode(&[0xa0]).is_err());
    }

    #[test]
    fn links_round_trip() {
        let l = DirectoryLink {
            issuer: [0xA5; 32],
            addr: "dir.example.org:7450".into(),
        };
        let s = l.to_string();
        assert!(s.starts_with(LINK_SCHEME));
        assert_eq!(s.parse::<DirectoryLink>().unwrap(), l);
        assert!(s.replace("@", "#").parse::<DirectoryLink>().is_err());
        assert!(
            format!("{LINK_SCHEME}AAAA@h:1")
                .parse::<DirectoryLink>()
                .is_err()
        );
        let all: Vec<u8> = (0..32).collect();
        assert_eq!(unbase32::<32>(&base32(&all)).unwrap().to_vec(), all);
    }
}
