//! Userspace WireGuard (boringtun) for platforms without kernel
//! WireGuard: Android, where the VPN service hands over a TUN file
//! descriptor, and unprivileged desktops (Appendix D).
//!
//! The device follows the node's tunnel peers (`Node::tunnel_peer_entries`),
//! which mutual approval and each session's `TunnelOffer` keep current: a
//! peer whose key or PSK changes gets a fresh tunnel, and a revoked one is
//! removed. Packets from the TUN interface go to the peer that owns the
//! destination overlay address; decrypted packets reach the interface only
//! when their source is the sending peer's own overlay address (its
//! `AllowedIPs`). Only IPv6 overlay traffic is carried.
//!
//! WireGuard's own UDP port is often unreachable: mobile carriers' NATs
//! give it an outside port nobody can guess, and nothing punches it. While
//! no authenticated packet has come over that path lately, data goes over
//! the Threnody session with the peer instead (QUIC datagrams on the
//! session's punched path, or the session stream), as call media does;
//! handshakes and keepalives try both, so the direct path takes over again
//! as soon as it works.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use boringtun::noise::handshake::parse_handshake_anon;
use boringtun::noise::{Packet, Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use threnody_core::PublicIdentity;
use threnody_core::tunnel::WgKeys;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::node::{Node, TunnelPeer};

/// How often WireGuard's timers run (handshakes, keepalives, expiry).
const TICK: Duration = Duration::from_millis(250);
/// How often the peer list is compared with the node's, in ticks.
const SYNC_EVERY: u32 = 4;
const KEEPALIVE_S: u16 = 25;
/// How long after the last authenticated packet over UDP the tunnel's own
/// path still counts as working: longer than the keepalive, so an idle
/// tunnel keeps it.
const DIRECT_FOR: Duration = Duration::from_secs(KEEPALIVE_S as u64 + 10);
/// Packets from sessions waiting for the device; more are dropped.
const INBOUND_QUEUE: usize = 1024;
/// Big enough for any IP packet plus WireGuard's overhead.
const BUF: usize = 65_536;

/// The tunnel interface: whole IP packets in and out.
pub trait TunIo: Send + Sync + 'static {
    /// Reads one packet into `buf`; returns its length.
    fn read(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send;
    /// Writes one packet.
    fn write(&self, packet: &[u8]) -> impl Future<Output = io::Result<()>> + Send;
}

/// A running userspace WireGuard device; stops when dropped.
pub struct WgDevice {
    port: u16,
    task: JoinHandle<()>,
    node: Node,
    inbound: mpsc::Sender<(PublicIdentity, Vec<u8>)>,
}

impl WgDevice {
    /// Binds UDP `port` (0 for any) and starts carrying packets between
    /// `tun` and the node's tunnel peers.
    pub async fn start(node: Node, tun: impl TunIo, port: u16) -> io::Result<Self> {
        let udp = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port)).await?;
        let port = udp.local_addr()?.port();
        let keys = WgKeys::derive(node.identity_ref());
        let device = Device {
            secret: StaticSecret::from(*keys.secret()),
            public: PublicKey::from(*keys.public()),
            peers: HashMap::new(),
            next_index: 1,
        };
        let (inbound, from_sessions) = mpsc::channel(INBOUND_QUEUE);
        node.set_tunnel_inbound(Some(inbound.clone()));
        let task = tokio::spawn(run(node.clone(), device, udp, tun, from_sessions));
        Ok(Self {
            port,
            task,
            node,
            inbound,
        })
    }

    /// The UDP port WireGuard listens on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for WgDevice {
    fn drop(&mut self) {
        self.task.abort();
        self.node.clear_tunnel_inbound(&self.inbound);
    }
}

struct Peer {
    tunn: Tunn,
    index: u32,
    wg_public: [u8; 32],
    psk: [u8; 32],
    overlay: Ipv6Addr,
    /// Where packets go: the session's address at first, then wherever
    /// authenticated packets last came from (WireGuard roaming).
    endpoint: SocketAddr,
    /// The endpoint the node last gave, to notice a new session's.
    offered: SocketAddr,
    /// When an authenticated packet last came over the tunnel's own UDP
    /// path: while recent, that path works.
    direct_at: Option<Instant>,
}

impl Peer {
    fn direct(&self) -> bool {
        self.direct_at.is_some_and(|t| t.elapsed() < DIRECT_FOR)
    }
}

struct Device {
    secret: StaticSecret,
    public: PublicKey,
    peers: HashMap<PublicIdentity, Peer>,
    next_index: u32,
}

/// Packets to send, each with the peer it is for.
type Outgoing = Vec<(PublicIdentity, Vec<u8>)>;

