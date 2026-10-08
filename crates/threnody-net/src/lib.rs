//! Transports and the session driver for Threnody.
//!
//! Every transport carries only handshake frames and ratchet ciphertext
//! (spec §7.1: cleartext is forbidden on any transport). TCP over IP is the
//! first backend; the [`node::Node`] session driver is generic over any
//! reliable byte stream, so Bluetooth sockets and Wi-Fi Direct links slot in.

pub mod account;
mod anon;
pub mod cred;
pub mod delivery;
pub mod direct;
pub mod discovery;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod history;
pub mod identity;
pub mod mailbox;
pub mod node;
pub mod onion;
pub(crate) mod portmap;
mod quic;
pub mod reach;
mod recover;
pub mod relay;
pub mod sync;
pub mod volunteer;
#[cfg(feature = "boringtun")]
pub mod wg_userspace;

pub use cred::{CredentialAsk, CredentialInfo, CredentialOffer};
pub use delivery::Tag;
pub use direct::DirectOffer;
pub use discovery::DiscoveryConfig;
pub use error::{NetError, Result};
pub use node::{AcceptPolicy, Event, Node, NodeConfig, SessionInfo};
pub use volunteer::{DirectoryInfo, ListedRelay, TestPay};
#[cfg(feature = "boringtun")]
pub use wg_userspace::{WgUserspace, WgPeerConfig};
