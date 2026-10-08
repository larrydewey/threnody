//! Hybrid post-quantum double ratchet with header encryption (spec §3.3,
//! §6.1, §9).
//!
//! The Signal double ratchet (header-encryption variant) with its
//! Diffie-Hellman step replaced by an X-Wing KEM step. Each time a party
//! receives a message from a new chain, it decapsulates to open the
//! receiving chain, then generates a fresh key pair and encapsulates to the
//! peer's new key to open its next sending chain. Steps strictly alternate,
//! so each party only ever needs its newest decapsulation key.
//!
//! Headers (ratchet key, KEM ciphertext, counters) are encrypted under
//! header keys that advance with the root chain, so a path observer or
//! relay cannot read message counters or link ratchet keys.
//! See `docs/appendix-b-ratchet.md`.

use std::collections::{HashMap, VecDeque};

use const_cbor::Decoder;
use rand_core::Rng;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::hybrid::{HybridPublic, HybridSecret};
use crate::crypto::kdf::{self, label};
use crate::crypto::rng::Rng as RngSource;
use crate::error::{Error, Result};
use crate::wire::{self, MsgType};

/// Maximum message keys skipped within one chain.
pub const MAX_SKIP: u32 = 1000;
/// Maximum skipped keys retained overall; oldest are evicted first.
pub const MAX_STORED_SKIPPED: usize = 2000;

type Key = Zeroizing<[u8; 32]>;

#[derive(Clone)]
struct Chain {
    key: Key,
    n: u32,
    /// Header key protecting this chain's headers.
    hk: Key,
}

impl Chain {
    /// Returns the next message key and advances the chain.
    fn next(&mut self) -> Key {
        let mk = Zeroizing::new(kdf::chain(&self.key, 1));
        self.key = Zeroizing::new(kdf::chain(&self.key, 2));
        self.n += 1;
        mk
    }
}

#[derive(Clone)]
struct SendChain {
    chain: Chain,
    /// Our ratchet public key and the ciphertext to the peer's key; repeated
    /// in every header of this chain so any one message can open it.
    pub_bytes: Vec<u8>,
    ct: Vec<u8>,
}

/// Double-ratchet session state.
#[derive(Clone)]
pub struct Ratchet {
    suite: Suite,
    session_id: [u8; 32],
    root: Key,
    own: HybridSecret,
    send: Option<SendChain>,
    next_send_hk: Key,
    prev_send_n: u32,
    recv: Option<Chain>,
    next_recv_hk: Key,
    skipped: HashMap<([u8; 32], u32), Key>,
    skipped_order: VecDeque<([u8; 32], u32)>,
    rng: RngSource,
}

struct Header {
    pub_bytes: Vec<u8>,
    ct: Vec<u8>,
    pn: u32,
    n: u32,
}

/// Serializable ratchet state for persistence across reconnects.
/// Excludes the RNG which is reseeded on restore.
#[derive(Clone)]
pub struct RatchetState {
    pub suite: Suite,
    pub session_id: [u8; 32],
    pub root: [u8; 32],
    pub own: Vec<u8>, // HybridSecret serialized
    pub send: Option<SendChainState>,
    pub next_send_hk: [u8; 32],
    pub prev_send_n: u32,
    pub recv: Option<ChainState>,
    pub next_recv_hk: [u8; 32],
    pub skipped: Vec<SkippedEntry>,
}

#[derive(Clone)]
pub struct ChainState {
    key: [u8; 32],
    n: u32,
    hk: [u8; 32],
}

#[derive(Clone)]
pub struct SendChainState {
    chain: ChainState,
    pub_bytes: Vec<u8>,
    ct: Vec<u8>,
}

#[derive(Clone)]
pub struct SkippedEntry {
    ratchet_pub: [u8; 32],
    n: u32,
    key: [u8; 32],
}

impl RatchetState {
    /// Encodes the ratchet state to CBOR bytes.
    pub fn encode(&self) -> Vec<u8> {
        cbor::to_vec(self.encoded_len(), |e| self.write_to(e)).expect("ratchet state encoding")
    }

    fn encoded_len(&self) -> usize {
        // Rough estimate
        512 + self.skipped.len() * 100
    }

