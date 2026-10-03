//! Threnody protocol core.
//!
//! Sans-IO implementation of the Threnody specification v0.1.0: identities,
//! the hybrid post-quantum handshake, the hybrid double ratchet, the CBOR
//! wire format and local persistence. Transports live in `threnody-net`.

pub mod cbor;
pub mod channel;
pub mod crypto;
pub mod discovery;
pub mod error;
pub mod handshake;
pub mod identity;
pub mod message;
pub mod onion;
pub mod prekey;
pub mod ratchet;
#[cfg(test)]
mod robustness;
pub mod sealed;
pub mod store;
pub mod tunnel;
#[cfg(test)]
mod vectors;
pub mod wire;

pub use channel::SecureChannel;
pub use error::{Error, Result};
pub use identity::{Fingerprint, Identity, PublicIdentity, safety_number};
pub use message::AppMessage;

/// Milliseconds since the Unix epoch, saturating at zero for clocks set
/// before 1970.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
