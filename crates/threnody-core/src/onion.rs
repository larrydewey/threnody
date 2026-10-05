//! Onion circuit cryptography (`docs/appendix-i-onion.md`, hop handshake
//! proved in `proofs/onion.spthy`).
//!
//! Sans-IO: the hop handshake for both sides, fixed-size cells with one
//! ChaCha20 layer per hop and per direction, and keyed "recognized"
//! digests telling a hop (or the client) that a cell is addressed to it.

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use zeroize::Zeroizing;

use crate::crypto::hybrid::{HybridPublic, HybridSecret};
use crate::crypto::kdf::{self, label};
use crate::error::{Error, Result};
use crate::identity::{Fingerprint, Identity, PublicIdentity};

/// Every cell body is exactly this long.
pub const CELL_LEN: usize = 2048;
const HEADER_LEN: usize = 2 + 4 + 1 + 2;
/// Largest payload one cell carries.
pub const MAX_DATA: usize = CELL_LEN - HEADER_LEN;

pub type Cell = Box<[u8; CELL_LEN]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cmd {
    Extend = 1,
    Extended = 2,
    ExtendFailed = 3,
    Begin = 4,
    Data = 5,
    End = 6,
    /// Origin to last hop: leave a sealed message in its mailbox.
    /// `to (32) || total_len (u32 BE) || first bytes`, continued in `Data`.
    Deposit = 7,
    /// Last hop to origin: `status (1)`, as in a mailbox receipt.
    Deposited = 8,
}

impl Cmd {
    fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Extend,
            2 => Self::Extended,
            3 => Self::ExtendFailed,
            4 => Self::Begin,
            5 => Self::Data,
            6 => Self::End,
            7 => Self::Deposit,
            8 => Self::Deposited,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub cmd: Cmd,
    pub data: Vec<u8>,
}

impl Payload {
    pub fn new(cmd: Cmd, data: Vec<u8>) -> Self {
        Self { cmd, data }
    }
}

/// Per-hop keys and cell counters, one per direction.
pub struct HopKeys {
    kf: Zeroizing<[u8; 32]>,
    kb: Zeroizing<[u8; 32]>,
    df: Zeroizing<[u8; 32]>,
    db: Zeroizing<[u8; 32]>,
    nf: u64,
    nb: u64,
}

impl HopKeys {
    fn derive(ss: &[u8; 32], e_pub: &[u8], ct: &[u8], id: &PublicIdentity) -> Self {
        let okm: Zeroizing<[u8; 128]> = Zeroizing::new(kdf::derive(
            label::ONION_LAYER_KEYS,
            &[ss, e_pub, ct, id.as_bytes()],
        ));
        let part = |i: usize| {
            let mut k = Zeroizing::new([0u8; 32]);
            k.copy_from_slice(&okm[i * 32..(i + 1) * 32]);
            k
        };
        Self {
            kf: part(0),
            kb: part(1),
            df: part(2),
            db: part(3),
            nf: 0,
            nb: 0,
        }
    }

    // ---- relay side ----

    /// Removes this hop's forward layer. Returns the payload when the cell
    /// is addressed to this hop; otherwise `cell` is ready to forward.
    pub fn relay_forward(&mut self, cell: &mut Cell) -> Option<Payload> {
        let n = self.nf;
        self.nf += 1;
        xor_layer(&self.kf, n, cell);
        recognize(&self.df, n, cell)
    }

    /// Adds this hop's backward layer to a cell coming from downstream.
    pub fn relay_backward(&mut self, cell: &mut Cell) {
        let n = self.nb;
        self.nb += 1;
        xor_layer(&self.kb, n, cell);
    }

    /// Builds a backward cell originating at this hop.
    pub fn relay_originate(&mut self, payload: &Payload) -> Result<Cell> {
        let n = self.nb;
        let mut cell = plaintext(payload, &self.db, n)?;
        self.relay_backward(&mut cell);
        Ok(cell)
    }
}

// ---------------------------------------------------------- hop handshake

/// Client state for one pending CREATE / EXTEND.
pub struct CreateState {
    e: HybridSecret,
    to: Fingerprint,
}

impl CreateState {
    /// Starts a handshake with the hop whose fingerprint is `to`. Returns
    /// the state and `e_pub` to send.
    pub fn new(to: Fingerprint) -> (Self, Vec<u8>) {
        let e = HybridSecret::generate();
        let e_pub = e.public().as_bytes().to_vec();
        (Self { e, to }, e_pub)
    }

