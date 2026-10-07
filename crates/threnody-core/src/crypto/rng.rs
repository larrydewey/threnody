//! Randomness source for protocol state machines.
//!
//! Production code always uses the OS CSPRNG. Tests can substitute a seeded
//! ChaCha20 generator so complete handshakes and ratchet runs are
//! reproducible, which is what the published test vectors are made from.

use getrandom::SysRng;
use rand_core::{TryCryptoRng, TryRng, UnwrapErr};

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

impl TryRng for Rng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        use rand_core::Rng;
        match self {
            Self::Os => Ok(UnwrapErr(SysRng).next_u32()),
            #[cfg(test)]
            Self::Seeded(r) => Ok(r.next_u32()),
        }
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        use rand_core::Rng;
        match self {
            Self::Os => Ok(UnwrapErr(SysRng).next_u64()),
            #[cfg(test)]
            Self::Seeded(r) => Ok(r.next_u64()),
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        use rand_core::Rng;
        match self {
            Self::Os => {
                UnwrapErr(SysRng).fill_bytes(dest);
                Ok(())
            }
            #[cfg(test)]
            Self::Seeded(r) => {
                r.fill_bytes(dest);
                Ok(())
            }
        }
    }
}

impl TryCryptoRng for Rng {}
