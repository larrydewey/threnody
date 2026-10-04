//! Prekeys and sealed messages for offline delivery
//! (`docs/appendix-h-offline.md`, proved in `proofs/sealed.spthy`).

use std::collections::HashMap;

use const_cbor::{Decoder, Encoder};
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::hybrid::{HybridPublic, HybridSecret, SEED_LEN};
use crate::crypto::kdf::{self, label};
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::{Identity, PublicIdentity};
use crate::message::{pad, unpad};

const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// A signed prekey is replaced after this long…
pub const SPK_ROTATE_MS: u64 = 7 * DAY_MS;
/// …and stays usable (and advertised as valid) for this long after creation.
pub const SPK_LIFETIME_MS: u64 = 21 * DAY_MS;
/// One-time prekeys handed to a contact per bundle.
pub const OPKS_PER_BUNDLE: usize = 5;
/// Bundles per contact whose one-time prekey secrets we keep.
const BATCHES_KEPT: usize = 2;
const STATE_VERSION: u64 = 1;
const NONCE: [u8; 12] = [0; 12];

fn lp(parts: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for p in parts {
        v.extend_from_slice(&(p.len() as u64).to_le_bytes());
        v.extend_from_slice(p);
    }
    v
}

fn spk_sig_input(owner: &PublicIdentity, id: u32, public: &[u8], expiry_ms: u64) -> Vec<u8> {
    lp(&[
        label::SIG_PREKEY.as_bytes(),
        owner.as_bytes(),
        &id.to_le_bytes(),
        public,
        &expiry_ms.to_le_bytes(),
    ])
}

fn opk_sig_input(owner: &PublicIdentity, id: u32, public: &[u8]) -> Vec<u8> {
    lp(&[
        label::SIG_ONE_TIME_PREKEY.as_bytes(),
        owner.as_bytes(),
        &id.to_le_bytes(),
        public,
    ])
}

/// A one-time prekey as published.
#[derive(Clone, Debug, PartialEq)]
pub struct PublicOpk {
    pub id: u32,
    pub public: HybridPublic,
    pub sig: [u8; 64],
}

/// What a contact hands us so we can seal messages to it while it is away.
#[derive(Clone, Debug, PartialEq)]
pub struct PrekeyBundle {
    pub owner: PublicIdentity,
    pub spk_id: u32,
    pub spk: HybridPublic,
    pub expiry_ms: u64,
    pub spk_sig: [u8; 64],
    pub opks: Vec<PublicOpk>,
}

