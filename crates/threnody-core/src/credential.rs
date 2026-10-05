//! Zero-knowledge credentials (Appendix O, spec §4.3).
//!
//! BBS signatures on BLS12-381 with blind issuance and per-verifier
//! pseudonyms, following the IETF CFRG drafts (`draft-irtf-cfrg-bbs-
//! signatures`, `…-bbs-blind-signatures`, `…-bbs-per-verifier-linkability`)
//! through `zkryptium`. An issuer signs a list of attributes together with
//! a secret only the holder knows; the holder then proves any subset of the
//! attributes to a verifier without revealing the rest, and two
//! presentations of one credential can't be linked, except that a verifier
//! sees the same pseudonym every time for its own context.
//!
//! Issuer keys and issuance are post-quantum authenticated as well: an
//! issuer key carries Ed25519 and ML-DSA-65 signatures by the issuing
//! identity, and every issued credential comes with an ML-DSA receipt. A
//! presentation itself is a BBS proof and so only classically sound (no
//! standardised post-quantum anonymous credential exists); see Appendix O.
//!
//! ```text
//! IssuerKey   = { 0: 1, 1: identity (32), 2: generation, 3: bbs_pk (96),
//!                 4: mldsa_pk (1952), 5: sig_ed (64), 6: sig_mldsa (3309) }
//! header      = { 0: 1, 1: issuer_id (32), 2: schema tstr, 3: expires_day }
//! attribute   = [ key tstr, value tstr ]                (one BBS message each)
//! Issued      = { 0: header, 1: [* attribute], 2: signature (80),
//!                 3: nym_entropy (32), 4: receipt (ML-DSA) }
//! Presentation= { 0: issuer_id, 1: header, 2: total, 3: [* [index, key, value]],
//!                 4: proof, 5: pseudonym (48) }
//! ```

use const_cbor::Decoder;
use zeroize::Zeroizing;
use zkryptium::bbsplus::commitment::BlindFactor;
use zkryptium::bbsplus::keys::{BBSplusPublicKey, BBSplusSecretKey};
use zkryptium::bbsplus::pseudonym::{BBSplusPseudonym, PseudonymSecret};
use zkryptium::keys::pair::KeyPair;
use zkryptium::schemes::algorithms::{BBSplus, BbsBls12381Sha256, Scheme};
use zkryptium::schemes::generics::{BlindSignature, Commitment, PoKSignature};

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::kdf::{derive, label};
use crate::crypto::pqsig::{self, PqSigner};
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::{Identity, PublicIdentity};
use crate::persona::{Profile, check_profile};

type Cs = <BbsBls12381Sha256 as Scheme>::Ciphersuite;
type Bbs = BBSplus<Cs>;

pub const BBS_PUBLIC_LEN: usize = 96;
pub const SIGNATURE_LEN: usize = 80;
pub const PSEUDONYM_LEN: usize = 48;
pub const MAX_SCHEMA: usize = 64;
/// Largest proof a presentation may carry (16 attributes, all hidden).
const MAX_PROOF: usize = 2048;
const PQ_CTX_KEY: &[u8] = b"threnody issuer key";
const PQ_CTX_RECEIPT: &[u8] = b"threnody credential receipt";
const PQ_CTX_DIRECTORY: &[u8] = b"threnody directory";
/// One pseudonym secret per credential.
const NYMS: usize = 1;

/// The schema of the anonymous tokens that volunteer relays accept.
pub const RELAY_SCHEMA: &str = "threnody/relay-access/1";
/// Circuits one relay token credential opens per relay per day.
pub const RELAY_SLOTS: u16 = 64;

fn bbs_err(_: zkryptium::errors::Error) -> Error {
    Error::BadSignature
}

/// Runs a `zkryptium` call on bytes that may come from the network. Its
/// parsers index and subtract without checking (an IETF-draft
/// implementation), so sizes are checked before every call; this also
/// turns any panic that remains into a verification failure rather than a
/// crash a peer could trigger.
fn guarded<T>(f: impl FnOnce() -> core::result::Result<T, zkryptium::errors::Error>) -> Result<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
        .map_err(|_| Error::BadSignature)?
        .map_err(bbs_err)
}

/// A request's commitment: one committed secret and one pseudonym secret,
/// with its proof of knowledge.
pub const COMMITMENT_LEN: usize = 176;

/// The exact length of a proof over `total` attributes with `disclosed` of
/// them shown: 3 points, 3 scalars, one response per hidden value (the
/// hidden attributes, the holder's secret, its pseudonym secret and the
/// blinding factor), and the challenge.
fn proof_len(total: usize, disclosed: usize) -> usize {
    3 * 48 + 3 * 32 + 32 * (total - disclosed + 3) + 32
}

/// Days since the Unix epoch, the unit of credential expiry and relay epochs.
pub fn day(now_ms: u64) -> u32 {
    u32::try_from(now_ms / 86_400_000).unwrap_or(u32::MAX)
}

// ----- Issuer keys -----

/// An issuer's public key bundle: its BBS key and an ML-DSA key, both
/// signed by the issuing identity with Ed25519 and ML-DSA.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuerKey {
    pub identity: PublicIdentity,
    pub generation: u32,
    pub bbs_public: [u8; BBS_PUBLIC_LEN],
    pub pq_public: Vec<u8>,
    sig_ed: [u8; 64],
    sig_pq: Vec<u8>,
}

fn key_parts(identity: &PublicIdentity, generation: u32, bbs: &[u8], pq: &[u8]) -> [u8; 64] {
    derive(
        label::SIG_ISSUER_KEY,
        &[identity.as_bytes(), &generation.to_le_bytes(), bbs, pq],
    )
}

impl IssuerKey {
    /// What credentials, documents and links name the issuer by. Pinning
    /// it pins the ML-DSA key too, which is what makes issuance and
    /// directories post-quantum authenticated.
    pub fn id(&self) -> [u8; 32] {
        derive(
            label::ISSUER_ID,
            &[
                self.identity.as_bytes(),
                &self.generation.to_le_bytes(),
                &self.bbs_public,
                &self.pq_public,
            ],
        )
    }

