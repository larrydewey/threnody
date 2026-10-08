//! Userspace WireGuard implementation using boringtun.
//!
//! Provides a WireGuard implementation that doesn't require kernel support
//! or CAP_NET_ADMIN, suitable for Android, iOS, and unprivileged environments.

#[cfg(feature = "boringtun")]
use std::collections::HashMap;
#[cfg(feature = "boringtun")]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(feature = "boringtun")]
use std::sync::Arc;
#[cfg(feature = "boringtun")]
use std::time::Duration;

#[cfg(feature = "boringtun")]
use boringtun::noise::{Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
#[cfg(feature = "boringtun")]
use socket2::{Domain, Protocol, Socket, Type};
#[cfg(feature = "boringtun")]
use tokio::net::UdpSocket;
#[cfg(feature = "boringtun")]
use tokio::sync::mpsc;
#[cfg(feature = "boringtun")]
use tokio::task::JoinHandle;
#[cfg(feature = "boringtun")]
use zeroize::Zeroizing;

#[cfg(feature = "boringtun")]
use threnody_core::tunnel::{DEFAULT_PORT, WgKeys, overlay_addr};
#[cfg(feature = "boringtun")]
use threnody_core::{Identity, PublicIdentity};

#[cfg(feature = "boringtun")]
use crate::error::{NetError, Result};
#[cfg(feature = "boringtun")]
use crate::node::Secret;
#[cfg(feature = "boringtun")]
use anyhow;

#[cfg(feature = "boringtun")]
#[derive(Clone, Debug)]
pub struct WgPeerConfig {
    pub peer_identity: PublicIdentity,
    pub peer_wg_public: [u8; 32],
    pub endpoint: SocketAddr,
    pub overlay: Ipv6Addr,
    pub psk: Secret,
}

#[cfg(feature = "boringtun")]
pub struct WgUserspace {
    local_keys: WgKeys,
    local_identity: PublicIdentity,
    listen_port: u16,
    peers: HashMap<PublicIdentity, WgPeerConfig>,
    tunnels: HashMap<PublicIdentity, Tunn>,
    udp_socket: Option<Arc<UdpSocket>>,
    tx_task: Option<JoinHandle<()>>,
    rx_task: Option<JoinHandle<()>>,
    shutdown_tx: Option<mpsc::Sender<()>>,
}

#[cfg(feature = "boringtun")]
impl WgUserspace {
    pub fn new(identity: &Identity, listen_port: u16) -> Result<Self> {
        let local_keys = WgKeys::derive(identity);
        let local_identity = identity.public();
        Ok(Self {
            local_keys,
            local_identity,
            listen_port,
            peers: HashMap::new(),
            tunnels: HashMap::new(),
            udp_socket: None,
            tx_task: None,
            rx_task: None,
            shutdown_tx: None,
        })
    }

    pub fn local_identity(&self) -> &PublicIdentity {
        &self.local_identity
    }

    pub fn listen_port(&self) -> u16 {
        self.listen_port
    }

    pub fn local_wg_public(&self) -> [u8; 32] {
        *self.local_keys.public()
    }

    pub fn local_overlay(&self) -> Ipv6Addr {
        overlay_addr(&self.local_identity)
    }

    pub fn add_peer(&mut self, config: WgPeerConfig) -> Result<()> {
        let peer_id = config.peer_identity;

        // Create boringtun tunnel
        let static_private = StaticSecret::from(*self.local_keys.public());
        let peer_static_public = PublicKey::from(config.peer_wg_public);

        let mut tunn = Tunn::new(
            static_private,
            peer_static_public,
            Some(config.psk.0[..].try_into().unwrap()),
            Some(25), // persistent keepalive
            0,        // index
            None,     // rate limiter
        );

        // Initiate handshake
        let mut handshake_buf = [0u8; 1024];
        let result = tunn.format_handshake_initiation(&mut handshake_buf, false);
        if let TunnResult::WriteToNetwork(buf) = result {
            // Send handshake initiation
            if let Some(socket) = &self.udp_socket {
                let endpoint = config.endpoint;
                let socket = socket.clone();
                let buf = buf.to_vec();
                tokio::spawn(async move {
                    let _ = socket.send_to(&buf, endpoint).await;
                });
            }
        }

        self.tunnels.insert(peer_id, tunn);
        self.peers.insert(peer_id, config);
        Ok(())
    }

    pub fn remove_peer(&mut self, peer: &PublicIdentity) -> bool {
        self.tunnels.remove(peer).is_some()
    }

    pub fn start(&mut self) -> Result<()> {
        // Create UDP socket
        let socket = std::net::UdpSocket::bind(format!("0.0.0.0:{}", self.listen_port))
            .map_err(NetError::Io)?;
        socket.set_nonblocking(true).map_err(NetError::Io)?;
        let udp_socket = Arc::new(UdpSocket::from_std(socket).map_err(NetError::Io)?);

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // We need to move tunnels and peers into the tasks
        // Since Tunn doesn't implement Clone, we'll use a different approach
        // For now, just store the socket and let the Node handle packet processing
        self.udp_socket = Some(udp_socket.clone());
        self.shutdown_tx = Some(shutdown_tx);

        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.try_send(());
        }
        if let Some(task) = self.rx_task.take() {
            task.abort();
        }
        if let Some(task) = self.tx_task.take() {
            task.abort();
        }
        self.udp_socket = None;
    }

    pub fn encapsulate(
        &mut self,
        peer: &PublicIdentity,
        src: &[u8],
        dst: &mut [u8],
    ) -> Result<usize> {
        if let Some(tunn) = self.tunnels.get_mut(peer) {
            let result = tunn.encapsulate(src, dst);
            match result {
                TunnResult::WriteToNetwork(out) => Ok(out.len()),
                TunnResult::Done => Ok(0),
                TunnResult::Err(e) => Err(NetError::External(anyhow::anyhow!(
                    "encapsulate error: {:?}",
                    e
                ))),
                _ => Err(NetError::External(anyhow::anyhow!(
                    "unexpected encapsulate result"
                ))),
            }
        } else {
            Err(NetError::External(anyhow::anyhow!("no tunnel for peer")))
        }
    }

    pub fn decapsulate(
        &mut self,
        peer: &PublicIdentity,
        src_addr: SocketAddr,
        packet: &[u8],
        out_buf: &mut [u8],
    ) -> Result<Option<Vec<u8>>> {
        if let Some(tunn) = self.tunnels.get_mut(peer) {
            let result = tunn.decapsulate(Some(src_addr.ip()), packet, out_buf);
            match result {
                TunnResult::WriteToNetwork(out) => Ok(Some(out.to_vec())),
                TunnResult::WriteToTunnelV6(out, dst) => Ok(Some(out.to_vec())),
                TunnResult::WriteToTunnelV4(out, dst) => Ok(Some(out.to_vec())),
                TunnResult::Done => Ok(None),
                TunnResult::Err(e) => Err(NetError::External(anyhow::anyhow!(
                    "decapsulate error: {:?}",
                    e
                ))),
            }
        } else {
            Err(NetError::External(anyhow::anyhow!("no tunnel for peer")))
        }
    }

    pub fn update_timers(
        &mut self,
        peer: &PublicIdentity,
        out_buf: &mut [u8],
    ) -> Result<Option<Vec<u8>>> {
        if let Some(tunn) = self.tunnels.get_mut(peer) {
            let result = tunn.update_timers(out_buf);
            match result {
                TunnResult::WriteToNetwork(out) => Ok(Some(out.to_vec())),
                TunnResult::Done => Ok(None),
                TunnResult::Err(e) => {
                    Err(NetError::External(anyhow::anyhow!("timer error: {:?}", e)))
                }
                _ => Ok(None),
            }
        } else {
            Err(NetError::External(anyhow::anyhow!("no tunnel for peer")))
        }
    }

    pub fn is_running(&self) -> bool {
        self.udp_socket.is_some()
    }

    pub fn udp_socket(&self) -> Option<Arc<UdpSocket>> {
        self.udp_socket.clone()
    }

    pub fn peer_config(&self, peer: &PublicIdentity) -> Option<&WgPeerConfig> {
        self.peers.get(peer)
    }
}

#[cfg(feature = "boringtun")]
impl Drop for WgUserspace {
    fn drop(&mut self) {
        self.stop();
    }
}

// Stub implementation when boringtun feature is not enabled
#[cfg(not(feature = "boringtun"))]
pub struct WgUserspace;

#[cfg(not(feature = "boringtun"))]
impl WgUserspace {
    pub fn new(_identity: &Identity, _listen_port: u16) -> Result<Self> {
        Err(NetError::External(anyhow::anyhow!(
            "boringtun feature not enabled"
        )))
    }
}
