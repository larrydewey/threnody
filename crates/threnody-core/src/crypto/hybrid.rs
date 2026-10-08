//! X-Wing hybrid KEM: X25519 + ML-KEM-768 (spec §3.2), exactly as in
//! draft-connolly-cfrg-xwing-kem-11 and checked against its test vectors.
//!
//! ```text
//! (d, z, sk_X)  = SHAKE256(seed, 96 bytes)
//! pk            = pk_M (1184) || pk_X (32)
//! ct            = ct_M (1088) || ct_X (32)
//! ss            = SHA3-256(ss_M || ss_X || ct_X || pk_X || XWingLabel)
//! ```
//!
//! The 32-byte seed is the whole private key, which keeps persisted
//! ratchet state small.

use getrandom::SysRng;
use ml_kem::{B32, Ciphertext, Decapsulate, Key, KeyExport, MlKem768, Seed};
use rand_core::UnwrapErr;
use rand_core::{CryptoRng, Rng};
use sha3::digest::{ExtendableOutput, Update, XofReader};
use sha3::{Digest, Sha3_256};
use shake::Shake256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

type DecapKey = ml_kem::DecapsulationKey768;
type EncapKey = ml_kem::EncapsulationKey768;

pub const PK_M_LEN: usize = 1184;
pub const CT_M_LEN: usize = 1088;
pub const PUBLIC_LEN: usize = PK_M_LEN + 32;
pub const CIPHERTEXT_LEN: usize = CT_M_LEN + 32;
pub const SEED_LEN: usize = 32;

const XWING_LABEL: &[u8; 6] = b"\\.//^\\";

/// An X-Wing decapsulation key. Secrets zeroize on drop.
#[derive(Clone)]
pub struct HybridSecret {
    seed: Zeroizing<[u8; SEED_LEN]>,
    dk: DecapKey,
    x: StaticSecret,
    public: HybridPublic,
}

/// A validated X-Wing encapsulation key.
#[derive(Clone, PartialEq)]
pub struct HybridPublic {
    ek: EncapKey,
    x: PublicKey,
    bytes: Box<[u8; PUBLIC_LEN]>,
}

impl core::fmt::Debug for HybridPublic {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "HybridPublic({:02x?}..)",
            &self.bytes[PK_M_LEN..PK_M_LEN + 4]
        )
    }
}

impl HybridSecret {
    pub fn generate() -> Self {
        Self::generate_with(&mut UnwrapErr(SysRng))
    }

    /// Draws a 32-byte seed from `rng` (X-Wing `GenerateKeyPair`).
    pub fn generate_with<R: Rng + CryptoRng>(rng: &mut R) -> Self {
        let mut seed = Zeroizing::new([0u8; SEED_LEN]);
        rng.fill_bytes(&mut seed[..]);
        Self::from_seed(&seed)
    }