    /// Checks both signatures by the issuing identity.
    pub fn verify(&self) -> Result<()> {
        let msg = key_parts(
            &self.identity,
            self.generation,
            &self.bbs_public,
            &self.pq_public,
        );
        self.identity.verify(&msg, &self.sig_ed)?;
        pqsig::verify(&self.pq_public, &msg, PQ_CTX_KEY, &self.sig_pq)?;
        BBSplusPublicKey::from_bytes(&self.bbs_public).map_err(|_| Error::InvalidKey)?;
        Ok(())
    }

    /// Checks a signature over `msg` by both of the issuer's keys (Ed25519
    /// by its identity, and ML-DSA), as directory documents carry.
    pub fn verify_document(&self, msg: &[u8], sig_ed: &[u8; 64], sig_pq: &[u8]) -> Result<()> {
        self.identity.verify(msg, sig_ed)?;
        pqsig::verify(&self.pq_public, msg, PQ_CTX_DIRECTORY, sig_pq)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(6000, |e| {
            e.map_len(7)?.u8(0)?.u8(1)?;
            e.u8(1)?.bytes(self.identity.as_bytes())?;
            e.u8(2)?.u32(self.generation)?;
            e.u8(3)?.bytes(&self.bbs_public)?;
            e.u8(4)?.bytes(&self.pq_public)?;
            e.u8(5)?.bytes(&self.sig_ed)?;
            e.u8(6)?.bytes(&self.sig_pq)?;
            Ok(())
        })
    }

    /// Decodes and verifies an issuer key.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut v, mut id, mut gen_, mut bbs, mut pq, mut sed, mut spq) =
            (None, None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => v = Some(d.u64()?),
                1 => id = Some(fixed_bytes::<32>(d)?),
                2 => gen_ = Some(d.u32()?),
                3 => bbs = Some(fixed_bytes::<BBS_PUBLIC_LEN>(d)?),
                4 => pq = Some(d.bytes()?.to_vec()),
                5 => sed = Some(fixed_bytes::<64>(d)?),
                6 => spq = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if required(v, "issuer key version")? != 1 {
            return Err(Error::UnsupportedVersion(v.unwrap_or(0)));
        }
        let key = Self {
            identity: PublicIdentity::from_bytes(&required(id, "issuer identity")?)?,
            generation: required(gen_, "issuer key generation")?,
            bbs_public: required(bbs, "bbs key")?,
            pq_public: required(pq, "ml-dsa key")?,
            sig_ed: required(sed, "issuer signature")?,
            sig_pq: required(spq, "issuer ml-dsa signature")?,
        };
        key.verify()?;
        Ok(key)
    }
}

/// An identity acting as a credential issuer (or a relay directory). Its
/// keys are derived from the identity's seed and a generation number, so
/// nothing new needs storing.
pub struct Issuer {
    identity: Identity,
    bbs_secret: BBSplusSecretKey,
    bbs_public: BBSplusPublicKey,
    pq: PqSigner,
    key: IssuerKey,
}

impl Issuer {
    pub fn new(identity: &Identity, generation: u32) -> Result<Self> {
        let seed = identity.seed();
        let gen_bytes = generation.to_le_bytes();
        let ikm = Zeroizing::new(derive::<32>(
            label::ISSUER_BBS_KEY,
            &[&seed[..], &gen_bytes],
        ));
        let pq_seed = Zeroizing::new(derive::<32>(
            label::ISSUER_MLDSA_KEY,
            &[&seed[..], &gen_bytes],
        ));
        let pair = KeyPair::<Bbs>::generate(&ikm[..], None, None).map_err(|_| Error::InvalidKey)?;
        let pq = PqSigner::from_seed(&pq_seed);
        let bbs_public = pair.public_key().to_bytes();
        let pq_public = pq.public();
        let msg = key_parts(&identity.public(), generation, &bbs_public, &pq_public);
        let key = IssuerKey {
            identity: identity.public(),
            generation,
            bbs_public,
            pq_public,
            sig_ed: identity.sign(&msg),
            sig_pq: pq.sign(&msg, PQ_CTX_KEY)?,
        };
        Ok(Self {
            identity: Identity::from_seed(&seed),
            bbs_secret: BBSplusSecretKey::from_bytes(&pair.private_key().to_bytes())
                .map_err(|_| Error::InvalidKey)?,
            bbs_public: pair.public_key().clone(),
            pq,
            key,
        })
    }

    pub fn key(&self) -> &IssuerKey {
        &self.key
    }

    /// Signs `msg` with both of the issuer's keys, as directory documents do.
    pub fn sign_document(&self, msg: &[u8]) -> Result<([u8; 64], Vec<u8>)> {
        Ok((
            self.identity.sign(msg),
            self.pq.sign(msg, PQ_CTX_DIRECTORY)?,
        ))
    }

    /// Issues a credential on `attributes` to whoever made `request`
    /// (`Request::commitment`), valid through `expires_day`.
    pub fn issue(
        &self,
        commitment: &[u8],
        schema: &str,
        attributes: &Profile,
        expires_day: u32,
    ) -> Result<Issued> {
        check_schema(schema)?;
        check_profile(attributes)?;
        if commitment.len() != COMMITMENT_LEN {
            return Err(Error::Malformed("credential request"));
        }
        let header = Header {
            issuer: self.key.id(),
            schema: schema.to_owned(),
            expires_day,
        }
        .encode()?;
        let messages = attribute_messages(attributes)?;
        let entropy = PseudonymSecret::random();
        let sig = guarded(|| {
            BlindSignature::<Bbs>::blind_sign_with_nym(
                &self.bbs_secret,
                &self.bbs_public,
                Some(commitment),
                NYMS,
                Some(&header),
                &entropy,
                Some(&messages),
            )
        })
        .map_err(|_| Error::Malformed("credential request"))?;
        let signature = sig.to_bytes();
        let nym_entropy = entropy.to_bytes();
        let receipt = self.pq.sign(
            &receipt_digest(&header, &messages, commitment, &signature, &nym_entropy),
            PQ_CTX_RECEIPT,
        )?;
        Ok(Issued {
            header,
            attributes: attributes.clone(),
            signature,
            nym_entropy,
            receipt,
        })
    }
}

