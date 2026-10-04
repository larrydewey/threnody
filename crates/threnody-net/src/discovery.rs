//! UDP transport for private discovery beacons (`threnody_core::discovery`).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use threnody_core::discovery::{beacon, recognise};
use threnody_core::{PublicIdentity, now_ms};
use tokio::net::UdpSocket;

use crate::error::Result;
use crate::node::{Event, Node};

/// Bluetooth LE service UUID for Threnody. Devices advertise service data
/// under it: the L2CAP PSM they listen on, as a little-endian u16.
pub const BLE_SERVICE_UUID: &str = "7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01";

/// Default multicast group and port for beacons (organisation-local scope).
pub const DEFAULT_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 84, 86);
pub const DEFAULT_PORT: u16 = 7451;
/// Minimum time between automatic dials to the same peer.
const REDIAL_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct DiscoveryConfig {
    /// Local address to receive beacons on.
    pub bind: SocketAddr,
    /// Where to send beacons (multicast group, broadcast or unicast).
    pub targets: Vec<SocketAddr>,
    pub interval: Duration,
    /// Dial recognised peers automatically (spec §10).
    pub auto_connect: bool,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT)),
            targets: vec![SocketAddr::from((DEFAULT_GROUP, DEFAULT_PORT))],
            interval: Duration::from_secs(10),
            auto_connect: true,
        }
    }
}

fn bind(cfg: &DiscoveryConfig) -> Result<UdpSocket> {
    let domain = if cfg.bind.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    // Several nodes on one host (or one node per user) share the port.
    sock.set_reuse_address(true)?;
    #[cfg(unix)]
    sock.set_reuse_port(true)?;
    sock.set_broadcast(true)?;
    sock.bind(&cfg.bind.into())?;
    for t in &cfg.targets {
        if let IpAddr::V4(g) = t.ip()
            && g.is_multicast()
        {
            // Joining can fail without a multicast route; unicast targets still work.
            let _ = sock.join_multicast_v4(&g, &Ipv4Addr::UNSPECIFIED);
            let _ = sock.set_multicast_loop_v4(true);
        }
    }
    sock.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(sock.into())?)
}

impl Node {
    /// Starts sending and listening for beacons. `listen_port` is the TCP
    /// port peers should dial.
    pub fn start_discovery(&self, cfg: DiscoveryConfig, listen_port: u16) -> Result<SocketAddr> {
        let sock = std::sync::Arc::new(bind(&cfg)?);
        let local = sock.local_addr()?;

        let (node, tx, c) = (self.clone(), sock.clone(), cfg.clone());
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(c.interval);
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    () = &mut closed => break,
                }
                let keys: Vec<[u8; 32]> = node
                    .contacts()
                    .iter()
                    .filter(|c| c.mutually_approved())
                    .filter_map(|c| c.discovery_key)
                    .collect();
                if keys.is_empty() {
                    continue;
                }
                let b = beacon(&node.identity(), &keys, listen_port, now_ms() / 1000);
                for t in &c.targets {
                    let _ = tx.send_to(&b, t).await;
                }
            }
        });

        let node = self.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            let mut last_dial: HashMap<PublicIdentity, Instant> = HashMap::new();
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                let received = tokio::select! {
                    r = sock.recv_from(&mut buf) => r,
                    () = &mut closed => break,
                };
                let Ok((n, src)) = received else {
                    continue;
                };
                let contacts = node.contacts();
                let candidates: Vec<_> = contacts
                    .iter()
                    .filter(|c| c.mutually_approved())
                    .filter_map(|c| c.discovery_key.as_ref().map(|k| (&c.key, k)))
                    .collect();
                for (peer, port) in recognise(&buf[..n], candidates, now_ms() / 1000) {
                    let addr = SocketAddr::new(src.ip(), port);
                    let connected = node.sessions().iter().any(|s| s.peer == peer);
                    node.emit(Event::Discovered {
                        peer,
                        addr,
                        connected,
                    });
                    // Exactly one side dials: the one with the smaller key.
                    let we_dial = node.identity().as_bytes() < peer.as_bytes();
                    let recently = last_dial
                        .get(&peer)
                        .is_some_and(|t| t.elapsed() < REDIAL_AFTER);
                    if cfg.auto_connect && we_dial && !connected && !recently {
                        last_dial.insert(peer, Instant::now());
                        let node = node.clone();
                        tokio::spawn(async move {
                            let _ = node
                                .connect(&addr.to_string(), Some(peer.fingerprint()))
                                .await;
                        });
                    }
                }
            }
        });
        Ok(local)
    }
}
