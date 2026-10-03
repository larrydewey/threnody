//! Three-message mutually authenticated hybrid post-quantum handshake.
//!
//! SIGMA-I shape with an X-Wing hybrid KEM in place of Diffie-Hellman.
//! Full description and rationale: `docs/appendix-a-handshake.md`.
//!
//! ```text
//! I -> R  HS1 { 0: suites [uint], 1: e_I (hybrid pk) }
//! R -> I  HS2 { 0: suite uint, 1: ct (encap to e_I), 2: AEAD_kR({0: id_R, 1: sig_R, 2: rk_R}) }
//! I -> R  HS3 { 0: AEAD_kI({0: id_I, 1: sig_I}) }
//! ```
//!
//! The API is sans-IO: each step consumes and produces raw frames.

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::hybrid::{HybridPublic, HybridSecret};
use crate::crypto::kdf::{self, label};
use crate::crypto::rng::Rng;
use crate::error::{Error, Result};
use crate::identity::{Identity, PublicIdentity};
use crate::ratchet::Ratchet;
use crate::wire::{self, MsgType};

const PROTOCOL_ID: &[u8] = b"threnody/1";
const ZERO_NONCE: [u8; 12] = [0; 12];

/// Running transcript hash. Every absorbed field is labelled and
/// length-prefixed so the hash is independent of CBOR encoding choices.
#[derive(Clone)]
struct Transcript(blake3::Hasher);

impl Transcript {
    fn new() -> Self {
        let mut t = Self(blake3::Hasher::new_derive_key(label::TRANSCRIPT));
        t.absorb(b"protocol", PROTOCOL_ID);
        t
    }

    fn absorb(&mut self, field: &[u8], data: &[u8]) {
        for part in [field, data] {
            self.0.update(&(part.len() as u64).to_le_bytes());
            self.0.update(part);
        }
    }

    fn hash(&self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

/// Handshake outcome: an authenticated peer and a ready ratchet.
pub struct Established {
    pub peer: PublicIdentity,
    pub suite: Suite,
    /// Final transcript hash; unique per session, bound into every
    /// ratchet message as associated data.
    pub session_id: [u8; 32],
    /// Secret for deriving keys outside the ratchet (e.g. WireGuard PSKs).
    pub exporter: Zeroizing<[u8; 32]>,
    pub ratchet: Ratchet,
}

/// Initiator state after sending HS1.
pub struct Initiator<'a> {
    identity: &'a Identity,
    eph: HybridSecret,
    transcript: Transcript,
    rng: Rng,
}

impl<'a> Initiator<'a> {
    /// Starts a handshake. Returns the state and the HS1 frame.
    pub fn start(identity: &'a Identity) -> Result<(Self, Vec<u8>)> {
        Self::start_with(identity, Rng::Os)
    }

    /// [`Initiator::start`] with an explicit randomness source.
    pub fn start_with(identity: &'a Identity, mut rng: Rng) -> Result<(Self, Vec<u8>)> {
        let eph = HybridSecret::generate_with(&mut rng);
        let body = cbor::to_vec(1300, |e| {
            e.map_len(2)?;
            e.u8(0)?.array_len(Suite::SUPPORTED.len())?;
            for s in Suite::SUPPORTED {
                e.u8(s as u8)?;
            }
            e.u8(1)?.bytes(eph.public().as_bytes())?;
            Ok(())
        })?;
        let mut transcript = Transcript::new();
        let offered: Vec<u8> = Suite::SUPPORTED.iter().map(|&s| s as u8).collect();
        transcript.absorb(b"offer", &offered);
        transcript.absorb(b"e_i", eph.public().as_bytes());
        let frame = wire::encode_envelope(MsgType::HandshakeInit, &body)?;
        Ok((
            Self {
                identity,
                eph,
                transcript,
                rng,
            },
            frame,
        ))
    }