    /// Verifies CREATED and derives the hop's keys.
    pub fn finish(self, id: &[u8; 32], ct: &[u8], sig: &[u8; 64]) -> Result<HopKeys> {
        let id = PublicIdentity::from_bytes(id)?;
        if id.fingerprint() != self.to {
            return Err(Error::BadSignature);
        }
        let e_pub = self.e.public().as_bytes();
        id.verify(&created_sig_input(e_pub, ct, &id), sig)?;
        let ss = self.e.decapsulate(ct)?;
        Ok(HopKeys::derive(&ss, e_pub, ct, &id))
    }
}

/// Relay side of the handshake: encapsulate to `e_pub` and sign.
/// Returns the keys plus CREATED's `ct` and `sig`.
pub fn respond(identity: &Identity, e_pub: &[u8]) -> Result<(HopKeys, Vec<u8>, [u8; 64])> {
    let epk = HybridPublic::from_bytes(e_pub)?;
    let (ct, ss) = epk.encapsulate()?;
    let me = identity.public();
    let sig = identity.sign(&created_sig_input(e_pub, &ct, &me));
    Ok((HopKeys::derive(&ss, e_pub, &ct, &me), ct, sig))
}

fn created_sig_input(e_pub: &[u8], ct: &[u8], id: &PublicIdentity) -> Vec<u8> {
    let mut v = Vec::new();
    for p in [
        label::SIG_ONION_CREATED.as_bytes(),
        blake3::hash(e_pub).as_bytes(),
        blake3::hash(ct).as_bytes(),
        id.as_bytes(),
    ] {
        v.extend_from_slice(&(p.len() as u64).to_le_bytes());
        v.extend_from_slice(p);
    }
    v
}

// ------------------------------------------------------------ client side

/// The client's view of a circuit: keys for every hop, nearest first.
#[derive(Default)]
pub struct OnionPath {
    hops: Vec<HopKeys>,
}

impl OnionPath {
    pub fn push(&mut self, hop: HopKeys) {
        self.hops.push(hop);
    }

    pub fn len(&self) -> usize {
        self.hops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hops.is_empty()
    }

    /// Builds a forward cell addressed to hop `target` (0 = nearest).
    pub fn wrap(&mut self, target: usize, payload: &Payload) -> Result<Cell> {
        let hop = self
            .hops
            .get(target)
            .ok_or(Error::Malformed("no such hop"))?;
        let mut cell = plaintext(payload, &hop.df, hop.nf)?;
        for hop in self.hops[..=target].iter_mut().rev() {
            let n = hop.nf;
            hop.nf += 1;
            xor_layer(&hop.kf, n, &mut cell);
        }
        Ok(cell)
    }

    /// Peels a backward cell; returns which hop sent it and its payload.
    pub fn unwrap(&mut self, mut cell: Cell) -> Result<(usize, Payload)> {
        for (i, hop) in self.hops.iter_mut().enumerate() {
            let n = hop.nb;
            hop.nb += 1;
            xor_layer(&hop.kb, n, &mut cell);
            if let Some(p) = recognize(&hop.db, n, &cell) {
                return Ok((i, p));
            }
        }
        Err(Error::Decrypt)
    }
}

// ------------------------------------------------------------------ cells

fn xor_layer(key: &[u8; 32], n: u64, cell: &mut [u8; CELL_LEN]) {
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&n.to_le_bytes());
    ChaCha20::new(key.into(), &nonce.into()).apply_keystream(cell);
}

fn digest(key: &[u8; 32], n: u64, cell: &[u8; CELL_LEN]) -> [u8; 4] {
    let mut h = blake3::Hasher::new_keyed(key);
    h.update(&n.to_le_bytes());
    h.update(&cell[..2]);
    h.update(&[0; 4]);
    h.update(&cell[6..]);
    let mut d = [0u8; 4];
    d.copy_from_slice(&h.finalize().as_bytes()[..4]);
    d
}

fn plaintext(p: &Payload, dkey: &[u8; 32], n: u64) -> Result<Cell> {
    if p.data.len() > MAX_DATA {
        return Err(Error::Malformed("cell payload too large"));
    }
    let mut cell: Cell = Box::new([0u8; CELL_LEN]);
    cell[6] = p.cmd as u8;
    cell[7..9].copy_from_slice(&(p.data.len() as u16).to_be_bytes());
    cell[HEADER_LEN..HEADER_LEN + p.data.len()].copy_from_slice(&p.data);
    let d = digest(dkey, n, &cell);
    cell[2..6].copy_from_slice(&d);
    Ok(cell)
}

