//! Cryptographic building blocks. Nothing here is novel: the module only
//! composes RustCrypto / dalek / BLAKE3 primitives with explicit domain
//! separation. The BBS credentials in `crate::credential` use `zkryptium`,
//! which follows IETF drafts and has not been audited.

pub mod aead;
pub mod hybrid;
pub mod kdf;
pub mod pqsig;
pub mod rng;

/// Returns `N` bytes from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    use getrandom::SysRng;
    use rand_core::{Rng as RandRng, UnwrapErr};
    let mut b = [0u8; N];
    UnwrapErr(SysRng).fill_bytes(&mut b);
    b
}
