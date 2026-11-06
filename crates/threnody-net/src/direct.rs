//! Wi-Fi Direct link upgrades (`docs/appendix-l-wifi-direct.md`).
//!
//! Two devices already in a session (typically over Bluetooth) agree on a
//! faster link: one creates a Wi-Fi Direct group and sends its credentials
//! and address over the session; the other joins and dials, and the new
//! session replaces the old one. The credentials travel end-to-end
//! encrypted and authenticated, so no Wi-Fi Protected Setup prompt is
//! needed, and the dial pins the peer's fingerprint.
//!
//! ```text
//! DirectMsg = { 0: op, ? 1: ssid tstr, ? 2: passphrase tstr, ? 3: addr tstr }
//! op: 1 offer (all fields), 2 request (none)
//! ```
//!
//! Both are only sent to and honoured from mutually approved peers on
//! sessions over a nearby link: Bluetooth, or TCP/QUIC on a loopback,
//! private or link-local address. A relayed peer is not nearby, and nor
//! is one reached directly across the internet. Each peer gets at most one
//! request and one offer honoured per [`DIRECT_EVERY`].

use std::net::IpAddr;
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::{AppMessage, PublicIdentity};

use crate::error::{NetError, Result};
use crate::node::{Event, Node, SessionInfo, lock};

/// Longest SSID (IEEE 802.11) and WPA2 passphrase.
const MAX_SSID: usize = 32;
const MAX_PASSPHRASE: usize = 63;
const MAX_ADDR: usize = 64;

/// The most often a peer's requests (or offers) are honoured: each makes
/// a phone host a new group, or a laptop leave its network to join one.
pub(crate) const DIRECT_EVERY: Duration = Duration::from_secs(30);

/// A Wi-Fi Direct group to join.
#[derive(Clone, PartialEq, Eq)]
pub struct DirectOffer {
    pub ssid: String,
    pub passphrase: String,
    /// Where the offering node listens inside the group (`ip:port`).
    pub addr: String,
}

impl std::fmt::Debug for DirectOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectOffer")
            .field("ssid", &self.ssid)
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DirectMsg {
    Offer(DirectOffer),
    Request,
}