    fn write_to(&self, e: &mut const_cbor::Encoder) -> core::result::Result<(), const_cbor::Error> {
        let map_len = 8 + usize::from(self.send.is_some()) + usize::from(self.recv.is_some());
        e.map_len(map_len)?;
        e.u8(0)?.u8(self.suite as u8)?;
        e.u8(1)?.bytes(&self.session_id)?;
        e.u8(2)?.bytes(&self.root)?;
        e.u8(3)?.bytes(&self.own)?;
        if let Some(s) = &self.send {
            e.u8(4)?;
            e.map_len(3)?;
            e.u8(0)?;
            e.map_len(3)?;
            e.u8(0)?.bytes(&s.chain.key)?;
            e.u8(1)?.u32(s.chain.n)?;
            e.u8(2)?.bytes(&s.chain.hk)?;
            e.u8(1)?.bytes(&s.pub_bytes)?;
            e.u8(2)?.bytes(&s.ct)?;
        }
        e.u8(5)?.bytes(&self.next_send_hk)?;
        e.u8(6)?.u32(self.prev_send_n)?;
        if let Some(r) = &self.recv {
            e.u8(7)?;
            e.map_len(3)?;
            e.u8(0)?.bytes(&r.key)?;
            e.u8(1)?.u32(r.n)?;
            e.u8(2)?.bytes(&r.hk)?;
        }
        e.u8(8)?.bytes(&self.next_recv_hk)?;
        e.u8(9)?;
        e.array_len(self.skipped.len())?;
        for s in &self.skipped {
            e.array_len(3)?;
            e.bytes(&s.ratchet_pub)?;
            e.u32(s.n)?;
            e.bytes(&s.key)?;
        }
        Ok(())
    }

    /// Decodes the ratchet state from CBOR bytes.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(data);
        let mut suite = Suite::ChaCha20Poly1305;
        let mut session_id = [0u8; 32];
        let mut root = [0u8; 32];
        let mut own = Vec::new();
        let mut send = None;
        let mut next_send_hk = [0u8; 32];
        let mut prev_send_n = 0;
        let mut recv = None;
        let mut next_recv_hk = [0u8; 32];
        let mut skipped = Vec::new();