    /// X-Wing `expandDecapsulationKey`.
    pub fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let mut expanded = Zeroizing::new([0u8; 96]);
        let mut xof = Shake256::default();
        xof.update(seed);
        xof.finalize_xof().read(&mut expanded[..]);
        // FIPS 203 seed form: d || z.
        let dk = DecapKey::from_seed(Seed::try_from(&expanded[0..64]).expect("64-byte slice"));
        let ek = dk.encapsulation_key().clone();
        let sk_x: [u8; 32] = expanded[64..96].try_into().expect("32-byte slice");
        let x = StaticSecret::from(sk_x);
        let xp = PublicKey::from(&x);
        let mut bytes = Box::new([0u8; PUBLIC_LEN]);
        bytes[..PK_M_LEN].copy_from_slice(&ek.to_bytes());
        bytes[PK_M_LEN..].copy_from_slice(xp.as_bytes());
        Self {
            seed: Zeroizing::new(*seed),
            dk,
            x,
            public: HybridPublic { ek, x: xp, bytes },
        }
    }

    pub fn seed(&self) -> &[u8; SEED_LEN] {
        &self.seed
    }

    /// Returns the 32-byte seed for persistence.
    pub fn to_bytes(&self) -> [u8; SEED_LEN] {
        *self.seed
    }

    /// Reconstructs a HybridSecret from a 32-byte seed.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SEED_LEN {
            return Err(Error::Malformed("hybrid secret length"));
        }
        let mut seed = [0u8; SEED_LEN];
        seed.copy_from_slice(bytes);
        Ok(Self::from_seed(&seed))
    }

    pub fn public(&self) -> &HybridPublic {
        &self.public
    }

    pub fn decapsulate(&self, ct: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
        if ct.len() != CIPHERTEXT_LEN {
            return Err(Error::Malformed("hybrid ciphertext length"));
        }
        let (ct_m, ct_x) = ct.split_at(CT_M_LEN);
        let ct_m = Ciphertext::<MlKem768>::try_from(ct_m).map_err(|_| Error::InvalidKey)?;
        // ML-KEM decapsulation uses implicit rejection and never fails.
        let ss_m: Zeroizing<[u8; 32]> = Zeroizing::new(self.dk.decapsulate(&ct_m).into());
        let ct_x: [u8; 32] = ct_x.try_into().map_err(|_| Error::InvalidKey)?;
        let ss_x = self.x.diffie_hellman(&PublicKey::from(ct_x));
        if !ss_x.was_contributory() {
            return Err(Error::InvalidKey);
        }
        Ok(combine(
            &ss_m[..],
            ss_x.as_bytes(),
            &ct_x,
            self.public.x.as_bytes(),
        ))
    }
}

impl HybridPublic {
    /// Parses and validates a 1216-byte public key. The ML-KEM part gets the
    /// FIPS 203 §7.2 modulus check (decode/encode round trip).
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        let bytes: Box<[u8; PUBLIC_LEN]> = Box::new(b.try_into().map_err(|_| Error::InvalidKey)?);
        let ek_enc = Key::<EncapKey>::try_from(&b[..PK_M_LEN]).map_err(|_| Error::InvalidKey)?;
        let ek = EncapKey::new(&ek_enc).map_err(|_| Error::InvalidKey)?;
        // Belt and braces: the encoding must round-trip exactly.
        if ek.to_bytes() != ek_enc {
            return Err(Error::InvalidKey);
        }
        let xb: [u8; 32] = b[PK_M_LEN..].try_into().map_err(|_| Error::InvalidKey)?;
        Ok(Self {
            ek,
            x: PublicKey::from(xb),
            bytes,
        })
    }

    pub fn as_bytes(&self) -> &[u8; PUBLIC_LEN] {
        &self.bytes
    }

    /// Returns `(ciphertext, shared_secret)`.
    pub fn encapsulate(&self) -> Result<(Vec<u8>, Zeroizing<[u8; 32]>)> {
        self.encapsulate_with(&mut UnwrapErr(SysRng))
    }

    /// Draws a 64-byte `eseed` from `rng` (X-Wing `Encapsulate`).
    pub fn encapsulate_with<R: Rng + CryptoRng>(
        &self,
        rng: &mut R,
    ) -> Result<(Vec<u8>, Zeroizing<[u8; 32]>)> {
        let mut eseed = Zeroizing::new([0u8; 64]);
        rng.fill_bytes(&mut eseed[..]);
        self.encapsulate_derand(&eseed)
    }

    /// X-Wing `EncapsulateDerand`.
    pub fn encapsulate_derand(&self, eseed: &[u8; 64]) -> Result<(Vec<u8>, Zeroizing<[u8; 32]>)> {
        let m = B32::try_from(&eseed[..32]).expect("32-byte slice");
        let (ct_m, ss_m) = self.ek.encapsulate_deterministic(&m);
        let ss_m: Zeroizing<[u8; 32]> = Zeroizing::new(ss_m.into());
        let ek_x: [u8; 32] = eseed[32..].try_into().expect("32-byte slice");
        let eph = StaticSecret::from(ek_x);
        let ct_x = PublicKey::from(&eph);
        let ss_x = eph.diffie_hellman(&self.x);
        if !ss_x.was_contributory() {
            return Err(Error::InvalidKey);
        }
        let mut ct = Vec::with_capacity(CIPHERTEXT_LEN);
        ct.extend_from_slice(&ct_m);
        ct.extend_from_slice(ct_x.as_bytes());
        let ss = combine(
            &ss_m[..],
            ss_x.as_bytes(),
            ct_x.as_bytes(),
            self.x.as_bytes(),
        );
        Ok((ct, ss))
    }
}

