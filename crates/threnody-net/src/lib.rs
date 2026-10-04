//! Transports and the session driver for Threnody.
//!
//! Every transport carries only handshake frames and ratchet ciphertext
//! (spec §7.1: cleartext is forbidden on any transport). TCP over IP is the
//! first backend; the [`node::Node`] session driver is generic over any
//! reliable byte stream so Bluetooth and Wi-Fi Direct sockets slot in later.

pub mod account;
pub mod discovery;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod history;
pub mod mailbox;
pub mod node;
pub mod onion;
pub mod relay;

pub use discovery::DiscoveryConfig;
pub use error::{NetError, Result};
pub use node::{AcceptPolicy, Event, Node, NodeConfig, SessionInfo};