fn recognize(dkey: &[u8; 32], n: u64, cell: &[u8; CELL_LEN]) -> Option<Payload> {
    if cell[0] != 0 || cell[1] != 0 || cell[2..6] != digest(dkey, n, cell) {
        return None;
    }
    let cmd = Cmd::from_wire(cell[6])?;
    let len = u16::from_be_bytes([cell[7], cell[8]]) as usize;
    (len <= MAX_DATA).then(|| Payload::new(cmd, cell[HEADER_LEN..HEADER_LEN + len].to_vec()))
}

/// Parses a cell body received from the network.
pub fn cell_from(bytes: &[u8]) -> Result<Cell> {
    let arr: [u8; CELL_LEN] = bytes
        .try_into()
        .map_err(|_| Error::Malformed("cell length"))?;
    Ok(Box::new(arr))
}

/// What an EXTEND cell asks for (Appendix I; the optional fields are
/// Appendix P's):
///
/// ```text
/// to (20) || e_pub (1216) [ || flags (1) [|| u16 len || addr] [|| u16 len || token] ]
/// flags: 1 = addr present, 2 = token present
/// ```
///
/// `addr` lets a volunteer relay dial the next hop by address (it need not
/// be a contact); `token` pays the next hop if it is a volunteer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtendReq {
    pub to: Fingerprint,
    pub e_pub: Vec<u8>,
    pub addr: Option<String>,
    pub token: Option<Vec<u8>>,
}

const EXTEND_ADDR: u8 = 1;
const EXTEND_TOKEN: u8 = 2;

