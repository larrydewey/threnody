//! Cryptographic building blocks. Nothing here is novel: the module only
//! composes audited RustCrypto / dalek / BLAKE3 primitives with explicit
//! domain separation.

pub mod aead;
pub mod hybrid;
pub mod kdf;
pub mod rng;

/// Returns `N` bytes from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    use rand_core::{OsRng, RngCore};
    let mut b = [0u8; N];
    OsRng.fill_bytes(&mut b);
    b
}