    /// Consumes HS2, authenticates the responder, and returns HS3 plus the
    /// established session. The caller decides whether `peer` is acceptable
    /// (TOFU, pinned fingerprint, approved contact) before using it.
    pub fn finish(mut self, frame: &[u8]) -> Result<(Vec<u8>, Established)> {
        let body = wire::expect(frame, MsgType::HandshakeResp)?;
        let mut dec = Decoder::new(body);
        let (mut suite, mut ct, mut sealed) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => suite = Some(d.u8()?),
                1 => ct = Some(d.bytes()?),
                2 => sealed = Some(d.bytes()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let suite_wire = required(suite, "suite")?;
        let suite = Suite::from_wire(suite_wire).ok_or(Error::NoCommonSuite)?;
        let ct = required(ct, "kem ciphertext")?;
        let sealed = required(sealed, "responder payload")?;

        let ss = self.eph.decapsulate(ct)?;
        self.transcript.absorb(b"suite", &[suite_wire]);
        self.transcript.absorb(b"kem_ct", ct);
        let h2 = self.transcript.hash();
        let keys = traffic_keys(&ss, &h2);

        let payload = suite.open(&keys.responder, &ZERO_NONCE, &h2, sealed)?;
        let (peer, sig, ratchet_pub) = decode_responder_payload(&payload)?;
        let ratchet_pub = HybridPublic::from_bytes(&required(ratchet_pub, "ratchet key")?)?;
        peer.verify(
            &sig_input(
                label::SIG_RESPONDER,
                &h2,
                peer.as_bytes(),
                ratchet_pub.as_bytes(),
            ),
            &sig,
        )?;
        self.transcript.absorb(b"sealed_r", sealed);
        let h3 = self.transcript.hash();

        let me = self.identity.public();
        let my_sig = self
            .identity
            .sign(&sig_input(label::SIG_INITIATOR, &h3, me.as_bytes(), &[]));
        let inner = cbor::to_vec(128, |e| {
            e.map_len(2)?;
            e.u8(0)?.bytes(me.as_bytes())?;
            e.u8(1)?.bytes(&my_sig)?;
            Ok(())
        })?;
        let sealed_i = suite.seal(&keys.initiator, &ZERO_NONCE, &h3, &inner);
        let body = cbor::to_vec(sealed_i.len() + 16, |e| {
            e.map_len(1)?;
            e.u8(0)?.bytes(&sealed_i)?;
            Ok(())
        })?;
        self.transcript.absorb(b"sealed_i", &sealed_i);
        let session_id = self.transcript.hash();
        let root = session_root(&ss, &session_id);
        let ratchet = Ratchet::new_initiator(suite, session_id, &root, ratchet_pub, self.rng)?;
        let frame = wire::encode_envelope(MsgType::HandshakeFinish, &body)?;
        Ok((
            frame,
            Established {
                peer,
                suite,
                session_id,
                exporter: exporter_secret(&ss, &session_id),
                ratchet,
            },
        ))
    }
}

/// Responder state after sending HS2.
pub struct Responder {
    suite: Suite,
    ss: Zeroizing<[u8; 32]>,
    transcript: Transcript,
    initiator_key: [u8; 32],
    ratchet_secret: HybridSecret,
    rng: Rng,
}

impl Responder {
    /// Consumes HS1 and returns the state plus the HS2 frame.
    pub fn respond(identity: &Identity, frame: &[u8]) -> Result<(Self, Vec<u8>)> {
        Self::respond_with(identity, frame, Rng::Os)
    }