fn check_schema(schema: &str) -> Result<()> {
    if schema.is_empty() || schema.len() > MAX_SCHEMA || schema.chars().any(char::is_control) {
        return Err(Error::Malformed("credential schema"));
    }
    Ok(())
}

fn attribute_messages(attrs: &Profile) -> Result<Vec<Vec<u8>>> {
    attrs.iter().map(|(k, v)| attribute_message(k, v)).collect()
}

fn attribute_message(k: &str, v: &str) -> Result<Vec<u8>> {
    cbor::to_vec(16 + k.len() + v.len(), |e| {
        e.array_len(2)?.str(k)?.str(v)?;
        Ok(())
    })
}

fn receipt_digest(
    header: &[u8],
    messages: &[Vec<u8>],
    commitment: &[u8],
    signature: &[u8],
    entropy: &[u8],
) -> [u8; 64] {
    let mut parts: Vec<&[u8]> = vec![header, commitment, signature, entropy];
    parts.extend(messages.iter().map(Vec::as_slice));
    derive(label::CREDENTIAL_RECEIPT, &parts)
}

// ----- Headers -----

/// The signed, always-disclosed part of a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub issuer: [u8; 32],
    pub schema: String,
    pub expires_day: u32,
}

impl Header {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(128, |e| {
            e.map_len(4)?.u8(0)?.u8(1)?;
            e.u8(1)?.bytes(&self.issuer)?;
            e.u8(2)?.str(&self.schema)?;
            e.u8(3)?.u32(self.expires_day)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut v, mut issuer, mut schema, mut exp) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => v = Some(d.u64()?),
                1 => issuer = Some(fixed_bytes::<32>(d)?),
                2 => schema = Some(d.str()?.to_owned()),
                3 => exp = Some(d.u32()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if required(v, "header version")? != 1 {
            return Err(Error::UnsupportedVersion(v.unwrap_or(0)));
        }
        let h = Self {
            issuer: required(issuer, "issuer id")?,
            schema: required(schema, "schema")?,
            expires_day: required(exp, "expiry")?,
        };
        check_schema(&h.schema)?;
        // The exact bytes are signed: only the canonical encoding is accepted.
        if h.encode()? != b {
            return Err(Error::Malformed("non-canonical credential header"));
        }
        Ok(h)
    }
}

// ----- Holder side -----

/// The holder's secrets for a credential being issued.
pub struct Request {
    link_secret: Zeroizing<[u8; 32]>,
    nym: Zeroizing<[u8; 32]>,
    blind: Zeroizing<[u8; 32]>,
    /// Sent to the issuer: a commitment to the holder's secrets with a
    /// proof that it is well formed. It reveals nothing about them.
    pub commitment: Vec<u8>,
}

impl Request {
    pub fn new() -> Result<Self> {
        let link_secret = Zeroizing::new(random_bytes::<32>());
        let nym = PseudonymSecret::random();
        let (commitment, blind) =
            Commitment::<Bbs>::commit_with_nym(Some(&[link_secret.to_vec()]), vec![nym.clone()])
                .map_err(|_| Error::Malformed("credential commitment"))?;
        debug_assert_eq!(commitment.to_bytes().len(), COMMITMENT_LEN);
        Ok(Self {
            link_secret,
            nym: Zeroizing::new(nym.to_bytes()),
            blind: Zeroizing::new(blind.to_bytes()),
            commitment: commitment.to_bytes(),
        })
    }

    /// Checks what the issuer sent (BBS signature and ML-DSA receipt, both
    /// against `key`) and keeps the credential.
    pub fn finish(self, key: &IssuerKey, issued: &Issued) -> Result<Credential> {
        let header = Header::decode(&issued.header)?;
        if header.issuer != key.id() {
            return Err(Error::Malformed("credential from another issuer"));
        }
        check_profile(&issued.attributes)?;
        let messages = attribute_messages(&issued.attributes)?;
        pqsig::verify(
            &key.pq_public,
            &receipt_digest(
                &issued.header,
                &messages,
                &self.commitment,
                &issued.signature,
                &issued.nym_entropy,
            ),
            PQ_CTX_RECEIPT,
            &issued.receipt,
        )?;
        let pk = BBSplusPublicKey::from_bytes(&key.bbs_public).map_err(|_| Error::InvalidKey)?;
        let sig = BlindSignature::<Bbs>::from_bytes(&issued.signature).map_err(bbs_err)?;
        let entropy = PseudonymSecret::from_bytes(&issued.nym_entropy).map_err(bbs_err)?;
        let blind = BlindFactor::from_bytes(&self.blind).map_err(bbs_err)?;
        let nym = PseudonymSecret::from_bytes(&self.nym).map_err(bbs_err)?;
        let secrets = guarded(|| {
            sig.verify_finalize_with_nym(
                &pk,
                Some(&issued.header),
                Some(&messages),
                Some(&[self.link_secret.to_vec()]),
                vec![nym],
                Some(&entropy),
                Some(&blind),
            )
        })?;
        let nym_secret = secrets.first().ok_or(Error::BadSignature)?.to_bytes();
        Ok(Credential {
            key: key.clone(),
            header,
            header_bytes: issued.header.clone(),
            attributes: issued.attributes.clone(),
            signature: issued.signature,
            link_secret: self.link_secret,
            nym_secret: Zeroizing::new(nym_secret),
            blind: self.blind,
            receipt: issued.receipt.clone(),
        })
    }
}

/// What an issuer sends back for a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issued {
    pub header: Vec<u8>,
    pub attributes: Profile,
    pub signature: [u8; SIGNATURE_LEN],
    pub nym_entropy: [u8; 32],
    pub receipt: Vec<u8>,
}