impl Device {
    /// Brings the peers in line with the node's, queueing handshake
    /// initiations for new tunnels.
    fn sync(&mut self, current: Vec<(PublicIdentity, TunnelPeer)>, out: &mut Outgoing) {
        let wanted: HashMap<_, _> = current.into_iter().collect();
        self.peers.retain(|id, _| wanted.contains_key(id));
        for (id, tp) in wanted {
            match self.peers.get_mut(&id) {
                Some(p) if p.wg_public == tp.wg_public && p.psk == *tp.psk.0 => {
                    if p.offered != tp.endpoint {
                        // A new session (the peer moved): its UDP path is
                        // unproven, so data rides the session until an
                        // authenticated packet arrives over it.
                        p.offered = tp.endpoint;
                        p.endpoint = tp.endpoint;
                        p.direct_at = None;
                    }
                }
                _ => {
                    // New, or a new session's key: start over.
                    let index = self.next_index;
                    self.next_index = (self.next_index % 0x00ff_ffff) + 1;
                    let mut tunn = Tunn::new(
                        self.secret.clone(),
                        PublicKey::from(tp.wg_public),
                        Some(*tp.psk.0),
                        Some(KEEPALIVE_S),
                        index,
                        None,
                    );
                    let mut buf = [0u8; 256];
                    if let TunnResult::WriteToNetwork(p) =
                        tunn.format_handshake_initiation(&mut buf, false)
                    {
                        out.push((id, p.to_vec()));
                    }
                    self.peers.insert(
                        id,
                        Peer {
                            tunn,
                            index,
                            wg_public: tp.wg_public,
                            psk: *tp.psk.0,
                            overlay: tp.overlay,
                            endpoint: tp.endpoint,
                            offered: tp.endpoint,
                            direct_at: None,
                        },
                    );
                }
            }
        }
    }

    fn by_overlay(&mut self, addr: Ipv6Addr) -> Option<(PublicIdentity, &mut Peer)> {
        self.peers
            .iter_mut()
            .find(|(_, p)| p.overlay == addr)
            .map(|(id, p)| (*id, p))
    }

    /// The peer an incoming UDP datagram is for.
    fn for_datagram(&mut self, datagram: &[u8]) -> Option<(PublicIdentity, &mut Peer)> {
        let found = match Tunn::parse_incoming_packet(datagram).ok()? {
            Packet::HandshakeInit(h) => {
                let half = parse_handshake_anon(&self.secret, &self.public, &h).ok()?;
                return self
                    .peers
                    .iter_mut()
                    .find(|(_, p)| p.wg_public == half.peer_static_public)
                    .map(|(id, p)| (*id, p));
            }
            Packet::HandshakeResponse(p) => p.receiver_idx,
            Packet::PacketCookieReply(p) => p.receiver_idx,
            Packet::PacketData(p) => p.receiver_idx,
        } >> 8;
        self.peers
            .iter_mut()
            .find(|(_, p)| p.index == found)
            .map(|(id, p)| (*id, p))
    }
}

/// The destination of an IPv6 packet.
fn ipv6_destination(packet: &[u8]) -> Option<Ipv6Addr> {
    if packet.len() < 40 || packet[0] >> 4 != 6 {
        return None;
    }
    let dst: [u8; 16] = packet[24..40].try_into().ok()?;
    Some(Ipv6Addr::from(dst))
}

/// The socket is IPv6 and dual-stack: IPv4 endpoints go as mapped addresses.
fn mapped(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) => SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port()),
        v6 => v6,
    }
}

fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}

/// Sends `packet` to peer `id`: over the tunnel's own UDP path while it
/// works, else over the session with the peer. Until the direct path is
/// proven, handshakes, cookies and keepalives go both ways, so a path
/// that opens (or comes back) is found; data goes only over the session.
async fn send(node: &Node, udp: &UdpSocket, id: &PublicIdentity, peer: &Peer, packet: &[u8]) {
    if peer.direct() {
        let _ = udp.send_to(packet, mapped(peer.endpoint)).await;
        return;
    }
    // Data packets carry at least one byte more than a bare keepalive.
    let data = packet.first() == Some(&4) && packet.len() > 32;
    let over_session = node.send_tunnel_packet(id, packet);
    if !data || !over_session {
        let _ = udp.send_to(packet, mapped(peer.endpoint)).await;
    }
}

async fn send_all(node: &Node, udp: &UdpSocket, device: &Device, out: &mut Outgoing) {
    for (id, packet) in out.drain(..) {
        if let Some(peer) = device.peers.get(&id) {
            send(node, udp, &id, peer, &packet).await;
        }
    }
}

