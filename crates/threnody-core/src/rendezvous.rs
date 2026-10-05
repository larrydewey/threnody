//! Rendezvous records and hole-punching probes (Appendix N).
//!
//! Two mutually approved contacts find each other's current addresses in
//! the public Mainline DHT (BEP 44) without revealing who they are. Every
//! key comes from the pairwise discovery key `D` (see `discovery`):
//!
//! ```text
//! epoch      = unix_seconds / 3600
//! seed(X→Y)  = KDF("… rendezvous signing key", D, id_X, epoch)   -- X's records for Y
//! salt       = KDF("… rendezvous salt", D, id_X, epoch)[0..16]
//! enc_key    = KDF("… rendezvous encryption", D, id_X, epoch)
//! value      = nonce (24) || XChaCha20-Poly1305(enc_key, nonce, pad(Candidates), aad)
//! Candidates = { 0: [* [kind, addr bstr, port]], 1: issued_ms, 2: flags, ? 3: seeking_until_ms }
//! ```
//!
//! The DHT signing key is an Ed25519 key pair built from `seed`; the caller
//! does that with whichever Ed25519 implementation its DHT client uses.
//!
//! A probe is a 32-byte UDP datagram, `nonce (16) || tag (16)`, with the
//! QUIC fixed bit (0x40) and long-header bit (0x80) of the first byte
//! cleared, so a QUIC endpoint never takes it for a packet. Only the
//! contact holding `D` recognises it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use const_cbor::Decoder;

use crate::cbor::{self, finish, read_map, required};
use crate::crypto::kdf::derive;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::PublicIdentity;

pub const EPOCH_SECS: u64 = 3600;
/// Most candidates one record carries.
pub const MAX_CANDIDATES: usize = 12;
/// Every record's plaintext is padded to this length, so its size doesn't
/// say how many candidates a node has.
pub const PADDED_LEN: usize = 384;
const NONCE_LEN: usize = 24;
/// Length of every record value: under BEP 44's 1,000-byte limit.
pub const RECORD_LEN: usize = NONCE_LEN + PADDED_LEN + 16;
pub const PROBE_LEN: usize = 32;
/// Record flag: this node's NAT looks symmetric (a new outside port for
/// every destination).
pub const FLAG_SYMMETRIC: u64 = 1;

const SIGNING: &str = "threnody v1 2026-10-05 rendezvous signing key";
const SALT: &str = "threnody v1 2026-10-05 rendezvous salt";
const ENCRYPTION: &str = "threnody v1 2026-10-05 rendezvous encryption";
const PROBE: &str = "threnody v1 2026-10-05 hole punching probe";
const AAD: &[u8] = b"threnody rendezvous v1";

/// The DHT keys for the records `sender` publishes for one contact.
pub struct RecordKeys {
    /// Seed of the Ed25519 key pair the record is stored (and signed) under.
    pub signing_seed: [u8; 32],
    pub salt: [u8; 16],
    enc_key: [u8; 32],
}

impl Drop for RecordKeys {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.signing_seed.zeroize();
        self.enc_key.zeroize();
    }
}

/// The rendezvous epoch at `now_secs`.
pub fn epoch(now_secs: u64) -> u64 {
    now_secs / EPOCH_SECS
}