fn encode_attrs(
    e: &mut const_cbor::Encoder<'_>,
    attrs: &Profile,
) -> core::result::Result<(), const_cbor::Error> {
    e.array_len(attrs.len())?;
    for (k, v) in attrs {
        e.array_len(2)?.str(k)?.str(v)?;
    }
    Ok(())
}

fn decode_attrs(d: &mut Decoder<'_>) -> Result<Profile> {
    let n = d.array_len()?;
    if n > crate::persona::MAX_ATTRIBUTES {
        return Err(Error::Malformed("too many attributes"));
    }
    let mut out = Vec::new();
    for _ in 0..n {
        if d.array_len()? != 2 {
            return Err(Error::Malformed("attribute"));
        }
        out.push((d.str()?.to_owned(), d.str()?.to_owned()));
    }
    check_profile(&out)?;
    Ok(out)
}

impl Issued {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(4096, |e| {
            e.map_len(5)?;
            e.u8(0)?.bytes(&self.header)?;
            e.u8(1)?;
            encode_attrs(e, &self.attributes)?;
            e.u8(2)?.bytes(&self.signature)?;
            e.u8(3)?.bytes(&self.nym_entropy)?;
            e.u8(4)?.bytes(&self.receipt)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut h, mut a, mut s, mut n, mut r) = (None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => h = Some(d.bytes()?.to_vec()),
                1 => a = Some(decode_attrs(d)?),
                2 => s = Some(fixed_bytes::<SIGNATURE_LEN>(d)?),
                3 => n = Some(fixed_bytes::<32>(d)?),
                4 => r = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            header: required(h, "header")?,
            attributes: required(a, "attributes")?,
            signature: required(s, "signature")?,
            nym_entropy: required(n, "nym entropy")?,
            receipt: required(r, "receipt")?,
        })
    }
}

/// A credential held by its holder, with the secrets that prove it.
pub struct Credential {
    pub key: IssuerKey,
    pub header: Header,
    header_bytes: Vec<u8>,
    pub attributes: Profile,
    signature: [u8; SIGNATURE_LEN],
    link_secret: Zeroizing<[u8; 32]>,
    nym_secret: Zeroizing<[u8; 32]>,
    blind: Zeroizing<[u8; 32]>,
    /// The issuer's ML-DSA signature over the issuance, kept as evidence.
    pub receipt: Vec<u8>,
}

impl Credential {
    /// A stable id for this credential (from its signature).
    pub fn id(&self) -> u64 {
        let h = blake3::hash(&self.signature);
        u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or_default())
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        self.header.expires_day < day(now_ms)
    }

    /// Proves the attributes with keys in `disclose` (and nothing else) for
    /// the verifier context `context`, bound to `binding` (a session or
    /// circuit value, so the proof can't be replayed elsewhere).
    pub fn present(
        &self,
        disclose: &[&str],
        context: &[u8],
        binding: &[u8],
    ) -> Result<Presentation> {
        let messages = attribute_messages(&self.attributes)?;
        let indexes: Vec<usize> = self
            .attributes
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| disclose.contains(&k.as_str()))
            .map(|(i, _)| i)
            .collect();
        let pk =
            BBSplusPublicKey::from_bytes(&self.key.bbs_public).map_err(|_| Error::InvalidKey)?;
        let nym = PseudonymSecret::from_bytes(&self.nym_secret).map_err(bbs_err)?;
        let blind = BlindFactor::from_bytes(&self.blind).map_err(bbs_err)?;
        let (proof, pseudonym) = guarded(|| {
            PoKSignature::<Bbs>::proof_gen_with_nym(
                &pk,
                &self.signature,
                Some(&self.header_bytes),
                Some(binding),
                &vec![nym],
                context,
                Some(&messages),
                Some(&[self.link_secret.to_vec()]),
                Some(&indexes),
                Some(&[]),
                Some(&blind),
            )
        })?;
        let pseudonym: [u8; PSEUDONYM_LEN] = pseudonym
            .to_bytes()
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        Ok(Presentation {
            issuer: self.key.id(),
            header: self.header_bytes.clone(),
            total: u16::try_from(self.attributes.len())
                .map_err(|_| Error::Malformed("attributes"))?,
            disclosed: indexes
                .iter()
                .map(|&i| {
                    let (k, v) = &self.attributes[i];
                    (i as u16, k.clone(), v.clone())
                })
                .collect(),
            proof: proof.to_bytes(),
            pseudonym,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let key = self.key.encode()?;
        cbor::to_vec(12_000, |e| {
            e.map_len(8)?;
            e.u8(0)?.bytes(&key)?;
            e.u8(1)?.bytes(&self.header_bytes)?;
            e.u8(2)?;
            encode_attrs(e, &self.attributes)?;
            e.u8(3)?.bytes(&self.signature)?;
            e.u8(4)?.bytes(&self.link_secret[..])?;
            e.u8(5)?.bytes(&self.nym_secret[..])?;
            e.u8(6)?.bytes(&self.blind[..])?;
            e.u8(7)?.bytes(&self.receipt)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut key, mut h, mut a, mut s, mut l, mut n, mut bl, mut r) =
            (None, None, None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => key = Some(IssuerKey::decode(d.bytes()?)?),
                1 => h = Some(d.bytes()?.to_vec()),
                2 => a = Some(decode_attrs(d)?),
                3 => s = Some(fixed_bytes::<SIGNATURE_LEN>(d)?),
                4 => l = Some(Zeroizing::new(fixed_bytes::<32>(d)?)),
                5 => n = Some(Zeroizing::new(fixed_bytes::<32>(d)?)),
                6 => bl = Some(Zeroizing::new(fixed_bytes::<32>(d)?)),
                7 => r = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let header_bytes = required(h, "header")?;
        Ok(Self {
            key: required(key, "issuer key")?,
            header: Header::decode(&header_bytes)?,
            header_bytes,
            attributes: required(a, "attributes")?,
            signature: required(s, "signature")?,
            link_secret: required(l, "link secret")?,
            nym_secret: required(n, "pseudonym secret")?,
            blind: required(bl, "blind factor")?,
            receipt: required(r, "receipt")?,
        })
    }
}

// ----- Verifier side -----

/// A proof of some of a credential's attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub issuer: [u8; 32],
    pub header: Vec<u8>,
    /// How many attributes the credential has (the hidden ones included).
    pub total: u16,
    pub disclosed: Vec<(u16, String, String)>,
    pub proof: Vec<u8>,
    pub pseudonym: [u8; PSEUDONYM_LEN],
}

/// What a valid presentation shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub issuer: PublicIdentity,
    pub schema: String,
    pub expires_day: u32,
    pub attributes: Profile,
    /// The same for every presentation of one credential in one context.
    pub pseudonym: [u8; PSEUDONYM_LEN],
}