        read_map(&mut dec, |k, d| {
            match k {
                0 => suite = Suite::from_wire(d.u8()?).ok_or(Error::Malformed("bad suite"))?,
                1 => session_id.copy_from_slice(&fixed_bytes::<32>(d)?),
                2 => root.copy_from_slice(&fixed_bytes::<32>(d)?),
                3 => own = d.bytes()?.to_vec(),
                4 => {
                    if d.is_null().map(|v| !v).unwrap_or(false) {
                        read_map(d, |k2, d2| {
                            match k2 {
                                0 => {
                                    read_map(d2, |k3, d3| {
                                        let mut key = [0u8; 32];
                                        let mut n = 0;
                                        let mut hk = [0u8; 32];
                                        match k3 {
                                            0 => key.copy_from_slice(&fixed_bytes::<32>(d3)?),
                                            1 => n = d3.u32()?,
                                            2 => hk.copy_from_slice(&fixed_bytes::<32>(d3)?),
                                            _ => return Ok(false),
                                        }
                                        send = Some(SendChainState {
                                            chain: ChainState { key, n, hk },
                                            pub_bytes: Vec::new(),
                                            ct: Vec::new(),
                                        });
                                        Ok(true)
                                    })?;
                                }
                                1 => send.as_mut().unwrap().pub_bytes = d2.bytes()?.to_vec(),
                                2 => send.as_mut().unwrap().ct = d2.bytes()?.to_vec(),
                                _ => return Ok(false),
                            }
                            Ok(true)
                        })?;
                    }
                }
                5 => next_send_hk.copy_from_slice(&fixed_bytes::<32>(d)?),
                6 => prev_send_n = d.u32()?,
                7 => {
                    if d.is_null().map(|v| !v).unwrap_or(false) {
                        read_map(d, |k2, d2| {
                            let mut key = [0u8; 32];
                            let mut n = 0;
                            let mut hk = [0u8; 32];
                            match k2 {
                                0 => key.copy_from_slice(&fixed_bytes::<32>(d2)?),
                                1 => n = d2.u32()?,
                                2 => hk.copy_from_slice(&fixed_bytes::<32>(d2)?),
                                _ => return Ok(false),
                            }
                            recv = Some(ChainState { key, n, hk });
                            Ok(true)
                        })?;
                    }
                }
                8 => next_recv_hk.copy_from_slice(&fixed_bytes::<32>(d)?),
                9 => {
                    for _ in 0..d.array_len()? {
                        let ratchet_pub = {
                            let mut r = [0u8; 32];
                            r.copy_from_slice(d.bytes()?);
                            r
                        };
                        let n = d.u32()?;
                        let key = {
                            let mut k = [0u8; 32];
                            k.copy_from_slice(d.bytes()?);
                            k
                        };
                        skipped.push(SkippedEntry {
                            ratchet_pub,
                            n,
                            key,
                        });
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;

        Ok(Self {
            suite,
            session_id,
            root,
            own,
            send,
            next_send_hk,
            prev_send_n,
            recv,
            next_recv_hk,
            skipped,
        })
    }
}

impl Ratchet {
    /// Converts the ratchet to a persistable state.
    pub fn to_state(&self) -> RatchetState {
        let mut skipped = Vec::new();
        for ((ratchet_pub, n), key) in &self.skipped {
            skipped.push(SkippedEntry {
                ratchet_pub: *ratchet_pub,
                n: *n,
                key: **key,
            });
        }

        RatchetState {
            suite: self.suite,
            session_id: self.session_id,
            root: *self.root,
            own: self.own.to_bytes().to_vec(),
            send: self.send.as_ref().map(|s| SendChainState {
                chain: ChainState {
                    key: *s.chain.key,
                    n: s.chain.n,
                    hk: *s.chain.hk,
                },
                pub_bytes: s.pub_bytes.clone(),
                ct: s.ct.clone(),
            }),
            next_send_hk: *self.next_send_hk,
            prev_send_n: self.prev_send_n,
            recv: self.recv.as_ref().map(|r| ChainState {
                key: *r.key,
                n: r.n,
                hk: *r.hk,
            }),
            next_recv_hk: *self.next_recv_hk,
            skipped,
        }
    }

    /// Restores the ratchet from a persisted state.
    pub fn from_state(state: RatchetState, rng: RngSource) -> Result<Self> {
        let own = HybridSecret::from_bytes(&state.own)
            .map_err(|_| Error::Malformed("invalid own secret in ratchet state"))?;

        let send = state.send.map(|s| SendChain {
            chain: Chain {
                key: Zeroizing::new(s.chain.key),
                n: s.chain.n,
                hk: Zeroizing::new(s.chain.hk),
            },
            pub_bytes: s.pub_bytes,
            ct: s.ct,
        });

        let recv = state.recv.map(|r| Chain {
            key: Zeroizing::new(r.key),
            n: r.n,
            hk: Zeroizing::new(r.hk),
        });

        let mut skipped = HashMap::new();
        let mut skipped_order = VecDeque::new();
        for s in state.skipped {
            skipped.insert((s.ratchet_pub, s.n), Zeroizing::new(s.key));
            skipped_order.push_back((s.ratchet_pub, s.n));
        }

        Ok(Self {
            suite: state.suite,
            session_id: state.session_id,
            root: Zeroizing::new(state.root),
            own,
            send,
            next_send_hk: Zeroizing::new(state.next_send_hk),
            prev_send_n: state.prev_send_n,
            recv,
            next_recv_hk: Zeroizing::new(state.next_recv_hk),
            skipped,
            skipped_order,
            rng,
        })
    }
}

fn initial_header_keys(root: &[u8; 32]) -> (Key, Key) {
    let okm: Zeroizing<[u8; 64]> = Zeroizing::new(kdf::derive(label::HEADER_KEYS, &[root]));
    let mut a = Zeroizing::new([0u8; 32]);
    let mut b = Zeroizing::new([0u8; 32]);
    a.copy_from_slice(&okm[..32]);
    b.copy_from_slice(&okm[32..]);
    (a, b)
}

impl Ratchet {
    /// Initiator side: knows the responder's first ratchet key and opens a
    /// sending chain immediately.
    pub fn new_initiator(
        suite: Suite,
        session_id: [u8; 32],
        root: &[u8; 32],
        peer: HybridPublic,
        mut rng: RngSource,
    ) -> Result<Self> {
        let (hk_i, hk_r) = initial_header_keys(root);
        let own = HybridSecret::generate_with(&mut rng);
        let mut r = Self {
            suite,
            session_id,
            root: Zeroizing::new(*root),
            own,
            send: None,
            next_send_hk: hk_i,
            prev_send_n: 0,
            recv: None,
            next_recv_hk: hk_r,
            skipped: HashMap::new(),
            skipped_order: VecDeque::new(),
            rng,
        };
        r.open_send_chain(&peer)?;
        Ok(r)
    }

    /// Responder side: holds the secret for the key it sent in HS2 and can
    /// send only after the initiator's first ratchet message arrives.
    pub fn new_responder(
        suite: Suite,
        session_id: [u8; 32],
        root: &[u8; 32],
        own: HybridSecret,
        rng: RngSource,
    ) -> Self {
        let (hk_i, hk_r) = initial_header_keys(root);
        Self {
            suite,
            session_id,
            root: Zeroizing::new(*root),
            own,
            send: None,
            next_send_hk: hk_r,
            prev_send_n: 0,
            recv: None,
            next_recv_hk: hk_i,
            skipped: HashMap::new(),
            skipped_order: VecDeque::new(),
            rng,
        }
    }

    pub fn can_send(&self) -> bool {
        self.send.is_some()
    }

    /// Encrypts `plaintext` into a complete `Ratchet` envelope frame.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let pn = self.prev_send_n;
        let send = self.send.as_mut().ok_or(Error::NotReady)?;
        let n = send.chain.n;
        let mk = send.chain.next();
        let header = encode_header(&send.pub_bytes, &send.ct, pn, n)?;
        let mut nonce = [0u8; 12];
        self.rng.fill_bytes(&mut nonce);
        let enc_header = self
            .suite
            .seal(&send.chain.hk, &nonce, &self.session_id, &header);
        let (key, msg_nonce) = expand(&mk);
        let ad = message_ad(&self.session_id, &nonce, &enc_header);
        let ct = self.suite.seal(&key, &msg_nonce, &ad, plaintext);
        let body = cbor::to_vec(enc_header.len() + ct.len() + 32, |e| {
            e.map_len(3)?;
            e.u8(0)?.bytes(&nonce)?;
            e.u8(1)?.bytes(&enc_header)?;
            e.u8(2)?.bytes(&ct)?;
            Ok(())
        })?;
        wire::encode_envelope(MsgType::Ratchet, &body)
    }

    /// Decrypts a `Ratchet` envelope frame. State is only updated when
    /// authentication succeeds.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let body = wire::expect(frame, MsgType::Ratchet)?;
        let mut dec = Decoder::new(body);
        let (mut nonce, mut enc_header, mut ct) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => nonce = Some(fixed_bytes::<12>(d)?),
                1 => enc_header = Some(d.bytes()?),
                2 => ct = Some(d.bytes()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let nonce = required(nonce, "header nonce")?;
        let enc_header = required(enc_header, "encrypted header")?;
        let ct = required(ct, "ratchet ciphertext")?;
        let ad = message_ad(&self.session_id, &nonce, enc_header);

        if let Some(pt) = self.try_skipped(&nonce, enc_header, &ad, ct)? {
            return Ok(pt);
        }

        let mut next = self.clone();
        let current = next
            .recv
            .as_ref()
            .and_then(|c| next.open_header(&c.hk, &nonce, enc_header));
        let header = match current {
            Some(h) => h,
            None => {
                let h = next
                    .open_header(&next.next_recv_hk, &nonce, enc_header)
                    .ok_or(Error::Decrypt)?;
                next.skip_until(h.pn)?;
                next.step(&h)?;
                h
            }
        };
        next.skip_until(header.n)?;
        let chain = next.recv.as_mut().ok_or(Error::Decrypt)?;
        if header.n < chain.n {
            return Err(Error::Replay);
        }
        let mk = chain.next();
        let (key, msg_nonce) = expand(&mk);
        let pt = next.suite.open(&key, &msg_nonce, &ad, ct)?;
        *self = next;
        Ok(pt)
    }

    fn open_header(&self, hk: &[u8; 32], nonce: &[u8; 12], enc: &[u8]) -> Option<Header> {
        let pt = self.suite.open(hk, nonce, &self.session_id, enc).ok()?;
        decode_header(&pt).ok()
    }

    /// Tries keys stored for skipped messages. Returns `Ok(None)` when the
    /// frame does not belong to a skipped slot.
    fn try_skipped(
        &mut self,
        nonce: &[u8; 12],
        enc_header: &[u8],
        ad: &[u8],
        ct: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let mut hks: Vec<[u8; 32]> = self.skipped_order.iter().map(|(hk, _)| *hk).collect();
        hks.dedup();
        for hk in hks {
            let Some(h) = self.open_header(&hk, nonce, enc_header) else {
                continue;
            };
            let Some(mk) = self.skipped.get(&(hk, h.n)) else {
                return Ok(None);
            };
            let (key, msg_nonce) = expand(mk);
            let pt = self.suite.open(&key, &msg_nonce, ad, ct)?;
            self.skipped.remove(&(hk, h.n));
            self.skipped_order.retain(|k| k != &(hk, h.n));
            return Ok(Some(pt));
        }
        Ok(None)
    }

    /// Receiving-side KEM ratchet step followed by a fresh sending chain.
    fn step(&mut self, h: &Header) -> Result<()> {
        let peer = HybridPublic::from_bytes(&h.pub_bytes)?;
        let ss = self.own.decapsulate(&h.ct)?;
        let (ck, nhk) = self.root_step(&ss);
        let hk = std::mem::replace(&mut self.next_recv_hk, nhk);
        self.recv = Some(Chain { key: ck, n: 0, hk });
        self.prev_send_n = self.send.as_ref().map_or(0, |s| s.chain.n);
        self.own = HybridSecret::generate_with(&mut self.rng);
        self.open_send_chain(&peer)
    }

    fn open_send_chain(&mut self, peer: &HybridPublic) -> Result<()> {
        let (ct, ss) = peer.encapsulate_with(&mut self.rng)?;
        let (ck, nhk) = self.root_step(&ss);
        let hk = std::mem::replace(&mut self.next_send_hk, nhk);
        self.send = Some(SendChain {
            chain: Chain { key: ck, n: 0, hk },
            pub_bytes: self.own.public().as_bytes().to_vec(),
            ct,
        });
        Ok(())
    }

    /// `root, chain, next_header = KDF(root, ss)`.
    fn root_step(&mut self, ss: &[u8; 32]) -> (Key, Key) {
        let okm: Zeroizing<[u8; 96]> =
            Zeroizing::new(kdf::derive(label::RATCHET_ROOT, &[&self.root[..], ss]));
        let mut root = Zeroizing::new([0u8; 32]);
        let mut ck = Zeroizing::new([0u8; 32]);
        let mut nhk = Zeroizing::new([0u8; 32]);
        root.copy_from_slice(&okm[..32]);
        ck.copy_from_slice(&okm[32..64]);
        nhk.copy_from_slice(&okm[64..]);
        self.root = root;
        (ck, nhk)
    }

    /// Stores message keys of the current receiving chain up to `until`.
    fn skip_until(&mut self, until: u32) -> Result<()> {
        let Some(chain) = self.recv.as_mut() else {
            return Ok(());
        };
        if until.saturating_sub(chain.n) > MAX_SKIP {
            return Err(Error::TooManySkipped);
        }
        while chain.n < until {
            let n = chain.n;
            let mk = chain.next();
            let slot = (*chain.hk, n);
            self.skipped.insert(slot, mk);
            self.skipped_order.push_back(slot);
            if self.skipped_order.len() > MAX_STORED_SKIPPED
                && let Some(old) = self.skipped_order.pop_front()
            {
                self.skipped.remove(&old);
            }
        }
        Ok(())
    }
}

fn message_ad(session_id: &[u8; 32], nonce: &[u8; 12], enc_header: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(44 + enc_header.len());
    v.extend_from_slice(session_id);
    v.extend_from_slice(nonce);
    v.extend_from_slice(enc_header);
    v
}

fn expand(mk: &[u8; 32]) -> (Zeroizing<[u8; 32]>, [u8; 12]) {
    let okm: Zeroizing<[u8; 44]> = Zeroizing::new(kdf::derive(label::MESSAGE_KEY, &[mk]));
    let mut key = Zeroizing::new([0u8; 32]);
    let mut nonce = [0u8; 12];
    key.copy_from_slice(&okm[..32]);
    nonce.copy_from_slice(&okm[32..]);
    (key, nonce)
}

fn encode_header(pub_bytes: &[u8], ct: &[u8], pn: u32, n: u32) -> Result<Vec<u8>> {
    cbor::to_vec(pub_bytes.len() + ct.len() + 32, |e| {
        e.map_len(4)?;
        e.u8(0)?.bytes(pub_bytes)?;
        e.u8(1)?.bytes(ct)?;
        e.u8(2)?.u32(pn)?;
        e.u8(3)?.u32(n)?;
        Ok(())
    })
}

fn decode_header(b: &[u8]) -> Result<Header> {
    let mut dec = Decoder::new(b);
    let (mut p, mut c, mut pn, mut n) = (None, None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => p = Some(d.bytes()?.to_vec()),
            1 => c = Some(d.bytes()?.to_vec()),
            2 => pn = Some(d.u32()?),
            3 => n = Some(d.u32()?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    Ok(Header {
        pub_bytes: required(p, "ratchet key")?,
        ct: required(c, "ratchet kem ciphertext")?,
        pn: required(pn, "previous chain length")?,
        n: required(n, "message number")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair_with_root(root: [u8; 32]) -> (Ratchet, Ratchet) {
        let sid = [3u8; 32];
        let b_secret = HybridSecret::generate();
        let b_pub = b_secret.public().clone();
        let a = Ratchet::new_initiator(Suite::ChaCha20Poly1305, sid, &root, b_pub, RngSource::Os)
            .unwrap();
        let b =
            Ratchet::new_responder(Suite::ChaCha20Poly1305, sid, &root, b_secret, RngSource::Os);
        (a, b)
    }

    fn pair() -> (Ratchet, Ratchet) {
        pair_with_root([9u8; 32])
    }

    #[test]
    fn ping_pong_with_steps() {
        let (mut a, mut b) = pair();
        assert!(!b.can_send());
        assert!(matches!(b.encrypt(b"x"), Err(Error::NotReady)));
        for round in 0..5u8 {
            let m = a.encrypt(&[round]).unwrap();
            assert_eq!(b.decrypt(&m).unwrap(), [round]);
            let m = b.encrypt(&[round, 1]).unwrap();
            assert_eq!(a.decrypt(&m).unwrap(), [round, 1]);
        }
    }

    #[test]
    fn out_of_order_and_across_steps() {
        let (mut a, mut b) = pair();
        let a0 = a.encrypt(b"a0").unwrap();
        let a1 = a.encrypt(b"a1").unwrap();
        let a2 = a.encrypt(b"a2").unwrap();
        assert_eq!(b.decrypt(&a2).unwrap(), b"a2");
        let b0 = b.encrypt(b"b0").unwrap();
        assert_eq!(a.decrypt(&b0).unwrap(), b"b0");
        let a3 = a.encrypt(b"a3").unwrap(); // new chain after step
        let b1 = b.encrypt(b"b1").unwrap();
        assert_eq!(b.decrypt(&a3).unwrap(), b"a3");
        // late arrivals from the previous chains still open
        assert_eq!(b.decrypt(&a0).unwrap(), b"a0");
        assert_eq!(b.decrypt(&a1).unwrap(), b"a1");
        assert_eq!(a.decrypt(&b1).unwrap(), b"b1");
        assert!(b.skipped.is_empty());
    }

    #[test]
    fn replay_and_tamper_rejected_without_state_damage() {
        let (mut a, mut b) = pair();
        let m = a.encrypt(b"hi").unwrap();
        let mut bad = m.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(b.decrypt(&bad).is_err());
        assert_eq!(b.decrypt(&m).unwrap(), b"hi");
        assert!(b.decrypt(&m).is_err(), "replay accepted");
        let m2 = a.encrypt(b"again").unwrap();
        assert_eq!(b.decrypt(&m2).unwrap(), b"again");
        // replay of a skipped-then-delivered message
        let s0 = a.encrypt(b"s0").unwrap();
        let s1 = a.encrypt(b"s1").unwrap();
        b.decrypt(&s1).unwrap();
        b.decrypt(&s0).unwrap();
        assert!(b.decrypt(&s0).is_err(), "skipped-key replay accepted");
    }

    #[test]
    fn skip_limit_enforced() {
        let (mut a, mut b) = pair();
        let mut last = Vec::new();
        for _ in 0..=MAX_SKIP + 1 {
            last = a.encrypt(b"x").unwrap();
        }
        assert!(matches!(b.decrypt(&last), Err(Error::TooManySkipped)));
    }

    #[test]
    fn foreign_session_rejected() {
        let (mut a, _) = pair();
        let (_, mut other_b) = pair();
        let m = a.encrypt(b"x").unwrap();
        assert!(other_b.decrypt(&m).is_err());
        let (_, mut other_root) = pair_with_root([1u8; 32]);
        assert!(other_root.decrypt(&m).is_err());
    }

    #[test]
    fn headers_are_opaque() {
        let (mut a, _) = pair();
        let m0 = a.encrypt(b"x").unwrap();
        let m1 = a.encrypt(b"x").unwrap();
        let pk = a.own.public().as_bytes();
        for m in [&m0, &m1] {
            assert!(
                !m.windows(32).any(|w| w == &pk[..32]),
                "ratchet public key visible on the wire"
            );
        }
        assert_eq!(m0.len(), m1.len());
    }
}