impl PrekeyBundle {
    /// Checks every signature against `owner` and the expiry against `now_ms`.
    pub fn verify(&self, owner: &PublicIdentity, now_ms: u64) -> Result<()> {
        if self.owner != *owner {
            return Err(Error::BadSignature);
        }
        if self.expiry_ms <= now_ms {
            return Err(Error::Malformed("prekey bundle expired"));
        }
        owner.verify(
            &spk_sig_input(owner, self.spk_id, self.spk.as_bytes(), self.expiry_ms),
            &self.spk_sig,
        )?;
        for o in &self.opks {
            owner.verify(&opk_sig_input(owner, o.id, o.public.as_bytes()), &o.sig)?;
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(1400 * (1 + self.opks.len()), |e| {
            e.map_len(6)?;
            e.u8(0)?.bytes(self.owner.as_bytes())?;
            e.u8(1)?.u32(self.spk_id)?;
            e.u8(2)?.bytes(self.spk.as_bytes())?;
            e.u8(3)?.u64(self.expiry_ms)?;
            e.u8(4)?.bytes(&self.spk_sig)?;
            e.u8(5)?.array_len(self.opks.len())?;
            for o in &self.opks {
                e.map_len(3)?;
                e.u8(0)?.u32(o.id)?;
                e.u8(1)?.bytes(o.public.as_bytes())?;
                e.u8(2)?.bytes(&o.sig)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        Self::decode_from(&mut dec).and_then(|v| finish(&dec).map(|()| v))
    }

    fn decode_from(dec: &mut Decoder<'_>) -> Result<Self> {
        let (mut owner, mut spk_id, mut spk, mut expiry, mut sig) = (None, None, None, None, None);
        let mut opks = Vec::new();
        read_map(dec, |k, d| {
            match k {
                0 => owner = Some(fixed_bytes::<32>(d)?),
                1 => spk_id = Some(d.u32()?),
                2 => spk = Some(HybridPublic::from_bytes(d.bytes()?)?),
                3 => expiry = Some(d.u64()?),
                4 => sig = Some(fixed_bytes::<64>(d)?),
                5 => {
                    let n = d.array_len()?;
                    if n > 4 * OPKS_PER_BUNDLE {
                        return Err(Error::Malformed("too many one-time prekeys"));
                    }
                    for _ in 0..n {
                        let (mut id, mut public, mut s) = (None, None, None);
                        read_map(d, |k, d| {
                            match k {
                                0 => id = Some(d.u32()?),
                                1 => public = Some(HybridPublic::from_bytes(d.bytes()?)?),
                                2 => s = Some(fixed_bytes::<64>(d)?),
                                _ => return Ok(false),
                            }
                            Ok(true)
                        })?;
                        opks.push(PublicOpk {
                            id: required(id, "opk id")?,
                            public: required(public, "opk")?,
                            sig: required(s, "opk signature")?,
                        });
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(Self {
            owner: PublicIdentity::from_bytes(&required(owner, "bundle owner")?)?,
            spk_id: required(spk_id, "spk id")?,
            spk: required(spk, "spk")?,
            expiry_ms: required(expiry, "expiry")?,
            spk_sig: required(sig, "spk signature")?,
            opks,
        })
    }
}

struct Spk {
    id: u32,
    secret: HybridSecret,
    created_ms: u64,
}

impl Spk {
    fn generate(now_ms: u64) -> Self {
        Self {
            id: u32::from_le_bytes(random_bytes()),
            secret: HybridSecret::generate(),
            created_ms: now_ms,
        }
    }

    fn expiry_ms(&self) -> u64 {
        self.created_ms + SPK_LIFETIME_MS
    }
}

struct Opk {
    secret: HybridSecret,
    peer: [u8; 32],
    batch: u64,
}

/// The recipient's secret prekey state (persist with `Home::save_state`).
pub struct PrekeyStore {
    current: Spk,
    previous: Option<Spk>,
    opks: HashMap<u32, Opk>,
    next_batch: u64,
    /// Transcript hashes of accepted messages, with the time they may be forgotten.
    seen: HashMap<[u8; 32], u64>,
}

impl PrekeyStore {
    pub fn new(now_ms: u64) -> Self {
        Self {
            current: Spk::generate(now_ms),
            previous: None,
            opks: HashMap::new(),
            next_batch: 0,
            seen: HashMap::new(),
        }
    }

    /// Rotates the signed prekey and forgets expired material.
    pub fn maintain(&mut self, now_ms: u64) {
        if now_ms >= self.current.created_ms + SPK_ROTATE_MS {
            let old = std::mem::replace(&mut self.current, Spk::generate(now_ms));
            self.previous = Some(old);
        }
        if self
            .previous
            .as_ref()
            .is_some_and(|p| now_ms >= p.expiry_ms())
        {
            self.previous = None;
        }
        self.seen.retain(|_, until| *until > now_ms);
    }

    /// A bundle with the signed prekey only, for sibling devices to pass
    /// on to their contacts (Appendix J). Without one-time prekeys, forward
    /// secrecy for messages sealed to it starts when the SPK retires.
    pub fn shared_bundle(&mut self, identity: &Identity, now_ms: u64) -> PrekeyBundle {
        self.maintain(now_ms);
        let owner = identity.public();
        let expiry_ms = self.current.expiry_ms();
        let spk = self.current.secret.public().clone();
        PrekeyBundle {
            owner,
            spk_id: self.current.id,
            spk_sig: identity.sign(&spk_sig_input(
                &owner,
                self.current.id,
                spk.as_bytes(),
                expiry_ms,
            )),
            spk,
            expiry_ms,
            opks: vec![],
        }
    }

    /// A fresh bundle for `peer`, with one-time prekeys reserved for it.
    pub fn bundle_for(
        &mut self,
        identity: &Identity,
        peer: &PublicIdentity,
        now_ms: u64,
    ) -> PrekeyBundle {
        self.maintain(now_ms);
        let owner = identity.public();
        let batch = self.next_batch;
        self.next_batch += 1;
        let mut opks = Vec::with_capacity(OPKS_PER_BUNDLE);
        for _ in 0..OPKS_PER_BUNDLE {
            let mut id = u32::from_le_bytes(random_bytes());
            while self.opks.contains_key(&id) {
                id = u32::from_le_bytes(random_bytes());
            }
            let secret = HybridSecret::generate();
            let sig = identity.sign(&opk_sig_input(&owner, id, secret.public().as_bytes()));
            opks.push(PublicOpk {
                id,
                public: secret.public().clone(),
                sig,
            });
            self.opks.insert(
                id,
                Opk {
                    secret,
                    peer: *peer.as_bytes(),
                    batch,
                },
            );
        }
        // Keep only the newest batches reserved for this peer.
        let mut batches: Vec<u64> = self
            .opks
            .values()
            .filter(|o| o.peer == *peer.as_bytes())
            .map(|o| o.batch)
            .collect();
        batches.sort_unstable();
        batches.dedup();
        if batches.len() > BATCHES_KEPT {
            let cutoff = batches[batches.len() - BATCHES_KEPT];
            self.opks
                .retain(|_, o| o.peer != *peer.as_bytes() || o.batch >= cutoff);
        }
        let expiry_ms = self.current.expiry_ms();
        let spk = self.current.secret.public().clone();
        PrekeyBundle {
            owner,
            spk_id: self.current.id,
            spk_sig: identity.sign(&spk_sig_input(
                &owner,
                self.current.id,
                spk.as_bytes(),
                expiry_ms,
            )),
            spk,
            expiry_ms,
            opks,
        }
    }

    /// Opens a sealed message addressed to `me`. `known` maps an identity
    /// key to a contact we accept sealed messages from. Returns the sender
    /// and the unpadded body.
    pub fn open(
        &mut self,
        me: &PublicIdentity,
        sealed: &[u8],
        now_ms: u64,
        known: impl Fn(&[u8; 32]) -> Option<PublicIdentity>,
    ) -> Result<(PublicIdentity, Vec<u8>)> {
        self.maintain(now_ms);
        let s = Sealed::decode(sealed)?;
        let spk = [Some(&self.current), self.previous.as_ref()]
            .into_iter()
            .flatten()
            .find(|k| k.id == s.spk_id)
            .ok_or(Error::Decrypt)?;
        let ss_s = spk.secret.decapsulate(&s.ct_s)?;
        let ss_o = match (s.opk_id, &s.ct_o) {
            (Some(id), Some(ct)) => Some(
                self.opks
                    .get(&id)
                    .ok_or(Error::Decrypt)?
                    .secret
                    .decapsulate(ct)?,
            ),
            (None, None) => None,
            _ => return Err(Error::Malformed("opk id without ciphertext")),
        };
        let h = transcript(me, s.spk_id, &s.ct_s, s.opk_id, s.ct_o.as_deref());
        if self.seen.contains_key(&h) {
            return Err(Error::Replay);
        }
        let k = message_key(&ss_s, ss_o.as_deref().map(|x| &x[..]), &h);
        let inner = Suite::ChaCha20Poly1305.open(&k, &NONCE, &h, &s.aead)?;
        let (sender, sig, body) = decode_inner(&inner)?;
        let sender =
            known(&sender).ok_or(Error::Malformed("sealed message from unknown sender"))?;
        sender.verify(&sender_sig_input(&h, &sender, me, &body), &sig)?;
        // Accept: consume the one-time prekey and remember the transcript.
        if let Some(id) = s.opk_id {
            self.opks.remove(&id);
        }
        self.seen.insert(h, spk.expiry_ms());
        Ok((sender, unpad(body)?))
    }

    pub fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        let spk = |e: &mut Encoder<'_>, k: &Spk| -> core::result::Result<(), const_cbor::Error> {
            e.array_len(3)?
                .u32(k.id)?
                .bytes(k.secret.seed())?
                .u64(k.created_ms)?;
            Ok(())
        };
        Ok(Zeroizing::new(cbor::to_vec(
            256 + self.opks.len() * 96 + self.seen.len() * 48,
            |e| {
                e.map_len(6)?;
                e.u8(0)?.u64(STATE_VERSION)?;
                e.u8(1)?;
                spk(e, &self.current)?;
                e.u8(2)?;
                match &self.previous {
                    Some(p) => spk(e, p)?,
                    None => {
                        e.null()?;
                    }
                }
                e.u8(3)?.array_len(self.opks.len())?;
                for (id, o) in &self.opks {
                    e.array_len(4)?
                        .u32(*id)?
                        .bytes(o.secret.seed())?
                        .bytes(&o.peer)?
                        .u64(o.batch)?;
                }
                e.u8(4)?.u64(self.next_batch)?;
                e.u8(5)?.array_len(self.seen.len())?;
                for (h, until) in &self.seen {
                    e.array_len(2)?.bytes(h)?.u64(*until)?;
                }
                Ok(())
            },
        )?))
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        fn spk(d: &mut Decoder<'_>) -> Result<Spk> {
            if d.array_len()? != 3 {
                return Err(Error::Malformed("spk"));
            }
            let id = d.u32()?;
            let seed = Zeroizing::new(fixed_bytes::<SEED_LEN>(d)?);
            Ok(Spk {
                id,
                secret: HybridSecret::from_seed(&seed),
                created_ms: d.u64()?,
            })
        }
        let mut dec = Decoder::new(b);
        let (mut ver, mut current, mut previous, mut next_batch) = (None, None, None, None);
        let (mut opks, mut seen) = (HashMap::new(), HashMap::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => current = Some(spk(d)?),
                2 => previous = if d.is_null()? { None } else { Some(spk(d)?) },
                3 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 4 {
                            return Err(Error::Malformed("opk"));
                        }
                        let id = d.u32()?;
                        let seed = Zeroizing::new(fixed_bytes::<SEED_LEN>(d)?);
                        let peer = fixed_bytes::<32>(d)?;
                        let batch = d.u64()?;
                        opks.insert(
                            id,
                            Opk {
                                secret: HybridSecret::from_seed(&seed),
                                peer,
                                batch,
                            },
                        );
                    }
                }
                4 => next_batch = Some(d.u64()?),
                5 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 2 {
                            return Err(Error::Malformed("seen"));
                        }
                        let h = fixed_bytes::<32>(d)?;
                        seen.insert(h, d.u64()?);
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(STATE_VERSION) {
            return Err(Error::Malformed("prekey state version"));
        }
        Ok(Self {
            current: required(current, "current spk")?,
            previous,
            opks,
            next_batch: required(next_batch, "batch counter")?,
            seen,
        })
    }
}

/// Bundles contacts gave us (persist with `Home::save_state`).
#[derive(Default)]
pub struct BundleBook {
    bundles: HashMap<[u8; 32], PrekeyBundle>,
}

impl BundleBook {
    /// Stores a bundle received in an authenticated session with `from`.
    pub fn insert(
        &mut self,
        from: &PublicIdentity,
        bundle: PrekeyBundle,
        now_ms: u64,
    ) -> Result<()> {
        bundle.verify(from, now_ms)?;
        self.bundles.insert(*from.as_bytes(), bundle);
        Ok(())
    }

    /// Stores a bundle a sibling of `owner` forwarded. It never replaces a
    /// direct bundle that still has one-time prekeys, only an absent,
    /// expired or older SPK-only one. Returns true if stored.
    pub fn offer_forwarded(
        &mut self,
        owner: &PublicIdentity,
        bundle: PrekeyBundle,
        now_ms: u64,
    ) -> Result<bool> {
        bundle.verify(owner, now_ms)?;
        let keep_existing = self.bundles.get(owner.as_bytes()).is_some_and(|b| {
            b.expiry_ms > now_ms && (!b.opks.is_empty() || b.expiry_ms >= bundle.expiry_ms)
        });
        if keep_existing {
            return Ok(false);
        }
        self.bundles.insert(*owner.as_bytes(), bundle);
        Ok(true)
    }

    pub fn has(&self, peer: &PublicIdentity, now_ms: u64) -> bool {
        self.bundles
            .get(peer.as_bytes())
            .is_some_and(|b| b.expiry_ms > now_ms)
    }

    /// Seals `body` (an encoded `AppMessage`) from `identity` to `to`,
    /// consuming a one-time prekey if any is left.
    pub fn seal(
        &mut self,
        identity: &Identity,
        to: &PublicIdentity,
        body: &[u8],
        now_ms: u64,
    ) -> Result<Vec<u8>> {
        let bundle = self
            .bundles
            .get_mut(to.as_bytes())
            .filter(|b| b.expiry_ms > now_ms)
            .ok_or(Error::Malformed("no prekey bundle for recipient"))?;
        let (ct_s, ss_s) = bundle.spk.encapsulate()?;
        let opk = bundle.opks.pop();
        let (opk_id, ct_o, ss_o) = match &opk {
            Some(o) => {
                let (ct, ss) = o.public.encapsulate()?;
                (Some(o.id), Some(ct), Some(ss))
            }
            None => (None, None, None),
        };
        let h = transcript(to, bundle.spk_id, &ct_s, opk_id, ct_o.as_deref());
        let k = message_key(&ss_s, ss_o.as_deref().map(|x| &x[..]), &h);
        let me = identity.public();
        let body = pad(body.to_vec());
        let sig = identity.sign(&sender_sig_input(&h, &me, to, &body));
        let inner = cbor::to_vec(body.len() + 128, |e| {
            e.map_len(3)?;
            e.u8(0)?.bytes(me.as_bytes())?;
            e.u8(1)?.bytes(&sig)?;
            e.u8(2)?.bytes(&body)?;
            Ok(())
        })?;
        let aead = Suite::ChaCha20Poly1305.seal(&k, &NONCE, &h, &inner);
        Sealed {
            spk_id: bundle.spk_id,
            ct_s,
            opk_id,
            ct_o,
            aead,
        }
        .encode()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let parts: Vec<Vec<u8>> = self
            .bundles
            .values()
            .map(PrekeyBundle::encode)
            .collect::<Result<_>>()?;
        cbor::to_vec(parts.iter().map(Vec::len).sum::<usize>() + 64, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(STATE_VERSION)?;
            e.u8(1)?.array_len(parts.len())?;
            for p in &parts {
                e.bytes(p)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut ver, mut bundles) = (None, HashMap::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    for _ in 0..d.array_len()? {
                        let bundle = PrekeyBundle::decode(d.bytes()?)?;
                        bundles.insert(*bundle.owner.as_bytes(), bundle);
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(STATE_VERSION) {
            return Err(Error::Malformed("bundle book version"));
        }
        Ok(Self { bundles })
    }
}

struct Sealed {
    spk_id: u32,
    ct_s: Vec<u8>,
    opk_id: Option<u32>,
    ct_o: Option<Vec<u8>>,
    aead: Vec<u8>,
}

impl Sealed {
    fn encode(&self) -> Result<Vec<u8>> {
        let n = 3 + 2 * usize::from(self.opk_id.is_some());
        cbor::to_vec(self.aead.len() + 2400, |e| {
            e.map_len(n)?;
            e.u8(0)?.u32(self.spk_id)?;
            e.u8(1)?.bytes(&self.ct_s)?;
            if let (Some(id), Some(ct)) = (self.opk_id, &self.ct_o) {
                e.u8(2)?.u32(id)?;
                e.u8(3)?.bytes(ct)?;
            }
            e.u8(4)?.bytes(&self.aead)?;
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut spk_id, mut ct_s, mut opk_id, mut ct_o, mut aead) = (None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => spk_id = Some(d.u32()?),
                1 => ct_s = Some(d.bytes()?.to_vec()),
                2 => opk_id = Some(d.u32()?),
                3 => ct_o = Some(d.bytes()?.to_vec()),
                4 => aead = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            spk_id: required(spk_id, "spk id")?,
            ct_s: required(ct_s, "spk ciphertext")?,
            opk_id,
            ct_o,
            aead: required(aead, "sealed payload")?,
        })
    }
}

fn transcript(
    to: &PublicIdentity,
    spk_id: u32,
    ct_s: &[u8],
    opk_id: Option<u32>,
    ct_o: Option<&[u8]>,
) -> [u8; 32] {
    let opk = opk_id.map(u32::to_le_bytes);
    kdf::derive(
        label::SEALED_TRANSCRIPT,
        &[
            to.as_bytes(),
            &spk_id.to_le_bytes(),
            ct_s,
            opk.as_ref().map_or(&[][..], |b| &b[..]),
            ct_o.unwrap_or_default(),
        ],
    )
}

fn message_key(ss_s: &[u8; 32], ss_o: Option<&[u8]>, h: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(kdf::derive(
        label::SEALED_KEY,
        &[ss_s, ss_o.unwrap_or_default(), h],
    ))
}

fn sender_sig_input(
    h: &[u8; 32],
    from: &PublicIdentity,
    to: &PublicIdentity,
    body: &[u8],
) -> Vec<u8> {
    lp(&[
        label::SIG_SEALED_SENDER.as_bytes(),
        h,
        from.as_bytes(),
        to.as_bytes(),
        blake3::hash(body).as_bytes(),
    ])
}

fn decode_inner(b: &[u8]) -> Result<([u8; 32], [u8; 64], Vec<u8>)> {
    let mut dec = Decoder::new(b);
    let (mut id, mut sig, mut body) = (None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => id = Some(fixed_bytes::<32>(d)?),
            1 => sig = Some(fixed_bytes::<64>(d)?),
            2 => body = Some(d.bytes()?.to_vec()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    Ok((
        required(id, "sender")?,
        required(sig, "sender signature")?,
        required(body, "body")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_791_000_000_000;

    fn setup() -> (Identity, Identity, PrekeyStore, BundleBook) {
        let (alice, bob) = (Identity::generate(), Identity::generate());
        let mut store = PrekeyStore::new(NOW);
        let bundle = store.bundle_for(&bob, &alice.public(), NOW);
        let bundle = PrekeyBundle::decode(&bundle.encode().unwrap()).unwrap();
        let mut book = BundleBook::default();
        book.insert(&bob.public(), bundle, NOW).unwrap();
        (alice, bob, store, book)
    }

    fn known(a: &Identity) -> impl Fn(&[u8; 32]) -> Option<PublicIdentity> + '_ {
        move |k| (k == a.public().as_bytes()).then(|| a.public())
    }

    #[test]
    fn seal_and_open_with_and_without_one_time_prekeys() {
        let (alice, bob, mut store, mut book) = setup();
        for i in 0..OPKS_PER_BUNDLE + 2 {
            let body = format!("msg {i}");
            let sealed = book
                .seal(&alice, &bob.public(), body.as_bytes(), NOW)
                .unwrap();
            let (from, got) = store
                .open(&bob.public(), &sealed, NOW, known(&alice))
                .unwrap();
            assert_eq!((from, got), (alice.public(), body.into_bytes()));
        }
        assert!(store.opks.is_empty(), "every one-time prekey consumed");
    }

    #[test]
    fn replays_forgeries_and_strangers_are_rejected() {
        let (alice, bob, mut store, mut book) = setup();
        let sealed = book.seal(&alice, &bob.public(), b"hi", NOW).unwrap();
        store
            .open(&bob.public(), &sealed, NOW, known(&alice))
            .unwrap();
        assert!(
            store
                .open(&bob.public(), &sealed, NOW, known(&alice))
                .is_err(),
            "replay"
        );

        // SPK-only replay is caught by the transcript cache.
        book.bundles
            .get_mut(bob.public().as_bytes())
            .unwrap()
            .opks
            .clear();
        let spk_only = book.seal(&alice, &bob.public(), b"x", NOW).unwrap();
        store
            .open(&bob.public(), &spk_only, NOW, known(&alice))
            .unwrap();
        assert!(matches!(
            store.open(&bob.public(), &spk_only, NOW, known(&alice)),
            Err(Error::Replay)
        ));

        // Unknown sender, wrong recipient, tampering.
        let s2 = book.seal(&alice, &bob.public(), b"y", NOW).unwrap();
        assert!(store.open(&bob.public(), &s2, NOW, |_| None).is_err());
        let carol = Identity::generate().public();
        assert!(store.open(&carol, &s2, NOW, known(&alice)).is_err());
        let mut bad = s2.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(store.open(&bob.public(), &bad, NOW, known(&alice)).is_err());
        store.open(&bob.public(), &s2, NOW, known(&alice)).unwrap();
    }

    #[test]
    fn shared_bundles_seal_spk_only() {
        let (alice, bob) = (Identity::generate(), Identity::generate());
        let mut store = PrekeyStore::new(NOW);
        let shared = store.shared_bundle(&bob, NOW);
        assert!(shared.opks.is_empty());
        let mut book = BundleBook::default();
        book.insert(&bob.public(), shared, NOW).unwrap();
        let sealed = book
            .seal(&alice, &bob.public(), b"via a sibling", NOW)
            .unwrap();
        let (_, body) = store
            .open(&bob.public(), &sealed, NOW, |k| {
                (k == alice.public().as_bytes()).then(|| alice.public())
            })
            .unwrap();
        assert_eq!(body, b"via a sibling");
    }

    #[test]
    fn bundles_are_signed_and_expire() {
        let (alice, bob) = (Identity::generate(), Identity::generate());
        let mut store = PrekeyStore::new(NOW);
        let mut b = store.bundle_for(&bob, &alice.public(), NOW);
        b.verify(&bob.public(), NOW).unwrap();
        assert!(b.verify(&alice.public(), NOW).is_err());
        assert!(b.verify(&bob.public(), NOW + SPK_LIFETIME_MS).is_err());
        b.opks[0].sig[0] ^= 1;
        assert!(b.verify(&bob.public(), NOW).is_err());
    }

    #[test]
    fn rotation_keeps_in_flight_messages_readable_then_forgets() {
        let (alice, bob, mut store, mut book) = setup();
        let sealed = book.seal(&alice, &bob.public(), b"late", NOW).unwrap();
        let later = NOW + SPK_ROTATE_MS + 1;
        store.maintain(later);
        assert!(store.previous.is_some());
        store
            .open(&bob.public(), &sealed, later, known(&alice))
            .unwrap();
        let sealed2 = book.seal(&alice, &bob.public(), b"too late", NOW).unwrap();
        assert!(
            store
                .open(
                    &bob.public(),
                    &sealed2,
                    NOW + SPK_LIFETIME_MS + 1,
                    known(&alice)
                )
                .is_err()
        );
    }

    #[test]
    fn only_recent_batches_per_peer_are_kept() {
        let (alice, bob) = (Identity::generate(), Identity::generate());
        let mut store = PrekeyStore::new(NOW);
        for _ in 0..4 {
            store.bundle_for(&bob, &alice.public(), NOW);
        }
        assert_eq!(store.opks.len(), BATCHES_KEPT * OPKS_PER_BUNDLE);
    }

    #[test]
    fn state_round_trips() {
        let (alice, bob, mut store, mut book) = setup();
        let sealed = book.seal(&alice, &bob.public(), b"persisted", NOW).unwrap();
        let mut store2 = PrekeyStore::decode(&store.encode().unwrap()).unwrap();
        let mut book2 = BundleBook::decode(&book.encode().unwrap()).unwrap();
        store2
            .open(&bob.public(), &sealed, NOW, known(&alice))
            .unwrap();
        let s2 = book2.seal(&alice, &bob.public(), b"again", NOW).unwrap();
        store.open(&bob.public(), &s2, NOW, known(&alice)).unwrap();
    }
}