impl DirectMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(256, |e| {
            match self {
                Self::Offer(o) => {
                    e.map_len(4)?.u8(0)?.u8(1)?;
                    e.u8(1)?.str(&o.ssid)?;
                    e.u8(2)?.str(&o.passphrase)?;
                    e.u8(3)?.str(&o.addr)?;
                }
                Self::Request => {
                    e.map_len(1)?.u8(0)?.u8(2)?;
                }
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut ssid, mut pass, mut addr) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => ssid = Some(d.str()?.to_owned()),
                2 => pass = Some(d.str()?.to_owned()),
                3 => addr = Some(d.str()?.to_owned()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let bad = threnody_core::Error::Malformed;
        match required(op, "direct op")? {
            1 => {
                let o = DirectOffer {
                    ssid: required(ssid, "ssid")?,
                    passphrase: required(pass, "passphrase")?,
                    addr: required(addr, "address")?,
                };
                if o.ssid.is_empty() || o.ssid.len() > MAX_SSID {
                    return Err(bad("ssid length"));
                }
                if !(8..=MAX_PASSPHRASE).contains(&o.passphrase.len()) {
                    return Err(bad("passphrase length"));
                }
                if o.addr.len() > MAX_ADDR || o.addr.parse::<std::net::SocketAddr>().is_err() {
                    return Err(bad("group address"));
                }
                Ok(Self::Offer(o))
            }
            2 => Ok(Self::Request),
            _ => Err(bad("direct op")),
        }
    }
}

/// Whether a session runs over a link only a nearby device has.
fn nearby(s: &SessionInfo) -> bool {
    nearby_link(s.transport, s.via.is_some(), s.addr.ip())
}

fn nearby_link(transport: &str, relayed: bool, ip: IpAddr) -> bool {
    if relayed {
        return false;
    }
    match transport {
        // Dialed, accepted or punched: nearby only on a local network.
        "tcp" | "quic" => match ip.to_canonical() {
            IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
            IpAddr::V6(v6) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
        },
        // A stream over a radio link (Bluetooth LE).
        _ => true,
    }
}

impl Node {
    /// Whether `peer` is mutually approved and connected over a nearby
    /// link, i.e. nearby and trusted with our link credentials. Never for
    /// an anonymous identity (Appendix M): a group would show a nearby
    /// contact the phone, and the main identity listening on it.
    fn direct_neighbour(&self, peer: &PublicIdentity) -> bool {
        !self.shared.persona
            && self.shared.mutual(peer)
            && self.sessions().iter().any(|s| s.peer == *peer && nearby(s))
    }

    /// Takes `peer`'s request (or offer) unless one was taken within
    /// [`DIRECT_EVERY`].
    fn direct_due(&self, peer: PublicIdentity, offer: bool) -> bool {
        let now = Instant::now();
        let mut taken = lock(&self.shared.direct_taken);
        taken.retain(|_, t| now.duration_since(*t) < DIRECT_EVERY);
        if taken.contains_key(&(peer, offer)) {
            return false;
        }
        taken.insert((peer, offer), now);
        true
    }

    /// Sends `peer` the credentials of a Wi-Fi Direct group we created,
    /// and the address we listen on inside it.
    pub fn offer_wifi_direct(&self, peer: &PublicIdentity, offer: DirectOffer) -> Result<()> {
        if !self.direct_neighbour(peer) {
            return Err(NetError::NoRoute(
                "Wi-Fi Direct needs a session over a nearby link with a mutually approved contact, \
                 and isn't for an anonymous identity"
                    .into(),
            ));
        }
        let m = DirectMsg::Offer(offer).encode()?;
        // Validate as the receiver will.
        DirectMsg::decode(&m)?;
        self.send(peer, AppMessage::Direct(m))
    }

    /// Asks `peer` to create a Wi-Fi Direct group and offer it to us.
    pub fn request_wifi_direct(&self, peer: &PublicIdentity) -> Result<()> {
        if !self.direct_neighbour(peer) {
            return Err(NetError::NoRoute(
                "Wi-Fi Direct needs a session over a nearby link with a mutually approved contact, \
                 and isn't for an anonymous identity"
                    .into(),
            ));
        }
        self.send(peer, AppMessage::Direct(DirectMsg::Request.encode()?))
    }

    pub(crate) fn on_direct(&self, peer: PublicIdentity, payload: &[u8]) {
        if !self.direct_neighbour(&peer) {
            return;
        }
        match DirectMsg::decode(payload) {
            Ok(DirectMsg::Offer(offer)) if self.direct_due(peer, true) => {
                self.emit(Event::WifiDirectOffer { peer, offer });
            }
            Ok(DirectMsg::Request) if self.direct_due(peer, false) => {
                self.emit(Event::WifiDirectRequested { peer });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_validate() {
        let o = DirectOffer {
            ssid: "DIRECT-th-abcd".into(),
            passphrase: "correct horse".into(),
            addr: "192.168.49.1:7450".into(),
        };
        for m in [DirectMsg::Offer(o.clone()), DirectMsg::Request] {
            assert_eq!(DirectMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        let bad = |f: &dyn Fn(&mut DirectOffer)| {
            let mut x = o.clone();
            f(&mut x);
            DirectMsg::decode(&DirectMsg::Offer(x).encode().unwrap()).is_err()
        };
        assert!(bad(&|x| x.passphrase = "short".into()));
        assert!(bad(&|x| x.ssid = String::new()));
        assert!(bad(&|x| x.ssid = "x".repeat(33)));
        assert!(bad(&|x| x.addr = "example.com:80".into()));
        assert!(!format!("{o:?}").contains("horse"), "passphrase not logged");
    }

    #[test]
    fn only_local_networks_and_radio_links_are_nearby() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        for local in [
            "127.0.0.1",
            "192.168.49.1",
            "10.1.2.3",
            "172.16.0.9",
            "169.254.3.4",
            "::1",
            "fe80::1",
            "fd12::1",
            "::ffff:192.168.1.5",
        ] {
            assert!(nearby_link("tcp", false, ip(local)), "{local}");
            assert!(nearby_link("quic", false, ip(local)), "{local}");
        }
        for far in [
            "203.0.113.7",
            "8.8.8.8",
            "100.64.0.1",
            "2001:db8::1",
            "::ffff:1.1.1.1",
        ] {
            assert!(!nearby_link("tcp", false, ip(far)), "{far}");
            assert!(!nearby_link("quic", false, ip(far)), "{far}");
        }
        assert!(nearby_link("ble", false, ip("0.0.0.0")));
        assert!(!nearby_link("tcp", true, ip("192.168.1.5")), "relayed");
    }
}