/// Keys for `sender`'s records in `epoch`, under discovery key `d`. Both
/// contacts can derive both directions.
pub fn record_keys(d: &[u8; 32], sender: &PublicIdentity, epoch: u64) -> RecordKeys {
    let parts: [&[u8]; 3] = [d, sender.as_bytes(), &epoch.to_le_bytes()];
    RecordKeys {
        signing_seed: derive(SIGNING, &parts),
        salt: derive(SALT, &parts),
        enc_key: derive(ENCRYPTION, &parts),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    /// Our address as seen from outside the NAT.
    Reflexive = 1,
    /// A LAN address, for two devices behind the same NAT.
    Local = 2,
    /// A global IPv6 address (no NAT, only a firewall).
    Ipv6 = 3,
}

impl CandidateKind {
    fn from_wire(v: u64) -> Option<Self> {
        match v {
            1 => Some(Self::Reflexive),
            2 => Some(Self::Local),
            3 => Some(Self::Ipv6),
            _ => None,
        }
    }

    /// Dialing order: IPv6, then local, then reflexive.
    pub fn rank(self) -> u8 {
        match self {
            Self::Ipv6 => 0,
            Self::Local => 1,
            Self::Reflexive => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Candidate {
    pub kind: CandidateKind,
    pub addr: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidates {
    pub list: Vec<Candidate>,
    pub issued_ms: u64,
    pub flags: u64,
    /// The sender wants to reach the reader until then.
    pub seeking_until_ms: Option<u64>,
}

impl Candidates {
    fn encode(&self) -> Result<Vec<u8>> {
        let list = &self.list[..self.list.len().min(MAX_CANDIDATES)];
        cbor::to_vec(PADDED_LEN, |e| {
            e.map_len(3 + usize::from(self.seeking_until_ms.is_some()))?;
            e.u8(0)?.array_len(list.len())?;
            for c in list {
                e.array_len(3)?.u8(c.kind as u8)?;
                match c.addr.ip() {
                    IpAddr::V4(a) => e.bytes(&a.octets())?,
                    IpAddr::V6(a) => e.bytes(&a.octets())?,
                };
                e.u16(c.addr.port())?;
            }
            e.u8(1)?.uint(self.issued_ms)?;
            e.u8(2)?.uint(self.flags)?;
            if let Some(t) = self.seeking_until_ms {
                e.u8(3)?.uint(t)?;
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut list, mut issued, mut flags, mut seeking) = (None, None, 0, None);
        read_map(&mut dec, |key, d| {
            match key {
                0 => {
                    let n = d.array_len()?;
                    if n > MAX_CANDIDATES {
                        return Err(Error::Malformed("too many candidates"));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        if d.array_len()? != 3 {
                            return Err(Error::Malformed("candidate"));
                        }
                        let kind = d.u64()?;
                        let ip = match d.bytes()? {
                            b if b.len() == 4 => IpAddr::V4(Ipv4Addr::from(
                                <[u8; 4]>::try_from(b).unwrap_or_default(),
                            )),
                            b if b.len() == 16 => IpAddr::V6(Ipv6Addr::from(
                                <[u8; 16]>::try_from(b).unwrap_or_default(),
                            )),
                            _ => return Err(Error::Malformed("candidate address")),
                        };
                        let port = d.u16()?;
                        // Unknown kinds (a later relay kind, say) are skipped.
                        if let Some(kind) = CandidateKind::from_wire(kind) {
                            v.push(Candidate {
                                kind,
                                addr: SocketAddr::new(ip, port),
                            });
                        }
                    }
                    list = Some(v);
                }
                1 => issued = Some(d.u64()?),
                2 => flags = d.u64()?,
                3 => seeking = Some(d.u64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            list: required(list, "candidates")?,
            issued_ms: required(issued, "issued time")?,
            flags,
            seeking_until_ms: seeking,
        })
    }
}

/// Encrypts `c` into a record value of exactly [`RECORD_LEN`] bytes.
pub fn seal_record(keys: &RecordKeys, c: &Candidates) -> Result<Vec<u8>> {
    let mut pt = c.encode()?;
    if pt.len() >= PADDED_LEN {
        return Err(Error::Malformed("record too large"));
    }
    pt.push(0x80);
    pt.resize(PADDED_LEN, 0);
    let nonce: [u8; NONCE_LEN] = random_bytes();
    let ct = XChaCha20Poly1305::new((&keys.enc_key).into())
        .encrypt((&nonce).into(), Payload { msg: &pt, aad: AAD })
        .map_err(|_| Error::Malformed("record encryption"))?;
    let mut out = Vec::with_capacity(RECORD_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypts a record value sealed with [`seal_record`].
pub fn open_record(keys: &RecordKeys, value: &[u8]) -> Result<Candidates> {
    if value.len() != RECORD_LEN {
        return Err(Error::Malformed("record length"));
    }
    let (nonce, ct) = value.split_at(NONCE_LEN);
    let pt = XChaCha20Poly1305::new((&keys.enc_key).into())
        .decrypt(nonce.into(), Payload { msg: ct, aad: AAD })
        .map_err(|_| Error::Malformed("record authentication"))?;
    let pt = crate::message::unpad(pt)?;
    Candidates::decode(&pt)
}

fn probe_tag(d: &[u8; 32], sender: &PublicIdentity, nonce: &[u8]) -> [u8; 16] {
    derive(PROBE, &[d, sender.as_bytes(), nonce])
}

/// A probe from `sender` that the holder of `d` recognises.
pub fn probe(d: &[u8; 32], sender: &PublicIdentity) -> [u8; PROBE_LEN] {
    let mut nonce: [u8; 16] = random_bytes();
    nonce[0] &= 0x3f;
    let mut out = [0u8; PROBE_LEN];
    out[..16].copy_from_slice(&nonce);
    out[16..].copy_from_slice(&probe_tag(d, sender, &nonce));
    out
}

/// Whether a datagram could be a probe (rather than a QUIC packet).
pub fn looks_like_probe(data: &[u8]) -> bool {
    data.len() == PROBE_LEN && data[0] & 0xc0 == 0
}

/// The first candidate whose key recognises `data` as its probe.
pub fn recognise_probe<'a>(
    data: &[u8],
    candidates: impl IntoIterator<Item = (&'a PublicIdentity, &'a [u8; 32])>,
) -> Option<PublicIdentity> {
    if !looks_like_probe(data) {
        return None;
    }
    let (nonce, tag) = data.split_at(16);
    candidates.into_iter().find_map(|(peer, d)| {
        let want = probe_tag(d, peer, nonce);
        // Not secret-dependent timing worth guarding: the tag is public once sent.
        (want == tag).then_some(*peer)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    fn sample() -> Candidates {
        Candidates {
            list: vec![
                Candidate {
                    kind: CandidateKind::Reflexive,
                    addr: "203.0.113.7:41000".parse().unwrap(),
                },
                Candidate {
                    kind: CandidateKind::Ipv6,
                    addr: "[2001:db8::1]:7450".parse().unwrap(),
                },
            ],
            issued_ms: 1_700_000_000_000,
            flags: FLAG_SYMMETRIC,
            seeking_until_ms: Some(1_700_000_120_000),
        }
    }

    #[test]
    fn both_sides_derive_the_same_keys_per_direction_and_epoch() {
        let (a, b) = (Identity::generate().public(), Identity::generate().public());
        let d = [7u8; 32];
        let ka = record_keys(&d, &a, 5);
        assert_eq!(ka.signing_seed, record_keys(&d, &a, 5).signing_seed);
        assert_ne!(ka.signing_seed, record_keys(&d, &b, 5).signing_seed);
        assert_ne!(ka.signing_seed, record_keys(&d, &a, 6).signing_seed);
        assert_ne!(ka.salt, record_keys(&[8; 32], &a, 5).salt);
    }

    #[test]
    fn records_round_trip_and_have_one_length() {
        let a = Identity::generate().public();
        let keys = record_keys(&[1; 32], &a, 9);
        let c = sample();
        let v = seal_record(&keys, &c).unwrap();
        assert_eq!(v.len(), RECORD_LEN);
        assert_eq!(open_record(&keys, &v).unwrap(), c);
        let empty = Candidates {
            list: vec![],
            issued_ms: 1,
            flags: 0,
            seeking_until_ms: None,
        };
        assert_eq!(seal_record(&keys, &empty).unwrap().len(), RECORD_LEN);
        let full = Candidates {
            list: vec![c.list[1]; MAX_CANDIDATES],
            ..c
        };
        assert_eq!(seal_record(&keys, &full).unwrap().len(), RECORD_LEN);
    }

    #[test]
    fn records_need_the_right_keys_and_resist_tampering() {
        let a = Identity::generate().public();
        let keys = record_keys(&[1; 32], &a, 9);
        let mut v = seal_record(&keys, &sample()).unwrap();
        assert!(open_record(&record_keys(&[2; 32], &a, 9), &v).is_err());
        assert!(open_record(&record_keys(&[1; 32], &a, 10), &v).is_err());
        v[40] ^= 1;
        assert!(open_record(&keys, &v).is_err());
    }

    #[test]
    fn probes_are_recognised_only_with_the_key_and_never_look_like_quic() {
        let (a, b) = (Identity::generate().public(), Identity::generate().public());
        let d = [3u8; 32];
        for _ in 0..64 {
            let p = probe(&d, &a);
            assert!(looks_like_probe(&p));
            assert_eq!(p[0] & 0x40, 0, "QUIC fixed bit clear");
            assert_eq!(recognise_probe(&p, [(&b, &d), (&a, &d)]), Some(a));
            assert_eq!(recognise_probe(&p, [(&a, &[4; 32])]), None);
            assert_eq!(recognise_probe(&p, [(&b, &d)]), None);
        }
    }
}
