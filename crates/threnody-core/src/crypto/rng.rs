//! Randomness source for protocol state machines.
//!
//! Production code always uses the OS CSPRNG. Tests can substitute a seeded
//! ChaCha20 generator so complete handshakes and ratchet runs are
//! reproducible, which is what the published test vectors are made from.

use rand_core::{CryptoRng, OsRng, RngCore};

#[derive(Clone, Default)]
pub enum Rng {
    #[default]
    Os,
    #[cfg(test)]
    Seeded(Box<rand_chacha::ChaCha20Rng>),
}

impl Rng {
    /// Deterministic generator for test vectors. Never available outside tests.
    #[cfg(test)]
    pub fn seeded(seed: [u8; 32]) -> Self {
        use rand_core::SeedableRng;
        Self::Seeded(Box::new(rand_chacha::ChaCha20Rng::from_seed(seed)))
    }
}

impl RngCore for Rng {
    fn next_u32(&mut self) -> u32 {
        match self {
            Self::Os => OsRng.next_u32(),
            #[cfg(test)]
            Self::Seeded(r) => r.next_u32(),
        }
    }

    fn next_u64(&mut self) -> u64 {
        match self {
            Self::Os => OsRng.next_u64(),
            #[cfg(test)]
            Self::Seeded(r) => r.next_u64(),
        }
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        match self {
            Self::Os => OsRng.fill_bytes(dest),
            #[cfg(test)]
            Self::Seeded(r) => r.fill_bytes(dest),
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        match self {
            Self::Os => OsRng.try_fill_bytes(dest),
            #[cfg(test)]
            Self::Seeded(r) => r.try_fill_bytes(dest),
        }
    }
}

impl CryptoRng for Rng {}