fn combine(ss_m: &[u8], ss_x: &[u8; 32], ct_x: &[u8; 32], pk_x: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut h = Sha3_256::new();
    Digest::update(&mut h, ss_m);
    Digest::update(&mut h, ss_x);
    Digest::update(&mut h, ct_x);
    Digest::update(&mut h, pk_x);
    Digest::update(&mut h, XWING_LABEL);
    Zeroizing::new(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn matches_xwing_draft_vectors() {
        let text = include_str!("xwing-draft-11-vectors.txt");
        let mut count = 0;
        for block in text.split("\n\n").filter(|b| b.contains("seed ")) {
            let field = |name: &str| {
                let prefix = format!("{name} ");
                unhex(
                    block
                        .lines()
                        .find_map(|l| l.strip_prefix(prefix.as_str()))
                        .unwrap(),
                )
            };
            let seed: [u8; 32] = field("seed").try_into().unwrap();
            let eseed: [u8; 64] = field("eseed").try_into().unwrap();
            let sk = HybridSecret::from_seed(&seed);
            assert_eq!(&sk.public().as_bytes()[..], &field("pk")[..]);
            let (ct, ss) = sk.public().encapsulate_derand(&eseed).unwrap();
            assert_eq!(ct, field("ct"));
            assert_eq!(&ss[..], &field("ss")[..]);
            assert_eq!(&sk.decapsulate(&ct).unwrap()[..], &field("ss")[..]);
            count += 1;
        }
        assert_eq!(count, 3);
    }

    #[test]
    fn encapsulate_decapsulate_agree() {
        let sk = HybridSecret::generate();
        let pk = HybridPublic::from_bytes(sk.public().as_bytes()).unwrap();
        let (ct, ss) = pk.encapsulate().unwrap();
        assert_eq!(ct.len(), CIPHERTEXT_LEN);
        assert_eq!(*sk.decapsulate(&ct).unwrap(), *ss);
        assert_eq!(
            HybridSecret::from_seed(sk.seed()).public(),
            sk.public(),
            "seed must reproduce the key"
        );
    }

    #[test]
    fn tampered_ciphertext_changes_secret() {
        let sk = HybridSecret::generate();
        let (mut ct, ss) = sk.public().encapsulate().unwrap();
        ct[10] ^= 1;
        assert_ne!(*sk.decapsulate(&ct).unwrap(), *ss);
    }

    #[test]
    fn rejects_bad_lengths_and_low_order_points() {
        assert!(HybridPublic::from_bytes(&[0u8; 10]).is_err());
        let sk = HybridSecret::generate();
        let mut ct = vec![0u8; CIPHERTEXT_LEN];
        // all-zero X25519 point is low order: must be rejected
        ct[..CT_M_LEN].fill(1);
        assert!(sk.decapsulate(&ct).is_err());
    }

    #[test]
    fn rejects_non_canonical_mlkem_key() {
        let sk = HybridSecret::generate();
        let mut pk = sk.public().as_bytes().to_vec();
        // Coefficient 0xFFF exceeds q = 3329: fails the modulus check.
        pk[0] = 0xff;
        pk[1] |= 0x0f;
        assert!(HybridPublic::from_bytes(&pk).is_err());
    }
}