impl ExtendReq {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = self.to.0.to_vec();
        out.extend_from_slice(&self.e_pub);
        if self.addr.is_none() && self.token.is_none() {
            return Ok(out);
        }
        let flags = u8::from(self.addr.is_some()) * EXTEND_ADDR
            + u8::from(self.token.is_some()) * EXTEND_TOKEN;
        out.push(flags);
        for field in [
            self.addr.as_ref().map(String::as_bytes),
            self.token.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let len = u16::try_from(field.len()).map_err(|_| Error::Malformed("extend field"))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(field);
        }
        if out.len() > MAX_DATA {
            return Err(Error::Malformed("extend too long"));
        }
        Ok(out)
    }

    pub fn decode(data: &[u8]) -> Result<Self> {
        let pk = crate::crypto::hybrid::PUBLIC_LEN;
        if data.len() < 20 + pk {
            return Err(Error::Malformed("extend"));
        }
        let to = Fingerprint(
            data[..20]
                .try_into()
                .map_err(|_| Error::Malformed("extend"))?,
        );
        let e_pub = data[20..20 + pk].to_vec();
        let mut rest = &data[20 + pk..];
        let mut req = Self {
            to,
            e_pub,
            addr: None,
            token: None,
        };
        let Some((&flags, tail)) = rest.split_first() else {
            return Ok(req);
        };
        if flags & !(EXTEND_ADDR | EXTEND_TOKEN) != 0 {
            return Err(Error::Malformed("extend flags"));
        }
        rest = tail;
        let field = |rest: &mut &[u8]| -> Result<Vec<u8>> {
            let (len, tail) = rest
                .split_at_checked(2)
                .ok_or(Error::Malformed("extend field"))?;
            let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
            let (v, tail) = tail
                .split_at_checked(len)
                .ok_or(Error::Malformed("extend field"))?;
            *rest = tail;
            Ok(v.to_vec())
        };
        if flags & EXTEND_ADDR != 0 {
            let a = String::from_utf8(field(&mut rest)?)
                .map_err(|_| Error::Malformed("extend address"))?;
            if a.is_empty()
                || a.len() > 64
                || a.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(Error::Malformed("extend address"));
            }
            req.addr = Some(a);
        }
        if flags & EXTEND_TOKEN != 0 {
            req.token = Some(field(&mut rest)?);
        }
        if !rest.is_empty() {
            return Err(Error::Malformed("extend trailing bytes"));
        }
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a client path and the matching relay keys for `n` hops.
    fn circuit(n: usize) -> (OnionPath, Vec<HopKeys>) {
        let mut path = OnionPath::default();
        let mut relays = Vec::new();
        for _ in 0..n {
            let id = Identity::generate();
            let (st, e_pub) = CreateState::new(id.public().fingerprint());
            let (rk, ct, sig) = respond(&id, &e_pub).unwrap();
            path.push(st.finish(id.public().as_bytes(), &ct, &sig).unwrap());
            relays.push(rk);
        }
        (path, relays)
    }

    #[test]
    fn extend_requests_round_trip() {
        let (_, e_pub) = CreateState::new(Fingerprint([1; 20]));
        let plain = ExtendReq {
            to: Fingerprint([2; 20]),
            e_pub: e_pub.clone(),
            addr: None,
            token: None,
        };
        // The plain form is exactly Appendix I's.
        assert_eq!(plain.encode().unwrap().len(), 20 + e_pub.len());
        for req in [
            plain.clone(),
            ExtendReq {
                addr: Some("192.0.2.1:7450".into()),
                ..plain.clone()
            },
            ExtendReq {
                token: Some(vec![9; 500]),
                ..plain.clone()
            },
            ExtendReq {
                addr: Some("[2001:db8::1]:7450".into()),
                token: Some(vec![9; 600]),
                ..plain.clone()
            },
        ] {
            let b = req.encode().unwrap();
            assert!(b.len() <= MAX_DATA);
            assert_eq!(ExtendReq::decode(&b).unwrap(), req);
        }
        let mut b = plain.encode().unwrap();
        b.extend_from_slice(&[EXTEND_TOKEN, 0, 9, 1]);
        assert!(ExtendReq::decode(&b).is_err());
        assert!(ExtendReq::decode(&b[..100]).is_err());
    }

    #[test]
    fn handshake_authenticates_the_addressed_hop() {
        let (r, other) = (Identity::generate(), Identity::generate());
        let (st, e_pub) = CreateState::new(r.public().fingerprint());
        let (_, ct, sig) = respond(&other, &e_pub).unwrap();
        assert!(
            st.finish(other.public().as_bytes(), &ct, &sig).is_err(),
            "wrong hop accepted"
        );
        let (st, e_pub) = CreateState::new(r.public().fingerprint());
        let (_, ct, mut sig) = respond(&r, &e_pub).unwrap();
        sig[0] ^= 1;
        assert!(
            st.finish(r.public().as_bytes(), &ct, &sig).is_err(),
            "bad signature accepted"
        );
    }

    #[test]
    fn forward_cells_reach_exactly_their_target() {
        let (mut path, mut relays) = circuit(3);
        for round in 0..3u8 {
            for target in 0..3 {
                let p = Payload::new(Cmd::Data, vec![round, target as u8]);
                let mut cell = path.wrap(target, &p).unwrap();
                for (i, hop) in relays.iter_mut().enumerate().take(target + 1) {
                    let got = hop.relay_forward(&mut cell);
                    if i == target {
                        assert_eq!(got.as_ref(), Some(&p));
                    } else {
                        assert!(got.is_none(), "hop {i} recognized a cell for hop {target}");
                    }
                }
            }
        }
    }

    #[test]
    fn backward_cells_identify_their_origin() {
        let (mut path, mut relays) = circuit(3);
        for origin in [2, 0, 1, 2] {
            let p = Payload::new(Cmd::Extended, vec![origin as u8; 100]);
            let mut cell = relays[origin].relay_originate(&p).unwrap();
            for hop in relays[..origin].iter_mut().rev() {
                hop.relay_backward(&mut cell);
            }
            assert_eq!(path.unwrap(cell).unwrap(), (origin, p));
        }
    }

    #[test]
    fn cells_are_fixed_size_and_opaque() {
        let (mut path, _) = circuit(2);
        let small = path.wrap(1, &Payload::new(Cmd::Begin, vec![])).unwrap();
        let big = path
            .wrap(1, &Payload::new(Cmd::Data, vec![7; MAX_DATA]))
            .unwrap();
        assert_eq!(small.len(), big.len());
        assert!(
            path.wrap(1, &Payload::new(Cmd::Data, vec![0; MAX_DATA + 1]))
                .is_err()
        );
        assert!(
            small.iter().filter(|b| **b == 0).count() < CELL_LEN / 64,
            "padding visible"
        );
    }

    #[test]
    fn tampered_or_misrouted_cells_are_not_recognized() {
        let (mut path, mut relays) = circuit(2);
        let mut cell = path
            .wrap(1, &Payload::new(Cmd::Data, b"x".to_vec()))
            .unwrap();
        cell[100] ^= 1;
        assert!(relays[0].relay_forward(&mut cell).is_none());
        assert!(relays[1].relay_forward(&mut cell).is_none());
        let (mut other, _) = circuit(1);
        assert!(other.unwrap(Box::new([0u8; CELL_LEN])).is_err());
    }
}