/// Decrypts `datagram` from peer `id`, which came over UDP from `from`
/// or (with `None`) over the session. Returns false when the TUN
/// interface is gone.
#[allow(clippy::too_many_arguments)]
async fn receive(
    node: &Node,
    udp: &UdpSocket,
    tun: &impl TunIo,
    id: PublicIdentity,
    peer: &mut Peer,
    from: Option<SocketAddr>,
    datagram: &[u8],
    dst: &mut [u8],
) -> bool {
    let mut flush = false;
    match peer.tunn.decapsulate(from.map(|f| f.ip()), datagram, dst) {
        TunnResult::Done => {}
        TunnResult::Err(_) => return true,
        TunnResult::WriteToNetwork(p) => {
            let p = p.to_vec();
            reply(node, udp, id, peer, from, &p).await;
            flush = true;
        }
        TunnResult::WriteToTunnelV6(p, src) => {
            // AllowedIPs: the peer's own overlay address only.
            if src == peer.overlay && tun.write(p).await.is_err() {
                return false;
            }
        }
        TunnResult::WriteToTunnelV4(..) => {}
    }
    if flush {
        while let TunnResult::WriteToNetwork(p) = peer.tunn.decapsulate(None, &[], dst) {
            let p = p.to_vec();
            reply(node, udp, id, peer, from, &p).await;
        }
    }
    if let Some(from) = from {
        // Authenticated over UDP: the direct path works, and this is
        // where the peer is now.
        peer.endpoint = from;
        peer.direct_at = Some(Instant::now());
    }
    true
}

/// Answers on the path a packet came by: back to its UDP source, or as
/// any other packet (`send`) when it came over the session.
async fn reply(
    node: &Node,
    udp: &UdpSocket,
    id: PublicIdentity,
    peer: &Peer,
    from: Option<SocketAddr>,
    packet: &[u8],
) {
    match from {
        Some(from) => {
            let _ = udp.send_to(packet, mapped(from)).await;
        }
        None => send(node, udp, &id, peer, packet).await,
    }
}

async fn run(
    node: Node,
    mut device: Device,
    udp: UdpSocket,
    tun: impl TunIo,
    mut from_sessions: mpsc::Receiver<(PublicIdentity, Vec<u8>)>,
) {
    let mut tun_buf = vec![0u8; BUF];
    let mut udp_buf = vec![0u8; BUF];
    let mut dst = vec![0u8; BUF];
    let mut out = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    let mut ticks = 0u32;
    device.sync(node.tunnel_peer_entries(), &mut out);
    send_all(&node, &udp, &device, &mut out).await;
    loop {
        tokio::select! {
            r = tun.read(&mut tun_buf) => {
                let Ok(n) = r else { return };
                let packet = &tun_buf[..n];
                let Some((id, peer)) = ipv6_destination(packet).and_then(|d| device.by_overlay(d)) else {
                    continue;
                };
                if let TunnResult::WriteToNetwork(p) = peer.tunn.encapsulate(packet, &mut dst) {
                    send(&node, &udp, &id, peer, p).await;
                }
            }
            r = udp.recv_from(&mut udp_buf) => {
                let Ok((n, from)) = r else { continue };
                let datagram = &udp_buf[..n];
                let Some((id, peer)) = device.for_datagram(datagram) else { continue };
                if !receive(&node, &udp, &tun, id, peer, Some(canonical(from)), datagram, &mut dst).await {
                    return;
                }
            }
            Some((id, datagram)) = from_sessions.recv() => {
                // Only the tunnel with the session's own peer.
                let Some(peer) = device.peers.get_mut(&id) else { continue };
                if !receive(&node, &udp, &tun, id, peer, None, &datagram, &mut dst).await {
                    return;
                }
            }
            _ = tick.tick() => {
                ticks = ticks.wrapping_add(1);
                if ticks.is_multiple_of(SYNC_EVERY) {
                    device.sync(node.tunnel_peer_entries(), &mut out);
                }
                for (id, peer) in device.peers.iter_mut() {
                    if let TunnResult::WriteToNetwork(p) = peer.tunn.update_timers(&mut dst) {
                        out.push((*id, p.to_vec()));
                    }
                }
                send_all(&node, &udp, &device, &mut out).await;
            }
        }
    }
}

// A TUN descriptor (Android's `VpnService`); `threnody-tun` holds the
// system calls.
#[cfg(unix)]
impl TunIo for threnody_tun::FdTun {
    fn read(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> + Send {
        threnody_tun::FdTun::read(self, buf)
    }

    fn write(&self, packet: &[u8]) -> impl Future<Output = io::Result<()>> + Send {
        threnody_tun::FdTun::write(self, packet)
    }
}