    /// [`Responder::respond`] with an explicit randomness source.
    pub fn respond_with(
        identity: &Identity,
        frame: &[u8],
        mut rng: Rng,
    ) -> Result<(Self, Vec<u8>)> {
        let body = wire::expect(frame, MsgType::HandshakeInit)?;
        let mut dec = Decoder::new(body);
        let (mut offered, mut e_i) = (None::<Vec<u8>>, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => {
                    let n = d.array_len()?;
                    if n > 16 {
                        return Err(Error::Malformed("too many suites"));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(d.u8()?);
                    }
                    offered = Some(v);
                }
                1 => e_i = Some(d.bytes()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let offered = required(offered, "suite offer")?;
        let e_i_bytes = required(e_i, "initiator ephemeral")?;
        let e_i = HybridPublic::from_bytes(e_i_bytes)?;
        // Our preference order wins; the offer is signed into the transcript
        // so a downgrade by an active attacker fails authentication.
        let suite = Suite::SUPPORTED
            .into_iter()
            .find(|s| offered.contains(&(*s as u8)))
            .ok_or(Error::NoCommonSuite)?;

        let mut transcript = Transcript::new();
        transcript.absorb(b"offer", &offered);
        transcript.absorb(b"e_i", e_i_bytes);
        let (ct, ss) = e_i.encapsulate_with(&mut rng)?;
        transcript.absorb(b"suite", &[suite as u8]);
        transcript.absorb(b"kem_ct", &ct);
        let h2 = transcript.hash();
        let keys = traffic_keys(&ss, &h2);

        let ratchet_secret = HybridSecret::generate_with(&mut rng);
        let me = identity.public();
        let sig = identity.sign(&sig_input(
            label::SIG_RESPONDER,
            &h2,
            me.as_bytes(),
            ratchet_secret.public().as_bytes(),
        ));
        let inner = cbor::to_vec(1400, |e| {
            e.map_len(3)?;
            e.u8(0)?.bytes(me.as_bytes())?;
            e.u8(1)?.bytes(&sig)?;
            e.u8(2)?.bytes(ratchet_secret.public().as_bytes())?;
            Ok(())
        })?;
        let sealed = suite.seal(&keys.responder, &ZERO_NONCE, &h2, &inner);
        transcript.absorb(b"sealed_r", &sealed);
        let body = cbor::to_vec(ct.len() + sealed.len() + 32, |e| {
            e.map_len(3)?;
            e.u8(0)?.u8(suite as u8)?;
            e.u8(1)?.bytes(&ct)?;
            e.u8(2)?.bytes(&sealed)?;
            Ok(())
        })?;
        let frame = wire::encode_envelope(MsgType::HandshakeResp, &body)?;
        let state = Self {
            suite,
            ss,
            transcript,
            initiator_key: keys.initiator,
            ratchet_secret,
            rng,
        };
        Ok((state, frame))
    }

    /// Consumes HS3 and authenticates the initiator.
    pub fn finish(mut self, frame: &[u8]) -> Result<Established> {
        let body = wire::expect(frame, MsgType::HandshakeFinish)?;
        let mut dec = Decoder::new(body);
        let mut sealed = None;
        read_map(&mut dec, |k, d| {
            if k == 0 {
                sealed = Some(d.bytes()?);
                return Ok(true);
            }
            Ok(false)
        })?;
        finish(&dec)?;
        let sealed = required(sealed, "initiator payload")?;
        let h3 = self.transcript.hash();
        let payload = self
            .suite
            .open(&self.initiator_key, &ZERO_NONCE, &h3, sealed)?;
        let (peer, sig, extra) = decode_responder_payload(&payload)?;
        if extra.is_some() {
            return Err(Error::Malformed("unexpected ratchet key from initiator"));
        }
        peer.verify(
            &sig_input(label::SIG_INITIATOR, &h3, peer.as_bytes(), &[]),
            &sig,
        )?;
        self.transcript.absorb(b"sealed_i", sealed);
        let session_id = self.transcript.hash();
        let root = session_root(&self.ss, &session_id);
        let ratchet =
            Ratchet::new_responder(self.suite, session_id, &root, self.ratchet_secret, self.rng);
        Ok(Established {
            peer,
            suite: self.suite,
            session_id,
            exporter: exporter_secret(&self.ss, &session_id),
            ratchet,
        })
    }
}

struct TrafficKeys {
    responder: [u8; 32],
    initiator: [u8; 32],
}

impl Drop for TrafficKeys {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.responder.zeroize();
        self.initiator.zeroize();
    }
}

fn traffic_keys(ss: &[u8; 32], h: &[u8; 32]) -> TrafficKeys {
    let okm: Zeroizing<[u8; 64]> = Zeroizing::new(kdf::derive(label::HANDSHAKE_KEYS, &[ss, h]));
    let mut k = TrafficKeys {
        responder: [0; 32],
        initiator: [0; 32],
    };
    k.responder.copy_from_slice(&okm[..32]);
    k.initiator.copy_from_slice(&okm[32..]);
    k
}

fn exporter_secret(ss: &[u8; 32], session_id: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(kdf::derive(label::EXPORTER, &[ss, session_id]))
}

fn session_root(ss: &[u8; 32], session_id: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(kdf::derive(label::ROOT, &[ss, session_id]))
}

/// The signed message: `label || len || transcript || len || id || len || extra`.
fn sig_input(label: &str, h: &[u8; 32], id: &[u8; 32], extra: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(label.len() + 32 + 32 + extra.len() + 32);
    for part in [label.as_bytes(), h, id, extra] {
        m.extend_from_slice(&(part.len() as u64).to_le_bytes());
        m.extend_from_slice(part);
    }
    m
}

type AuthPayload = (PublicIdentity, [u8; 64], Option<Vec<u8>>);

/// Decodes `{0: id, 1: sig, ?2: ratchet_pub}` (shared by both directions).
fn decode_responder_payload(payload: &[u8]) -> Result<AuthPayload> {
    let mut dec = Decoder::new(payload);
    let (mut id, mut sig, mut rk) = (None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => id = Some(fixed_bytes::<32>(d)?),
            1 => sig = Some(fixed_bytes::<64>(d)?),
            2 => rk = Some(d.bytes()?.to_vec()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    let id = PublicIdentity::from_bytes(&required(id, "identity")?)?;
    Ok((id, required(sig, "signature")?, rk))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> (Identity, Identity, Established, Established) {
        let a = Identity::generate();
        let b = Identity::generate();
        let (ini, m1) = Initiator::start(&a).unwrap();
        let (resp, m2) = Responder::respond(&b, &m1).unwrap();
        let (m3, ea) = ini.finish(&m2).unwrap();
        let eb = resp.finish(&m3).unwrap();
        (a, b, ea, eb)
    }

    #[test]
    fn handshake_authenticates_both_sides() {
        let (a, b, ea, eb) = run();
        assert_eq!(ea.peer, b.public());
        assert_eq!(eb.peer, a.public());
        assert_eq!(ea.session_id, eb.session_id);
        assert_eq!(*ea.exporter, *eb.exporter);
        assert_ne!(*ea.exporter, ea.session_id);
        assert_eq!(ea.suite, Suite::ChaCha20Poly1305);
    }

    #[test]
    fn tampering_any_handshake_byte_fails() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (_, m1) = Initiator::start(&a).unwrap();
        let (_, m2) = Responder::respond(&b, &m1).unwrap();
        // Flip a byte inside the responder's sealed payload region.
        for pos in [m2.len() - 5, m2.len() - 100, m2.len() / 2] {
            let (ini, m1) = Initiator::start(&a).unwrap();
            let (_, mut m2) = Responder::respond(&b, &m1).unwrap();
            m2[pos] ^= 0x40;
            assert!(ini.finish(&m2).is_err(), "tamper at {pos} accepted");
        }
        let (ini, m1) = Initiator::start(&a).unwrap();
        let (resp, m2) = Responder::respond(&b, &m1).unwrap();
        let (mut m3, _) = ini.finish(&m2).unwrap();
        let last = m3.len() - 1;
        m3[last] ^= 1;
        assert!(resp.finish(&m3).is_err());
    }

    #[test]
    fn hs3_from_another_session_is_rejected() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (ini1, m1a) = Initiator::start(&a).unwrap();
        let (_resp1, m2a) = Responder::respond(&b, &m1a).unwrap();
        let (m3a, _) = ini1.finish(&m2a).unwrap();
        let (_ini2, m1b) = Initiator::start(&a).unwrap();
        let (resp2, _m2b) = Responder::respond(&b, &m1b).unwrap();
        assert!(resp2.finish(&m3a).is_err());
    }

    #[test]
    fn suite_negotiation_falls_back_to_aes() {
        let a = Identity::generate();
        let b = Identity::generate();
        let (ini, _) = Initiator::start(&a).unwrap();
        // Hand-build an HS1 offering only AES-256-GCM.
        let body = cbor::to_vec(1300, |e| {
            e.map_len(2)?;
            e.u8(0)?.array_len(1)?.u8(Suite::Aes256Gcm as u8)?;
            e.u8(1)?.bytes(ini.eph.public().as_bytes())?;
            Ok(())
        })
        .unwrap();
        let m1 = wire::encode_envelope(MsgType::HandshakeInit, &body).unwrap();
        let (resp, m2) = Responder::respond(&b, &m1).unwrap();
        assert_eq!(resp.suite, Suite::Aes256Gcm);
        // The initiator's transcript recorded both suites, so it must refuse.
        assert!(ini.finish(&m2).is_err());
    }
}