impl Presentation {
    /// Verifies the proof against `key` (which must be the issuer the
    /// presentation names) for `context` and `binding`, and that the
    /// credential hasn't expired.
    pub fn verify(
        &self,
        key: &IssuerKey,
        context: &[u8],
        binding: &[u8],
        now_ms: u64,
    ) -> Result<Verified> {
        if self.issuer != key.id() {
            return Err(Error::Malformed("presentation names another issuer"));
        }
        let header = Header::decode(&self.header)?;
        if header.issuer != self.issuer {
            return Err(Error::Malformed("header names another issuer"));
        }
        if header.expires_day < day(now_ms) {
            return Err(Error::Malformed("credential expired"));
        }
        let total = usize::from(self.total);
        if total > crate::persona::MAX_ATTRIBUTES
            || self.disclosed.len() > total
            || self.disclosed.windows(2).any(|w| w[0].0 >= w[1].0)
            || self
                .disclosed
                .iter()
                .any(|(i, _, _)| usize::from(*i) >= total)
        {
            return Err(Error::Malformed("disclosed attributes"));
        }
        let attributes: Profile = self
            .disclosed
            .iter()
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect();
        check_profile(&attributes)?;
        let messages: Vec<Vec<u8>> = self
            .disclosed
            .iter()
            .map(|(_, k, v)| attribute_message(k, v))
            .collect::<Result<_>>()?;
        let indexes: Vec<usize> = self
            .disclosed
            .iter()
            .map(|(i, _, _)| usize::from(*i))
            .collect();
        if self.proof.len() != proof_len(total, indexes.len()) {
            return Err(Error::Malformed("proof length"));
        }
        let pk = BBSplusPublicKey::from_bytes(&key.bbs_public).map_err(|_| Error::InvalidKey)?;
        guarded(|| {
            let proof = PoKSignature::<Bbs>::from_bytes(&self.proof)?;
            let nym = BBSplusPseudonym::from_bytes(&self.pseudonym)?;
            proof.proof_verify_with_nym(
                &pk,
                Some(&self.header),
                Some(binding),
                &nym,
                context,
                NYMS,
                Some(total),
                Some(&messages),
                Some(&[]),
                Some(&indexes),
                Some(&[]),
            )
        })?;
        Ok(Verified {
            issuer: key.identity,
            schema: header.schema,
            expires_day: header.expires_day,
            attributes,
            pseudonym: self.pseudonym,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(1024 + self.proof.len(), |e| {
            e.map_len(6)?;
            e.u8(0)?.bytes(&self.issuer)?;
            e.u8(1)?.bytes(&self.header)?;
            e.u8(2)?.u16(self.total)?;
            e.u8(3)?.array_len(self.disclosed.len())?;
            for (i, k, v) in &self.disclosed {
                e.array_len(3)?.u16(*i)?.str(k)?.str(v)?;
            }
            e.u8(4)?.bytes(&self.proof)?;
            e.u8(5)?.bytes(&self.pseudonym)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut is, mut h, mut t, mut dis, mut p, mut n) = (None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => is = Some(fixed_bytes::<32>(d)?),
                1 => {
                    let b = d.bytes()?;
                    if b.len() > 256 {
                        return Err(Error::Malformed("header too long"));
                    }
                    h = Some(b.to_vec());
                }
                2 => t = Some(d.u16()?),
                3 => {
                    let len = d.array_len()?;
                    if len > crate::persona::MAX_ATTRIBUTES {
                        return Err(Error::Malformed("too many disclosed attributes"));
                    }
                    let mut v = Vec::new();
                    for _ in 0..len {
                        if d.array_len()? != 3 {
                            return Err(Error::Malformed("disclosed attribute"));
                        }
                        v.push((d.u16()?, d.str()?.to_owned(), d.str()?.to_owned()));
                    }
                    dis = Some(v);
                }
                4 => {
                    let b = d.bytes()?;
                    if b.len() > MAX_PROOF {
                        return Err(Error::Malformed("proof too long"));
                    }
                    p = Some(b.to_vec());
                }
                5 => n = Some(fixed_bytes::<PSEUDONYM_LEN>(d)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            issuer: required(is, "issuer id")?,
            header: required(h, "header")?,
            total: required(t, "attribute count")?,
            disclosed: required(dis, "disclosed attributes")?,
            proof: required(p, "proof")?,
            pseudonym: required(n, "pseudonym")?,
        })
    }
}

// ----- Peer-to-peer protocol -----

/// Credential messages between peers (`AppMessage::Credential`).
///
/// ```text
/// CredMsg = { 0: op, 1: id u64, ? 2: issuer key, ? 3: schema, ? 4: [* attribute],
///             ? 5: expires_day, ? 6: commitment, ? 7: Issued, ? 8: nonce (32),
///             ? 9: [* key tstr], ? 10: Presentation }
/// op: 1 offer, 2 request, 3 issued, 4 decline, 5 ask, 6 proof
/// ```
///
/// An issuer offers a credential (1); the holder answers with a request
/// carrying its commitment (2), and the issuer signs it (3). A verifier
/// asks for attributes of a schema (5), and the holder answers with a
/// presentation (6). Either side may decline (4). `id` ties the steps of
/// one exchange together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredMsg {
    Offer {
        id: u64,
        key: Vec<u8>,
        schema: String,
        attributes: Profile,
        expires_day: u32,
    },
    Request {
        id: u64,
        commitment: Vec<u8>,
    },
    Issued {
        id: u64,
        issued: Vec<u8>,
    },
    Decline {
        id: u64,
    },
    Ask {
        id: u64,
        schema: String,
        keys: Vec<String>,
        nonce: [u8; 32],
    },
    Proof {
        id: u64,
        key: Vec<u8>,
        presentation: Vec<u8>,
    },
}

/// Largest commitment a request may carry (one committed secret, one nym).
const MAX_COMMITMENT: usize = 512;

impl CredMsg {
    pub fn id(&self) -> u64 {
        match self {
            Self::Offer { id, .. }
            | Self::Request { id, .. }
            | Self::Issued { id, .. }
            | Self::Decline { id }
            | Self::Ask { id, .. }
            | Self::Proof { id, .. } => *id,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(8192, |e| {
            match self {
                Self::Offer {
                    id,
                    key,
                    schema,
                    attributes,
                    expires_day,
                } => {
                    e.map_len(6)?.u8(0)?.u8(1)?.u8(1)?.u64(*id)?;
                    e.u8(2)?.bytes(key)?;
                    e.u8(3)?.str(schema)?;
                    e.u8(4)?;
                    encode_attrs(e, attributes)?;
                    e.u8(5)?.u32(*expires_day)?;
                }
                Self::Request { id, commitment } => {
                    e.map_len(3)?.u8(0)?.u8(2)?.u8(1)?.u64(*id)?;
                    e.u8(6)?.bytes(commitment)?;
                }
                Self::Issued { id, issued } => {
                    e.map_len(3)?.u8(0)?.u8(3)?.u8(1)?.u64(*id)?;
                    e.u8(7)?.bytes(issued)?;
                }
                Self::Decline { id } => {
                    e.map_len(2)?.u8(0)?.u8(4)?.u8(1)?.u64(*id)?;
                }
                Self::Ask {
                    id,
                    schema,
                    keys,
                    nonce,
                } => {
                    e.map_len(5)?.u8(0)?.u8(5)?.u8(1)?.u64(*id)?;
                    e.u8(3)?.str(schema)?;
                    e.u8(8)?.bytes(nonce)?;
                    e.u8(9)?.array_len(keys.len())?;
                    for k in keys {
                        e.str(k)?;
                    }
                }
                Self::Proof {
                    id,
                    key,
                    presentation,
                } => {
                    e.map_len(4)?.u8(0)?.u8(6)?.u8(1)?.u64(*id)?;
                    e.u8(2)?.bytes(key)?;
                    e.u8(10)?.bytes(presentation)?;
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut op = None;
        let mut id = None;
        let (mut key, mut schema, mut attrs, mut exp, mut commit, mut issued) =
            (None, None, None, None, None, None);
        let (mut nonce, mut keys, mut pres) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => id = Some(d.u64()?),
                2 => key = Some(d.bytes()?.to_vec()),
                3 => {
                    let s = d.str()?.to_owned();
                    check_schema(&s)?;
                    schema = Some(s);
                }
                4 => attrs = Some(decode_attrs(d)?),
                5 => exp = Some(d.u32()?),
                6 => {
                    let c = d.bytes()?;
                    if c.len() > MAX_COMMITMENT {
                        return Err(Error::Malformed("commitment too long"));
                    }
                    commit = Some(c.to_vec());
                }
                7 => issued = Some(d.bytes()?.to_vec()),
                8 => nonce = Some(fixed_bytes::<32>(d)?),
                9 => {
                    let n = d.array_len()?;
                    if n > crate::persona::MAX_ATTRIBUTES {
                        return Err(Error::Malformed("too many keys"));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(d.str()?.to_owned());
                    }
                    keys = Some(v);
                }
                10 => pres = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let id = required(id, "exchange id")?;
        Ok(match required(op, "credential op")? {
            1 => Self::Offer {
                id,
                key: required(key, "issuer key")?,
                schema: required(schema, "schema")?,
                attributes: required(attrs, "attributes")?,
                expires_day: required(exp, "expiry")?,
            },
            2 => Self::Request {
                id,
                commitment: required(commit, "commitment")?,
            },
            3 => Self::Issued {
                id,
                issued: required(issued, "issued credential")?,
            },
            4 => Self::Decline { id },
            5 => Self::Ask {
                id,
                schema: required(schema, "schema")?,
                keys: keys.unwrap_or_default(),
                nonce: required(nonce, "nonce")?,
            },
            6 => Self::Proof {
                id,
                key: required(key, "issuer key")?,
                presentation: required(pres, "presentation")?,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

// ----- Bindings and contexts -----

/// Binds a peer-to-peer presentation to one session and one request.
pub fn presentation_binding(session_id: &[u8], nonce: &[u8; 32]) -> [u8; 32] {
    derive(label::PRESENTATION, &[session_id, nonce])
}

/// The verifier context of peer presentations: each verifier sees its own
/// stable pseudonym for a credential, and no two verifiers can link theirs.
pub fn peer_context(verifier: &PublicIdentity) -> Vec<u8> {
    [b"threnody peer nym v1".as_slice(), verifier.as_bytes()].concat()
}

/// The context of a relay token: one relay, one day, one of
/// [`RELAY_SLOTS`] slots. A pseudonym repeats only if a slot is reused,
/// which is how a relay limits a credential to `RELAY_SLOTS` circuits a day
/// without learning anything else about it.
pub fn relay_context(relay: &PublicIdentity, epoch: u32, slot: u16) -> Vec<u8> {
    [
        b"threnody relay token v1".as_slice(),
        relay.as_bytes(),
        &epoch.to_le_bytes(),
        &slot.to_le_bytes(),
    ]
    .concat()
}

/// Binds a relay token to the circuit it opens (the hop's ephemeral key).
pub fn relay_binding(e_pub: &[u8]) -> [u8; 32] {
    derive(label::RELAY_TOKEN, &[e_pub])
}

/// A relay token: a presentation of a relay-access credential (no
/// attributes) for one slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayToken {
    pub epoch: u32,
    pub slot: u16,
    pub presentation: Presentation,
}

impl RelayToken {
    pub fn new(
        cred: &Credential,
        relay: &PublicIdentity,
        epoch: u32,
        slot: u16,
        e_pub: &[u8],
    ) -> Result<Self> {
        if cred.header.schema != RELAY_SCHEMA || slot >= RELAY_SLOTS {
            return Err(Error::Malformed("not a relay credential"));
        }
        Ok(Self {
            epoch,
            slot,
            presentation: cred.present(
                &[],
                &relay_context(relay, epoch, slot),
                &relay_binding(e_pub),
            )?,
        })
    }

    /// Checks a token for this relay (`me`) and this circuit (`e_pub`),
    /// issued by one of `issuers`, for today or yesterday. Returns the
    /// pseudonym, which the relay must not have seen in this epoch.
    pub fn verify(
        &self,
        issuers: &[IssuerKey],
        me: &PublicIdentity,
        e_pub: &[u8],
        now_ms: u64,
    ) -> Result<[u8; PSEUDONYM_LEN]> {
        let today = day(now_ms);
        if self.slot >= RELAY_SLOTS || !(self.epoch == today || self.epoch + 1 == today) {
            return Err(Error::Malformed("relay token epoch or slot"));
        }
        let key = issuers
            .iter()
            .find(|k| k.id() == self.presentation.issuer)
            .ok_or(Error::Malformed("relay token from an unknown directory"))?;
        let v = self.presentation.verify(
            key,
            &relay_context(me, self.epoch, self.slot),
            &relay_binding(e_pub),
            now_ms,
        )?;
        if v.schema != RELAY_SCHEMA || v.expires_day < self.epoch {
            return Err(Error::Malformed("not a relay token"));
        }
        Ok(v.pseudonym)
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let p = self.presentation.encode()?;
        cbor::to_vec(p.len() + 32, |e| {
            e.map_len(3)?
                .u8(0)?
                .u32(self.epoch)?
                .u8(1)?
                .u16(self.slot)?
                .u8(2)?
                .bytes(&p)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut ep, mut sl, mut p) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => ep = Some(d.u32()?),
                1 => sl = Some(d.u16()?),
                2 => p = Some(Presentation::decode(d.bytes()?)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            epoch: required(ep, "epoch")?,
            slot: required(sl, "slot")?,
            presentation: required(p, "presentation")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> Profile {
        vec![
            ("name".into(), "Ada".into()),
            ("member".into(), "hackspace".into()),
            ("over18".into(), "yes".into()),
        ]
    }

    fn issued(issuer: &Issuer, schema: &str, a: &Profile) -> Credential {
        let req = Request::new().unwrap();
        let issued = issuer
            .issue(&req.commitment, schema, a, day(crate::now_ms()) + 30)
            .unwrap();
        let wire = Issued::decode(&issued.encode().unwrap()).unwrap();
        req.finish(issuer.key(), &wire).unwrap()
    }

    #[test]
    fn issuer_keys_are_deterministic_and_verify() {
        let id = Identity::generate();
        let a = Issuer::new(&id, 0).unwrap();
        let b = Issuer::new(&id, 0).unwrap();
        assert_eq!(a.key(), b.key());
        assert_ne!(a.key().id(), Issuer::new(&id, 1).unwrap().key().id());
        let bytes = a.key().encode().unwrap();
        assert_eq!(&IssuerKey::decode(&bytes).unwrap(), a.key());
        // A key whose ML-DSA half was swapped no longer verifies.
        let mut forged = a.key().clone();
        forged.pq_public = Issuer::new(&Identity::generate(), 0)
            .unwrap()
            .key()
            .pq_public
            .clone();
        assert!(forged.verify().is_err());
    }

    #[test]
    fn selective_disclosure_round_trip() {
        let issuer = Issuer::new(&Identity::generate(), 0).unwrap();
        let cred = Credential::decode(
            &issued(&issuer, "example/member/1", &attrs())
                .encode()
                .unwrap(),
        )
        .unwrap();
        let verifier = Identity::generate().public();
        let ctx = peer_context(&verifier);
        let binding = presentation_binding(b"session", &[9; 32]);
        let p = cred.present(&["over18"], &ctx, &binding).unwrap();
        let p = Presentation::decode(&p.encode().unwrap()).unwrap();
        let v = p
            .verify(issuer.key(), &ctx, &binding, crate::now_ms())
            .unwrap();
        assert_eq!(v.attributes, vec![("over18".to_owned(), "yes".to_owned())]);
        assert_eq!(v.schema, "example/member/1");
        // Another binding, context or issuer fails.
        assert!(
            p.verify(issuer.key(), &ctx, &[0; 32], crate::now_ms())
                .is_err()
        );
        assert!(
            p.verify(
                issuer.key(),
                &peer_context(&Identity::generate().public()),
                &binding,
                crate::now_ms()
            )
            .is_err()
        );
        let other = Issuer::new(&Identity::generate(), 0).unwrap();
        assert!(
            p.verify(other.key(), &ctx, &binding, crate::now_ms())
                .is_err()
        );
        // Changing a disclosed value fails.
        let mut lie = p.clone();
        lie.disclosed[0].2 = "no".into();
        assert!(
            lie.verify(issuer.key(), &ctx, &binding, crate::now_ms())
                .is_err()
        );
        // Claiming a hidden attribute's index for another attribute fails.
        let mut moved = p.clone();
        moved.disclosed[0].0 = 0;
        assert!(
            moved
                .verify(issuer.key(), &ctx, &binding, crate::now_ms())
                .is_err()
        );
        // Every proof has exactly the length the verifier expects.
        for keys in [&[][..], &["name"][..], &["name", "member", "over18"][..]] {
            let p = cred.present(keys, &ctx, &binding).unwrap();
            assert_eq!(p.proof.len(), proof_len(3, keys.len()));
        }
        // Same verifier, same pseudonym; another verifier, another one.
        let p2 = cred.present(&[], &ctx, &[1; 32]).unwrap();
        assert_eq!(p2.pseudonym, p.pseudonym);
        let p3 = cred
            .present(&[], &peer_context(&Identity::generate().public()), &[1; 32])
            .unwrap();
        assert_ne!(p3.pseudonym, p.pseudonym);
        // Two presentations to one verifier share only the pseudonym.
        assert_ne!(p2.proof, p.proof);
    }

    #[test]
    fn tampered_issuance_is_refused() {
        let issuer = Issuer::new(&Identity::generate(), 0).unwrap();
        let req = Request::new().unwrap();
        let mut iss = issuer
            .issue(&req.commitment, "s", &attrs(), 99_999)
            .unwrap();
        iss.attributes[0].1 = "Eve".into();
        assert!(req.finish(issuer.key(), &iss).is_err());
        // A receipt for someone else's commitment doesn't fit ours.
        let req = Request::new().unwrap();
        let other = Request::new().unwrap();
        let iss = issuer
            .issue(&other.commitment, "s", &attrs(), 99_999)
            .unwrap();
        assert!(req.finish(issuer.key(), &iss).is_err());
        // A garbage commitment is refused by the issuer.
        assert!(issuer.issue(&[1, 2, 3], "s", &attrs(), 99_999).is_err());
    }

    #[test]
    fn expired_credentials_fail() {
        let issuer = Issuer::new(&Identity::generate(), 0).unwrap();
        let req = Request::new().unwrap();
        let iss = issuer.issue(&req.commitment, "s", &attrs(), 1).unwrap();
        let cred = req.finish(issuer.key(), &iss).unwrap();
        assert!(cred.expired(crate::now_ms()));
        let ctx = peer_context(&Identity::generate().public());
        let p = cred.present(&[], &ctx, &[0; 32]).unwrap();
        assert!(
            p.verify(issuer.key(), &ctx, &[0; 32], crate::now_ms())
                .is_err()
        );
    }

    #[test]
    fn relay_tokens_limit_slots_and_bind_circuits() {
        let dir = Issuer::new(&Identity::generate(), 0).unwrap();
        let now = crate::now_ms();
        let req = Request::new().unwrap();
        let iss = dir
            .issue(&req.commitment, RELAY_SCHEMA, &vec![], day(now) + 1)
            .unwrap();
        let cred = req.finish(dir.key(), &iss).unwrap();
        let relay = Identity::generate().public();
        let keys = vec![dir.key().clone()];
        let e_pub = [5u8; 64];
        let t0 = RelayToken::new(&cred, &relay, day(now), 0, &e_pub).unwrap();
        let t0 = RelayToken::decode(&t0.encode().unwrap()).unwrap();
        let n0 = t0.verify(&keys, &relay, &e_pub, now).unwrap();
        // The same slot again shows the same pseudonym: the relay refuses it.
        let again = RelayToken::new(&cred, &relay, day(now), 0, &[6u8; 64]).unwrap();
        assert_eq!(again.verify(&keys, &relay, &[6u8; 64], now).unwrap(), n0);
        // Another slot is unlinkable.
        let t1 = RelayToken::new(&cred, &relay, day(now), 1, &e_pub).unwrap();
        assert_ne!(t1.verify(&keys, &relay, &e_pub, now).unwrap(), n0);
        // Bound to the circuit, the relay and the directories it trusts.
        assert!(t0.verify(&keys, &relay, &[7u8; 64], now).is_err());
        assert!(
            t0.verify(&keys, &Identity::generate().public(), &e_pub, now)
                .is_err()
        );
        assert!(t0.verify(&[], &relay, &e_pub, now).is_err());
        // Out-of-range slots and stale epochs are refused.
        assert!(RelayToken::new(&cred, &relay, day(now), RELAY_SLOTS, &e_pub).is_err());
        let old = RelayToken::new(&cred, &relay, day(now) - 2, 0, &e_pub).unwrap();
        assert!(old.verify(&keys, &relay, &e_pub, now).is_err());
        // A peer credential isn't a relay token.
        let peer = issued(&dir, "example/member/1", &attrs());
        assert!(RelayToken::new(&peer, &relay, day(now), 0, &e_pub).is_err());
        // Small enough to ride in an EXTEND cell beside an X-Wing key.
        assert!(t0.encode().unwrap().len() < 700);
    }

    #[test]
    fn protocol_messages_round_trip() {
        let msgs = [
            CredMsg::Offer {
                id: 1,
                key: vec![1, 2],
                schema: "s".into(),
                attributes: attrs(),
                expires_day: 9,
            },
            CredMsg::Request {
                id: 2,
                commitment: vec![3; 100],
            },
            CredMsg::Issued {
                id: 3,
                issued: vec![4],
            },
            CredMsg::Decline { id: 4 },
            CredMsg::Ask {
                id: 5,
                schema: "s".into(),
                keys: vec!["name".into()],
                nonce: [6; 32],
            },
            CredMsg::Proof {
                id: 6,
                key: vec![7],
                presentation: vec![8],
            },
        ];
        for m in msgs {
            assert_eq!(CredMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(CredMsg::decode(&[0xa0]).is_err());
    }
}
