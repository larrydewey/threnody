//! Sessions over QUIC on UDP (Appendix N §1).
//!
//! One QUIC endpoint per node, on one UDP socket with the same port number
//! as the TCP listener. Every probe and connection leaves from that socket,
//! so a NAT gives all of them the same outside port. QUIC only carries
//! bytes: each session is one bidirectional stream running the Threnody
//! handshake and ratchet, exactly as over TCP. TLS is a wrapper with a
//! throwaway certificate that nobody checks; the handshake inside
//! authenticates both identities.
//!
//! The socket also carries datagrams that aren't QUIC: hole-punching probes
//! and DHT pings (for our outside address). [`Demux`] takes those out
//! before the endpoint sees them.

use std::io;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use socket2::{Domain, Protocol, Socket, Type};
use threnody_core::{Fingerprint, PublicIdentity};
use tokio::sync::mpsc;

use crate::error::{NetError, Result};
use crate::handshake;
use crate::node::{Event, Node, Route, lock};

const ALPN: &[u8] = b"threnody/1";
/// QUIC keepalive, for sessions without cover traffic: NAT mappings for
/// UDP often expire within 30 s.
const KEEPALIVE: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a QUIC dial may take before it counts as failed.
pub(crate) const DIAL_TIMEOUT: Duration = Duration::from_secs(8);

/// A datagram that isn't QUIC: a probe or a DHT reply.
pub(crate) type Foreign = (SocketAddr, Vec<u8>);

/// Whether a datagram is something other than QUIC. Probes have the QUIC
/// fixed bit clear (grease is off, so no QUIC peer clears it); DHT replies
/// are bencoded dictionaries that say they are replies.
fn is_foreign(data: &[u8]) -> bool {
    threnody_core::rendezvous::looks_like_probe(data) || crate::reach::is_krpc_reply(data)
}

/// Wraps the endpoint's socket, diverting non-QUIC datagrams.
#[derive(Debug)]
struct Demux {
    inner: Arc<dyn AsyncUdpSocket>,
    foreign: mpsc::UnboundedSender<Foreign>,
}

impl AsyncUdpSocket for Demux {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let n = ready!(self.inner.poll_recv(cx, bufs, meta))?;
            let mut kept = 0;
            for i in 0..n {
                let m = meta[i];
                // Coalesced (GRO) batches are QUIC: probes and DHT replies
                // are never the same size as their neighbours.
                if m.len == m.stride && is_foreign(&bufs[i][..m.len]) {
                    let src = SocketAddr::new(m.addr.ip().to_canonical(), m.addr.port());
                    let _ = self.foreign.send((src, bufs[i][..m.len].to_vec()));
                    continue;
                }
                if kept != i {
                    let (head, tail) = bufs.split_at_mut(i);
                    head[kept][..m.len].copy_from_slice(&tail[0][..m.len]);
                    meta[kept] = m;
                }
                kept += 1;
            }
            if kept > 0 {
                return Poll::Ready(Ok(kept));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// A node's QUIC endpoint.
pub(crate) struct Quic {
    pub(crate) endpoint: quinn::Endpoint,
    socket: Arc<dyn AsyncUdpSocket>,
    client: quinn::ClientConfig,
    /// The socket is IPv6 (dual-stack when bound to `[::]`).
    ipv6: bool,
    pub(crate) local: SocketAddr,
}

impl Quic {
    /// Sends one raw datagram (a probe or DHT ping) from the endpoint's socket.
    pub(crate) fn send_raw(&self, to: SocketAddr, data: &[u8]) -> io::Result<()> {
        let destination = match (self.ipv6, to.ip()) {
            (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), to.port()),
            (false, IpAddr::V6(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "IPv4-only socket",
                ));
            }
            _ => to,
        };
        self.socket.try_send(&Transmit {
            destination,
            ecn: None,
            contents: data,
            segment_size: None,
            src_ip: None,
        })
    }

    /// Whether this socket can reach `ip` at all.
    pub(crate) fn can_reach(&self, ip: IpAddr) -> bool {
        self.ipv6 || ip.is_ipv4()
    }
}

/// Accepts any certificate: the Threnody handshake inside the stream is
/// what authenticates the peer. Signatures are still checked, so the TLS
/// layer is at least internally consistent.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_error(e: impl std::fmt::Display) -> NetError {
    NetError::Io(io::Error::other(format!("QUIC setup: {e}")))
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(KEEPALIVE));
    t.max_idle_timeout(Some(
        IDLE_TIMEOUT.try_into().unwrap_or(quinn::VarInt::MAX.into()),
    ));
    // One stream per connection, from the dialer.
    t.max_concurrent_uni_streams(0u8.into());
    t.max_concurrent_bidi_streams(1u8.into());
    Arc::new(t)
}

fn configs() -> Result<(quinn::ServerConfig, quinn::ClientConfig)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert =
        rcgen::generate_simple_self_signed(vec!["threnody".to_owned()]).map_err(tls_error)?;
    let key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .with_no_client_auth()
        .with_single_cert(vec![cert.cert.der().clone()], key.into())
        .map_err(tls_error)?;
    server.alpn_protocols = vec![ALPN.to_vec()];
    let mut client = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    client.alpn_protocols = vec![ALPN.to_vec()];
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(server).map_err(tls_error)?,
    ));
    server.transport_config(transport());
    let mut client = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client).map_err(tls_error)?,
    ));
    client.transport_config(transport());
    Ok((server, client))
}

fn bind(addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if addr.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) {
        // Dual-stack: IPv4 peers arrive as mapped addresses.
        sock.set_only_v6(false)?;
    }
    sock.bind(&addr.into())?;
    Ok(sock.into())
}

/// An address as a peer should see it: IPv4-mapped IPv6 back to IPv4.
pub(crate) fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

impl Node {
    pub(crate) fn quic(&self) -> Option<Arc<Quic>> {
        lock(&self.shared.quic).clone()
    }

    /// The local address of the QUIC endpoint, if one is running.
    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.quic().map(|q| q.local)
    }

    /// Opens the QUIC endpoint on `addr` (UDP) and accepts sessions over it
    /// in the background. Bind `[::]:port` for IPv4 and IPv6 together.
    pub async fn listen_quic(&self, addr: &str) -> Result<SocketAddr> {
        let addr: SocketAddr = tokio::net::lookup_host(addr)
            .await?
            .next()
            .ok_or_else(|| NetError::Io(io::Error::other("no address to bind")))?;
        let std_sock = bind(addr)?;
        let ipv6 = addr.is_ipv6();
        let runtime = quinn::default_runtime()
            .ok_or_else(|| NetError::Io(io::Error::other("no async runtime")))?;
        let socket = runtime.wrap_udp_socket(std_sock)?;
        let (ftx, mut frx) = mpsc::unbounded_channel();
        let demux = Arc::new(Demux {
            inner: socket.clone(),
            foreign: ftx,
        });
        let (server, client) = configs()?;
        let mut ec = quinn::EndpointConfig::default();
        // Peers must keep the QUIC fixed bit set, so probes stay recognisable.
        ec.grease_quic_bit(false);
        let endpoint = quinn::Endpoint::new_with_abstract_socket(ec, Some(server), demux, runtime)?;
        let local = endpoint.local_addr()?;
        let quic = Arc::new(Quic {
            endpoint: endpoint.clone(),
            socket,
            client,
            ipv6,
            local,
        });
        *lock(&self.shared.quic) = Some(quic);

        let node = self.clone();
        tokio::spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                let incoming = tokio::select! {
                    i = endpoint.accept() => i,
                    () = &mut closed => break,
                };
                let Some(incoming) = incoming else { break };
                let node = node.clone();
                tokio::spawn(async move {
                    let addr = canonical(incoming.remote_address());
                    if let Err(e) = node.run_quic_inbound(incoming).await {
                        node.emit(Event::Rejected {
                            addr,
                            reason: e.to_string(),
                        });
                    }
                });
            }
            endpoint.close(0u8.into(), b"bye");
        });

        let node = self.clone();
        tokio::spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                let got = tokio::select! {
                    f = frx.recv() => f,
                    () = &mut closed => break,
                };
                let Some((src, data)) = got else { break };
                node.on_foreign(src, &data);
            }
        });
        Ok(local)
    }

    async fn run_quic_inbound(&self, incoming: quinn::Incoming) -> Result<()> {
        let conn = incoming.await.map_err(quic_error)?;
        let addr = canonical(conn.remote_address());
        let (send, recv) = tokio::time::timeout(DIAL_TIMEOUT, conn.accept_bi())
            .await
            .map_err(|_| NetError::Io(io::ErrorKind::TimedOut.into()))?
            .map_err(quic_error)?;
        let mut stream = tokio::io::join(recv, send);
        let chan = handshake::accept(&mut stream, self.identity_ref()).await?;
        self.check_policy(chan.peer())?;
        self.spawn_session(stream, chan, addr, false, Route::Quic);
        Ok(())
    }

    /// Opens a QUIC connection to `addr`, without a session yet.
    pub(crate) async fn quic_dial(&self, addr: SocketAddr) -> Result<quinn::Connection> {
        let quic = self.quic().ok_or(NetError::Closed)?;
        if !quic.can_reach(addr.ip()) {
            return Err(NetError::Io(io::ErrorKind::Unsupported.into()));
        }
        let connecting = quic
            .endpoint
            .connect_with(quic.client.clone(), addr, "threnody")
            .map_err(|e| NetError::Io(io::Error::other(e.to_string())))?;
        tokio::time::timeout(DIAL_TIMEOUT, connecting)
            .await
            .map_err(|_| NetError::Io(io::ErrorKind::TimedOut.into()))?
            .map_err(quic_error)
    }

    /// Runs the handshake over a QUIC connection we opened and starts the
    /// session. With `expect`, the peer must present that fingerprint.
    pub(crate) async fn quic_session(
        &self,
        conn: quinn::Connection,
        expect: Option<Fingerprint>,
    ) -> Result<PublicIdentity> {
        let addr = canonical(conn.remote_address());
        let (send, recv) = conn.open_bi().await.map_err(quic_error)?;
        let mut stream = tokio::io::join(recv, send);
        let chan = handshake::initiate(&mut stream, self.identity_ref()).await?;
        let peer = *chan.peer();
        if let Some(want) = expect
            && peer.fingerprint() != want
        {
            return Err(NetError::IdentityMismatch {
                expected: want.to_string(),
                got: peer.fingerprint().to_string(),
            });
        }
        self.spawn_session(stream, chan, addr, true, Route::Quic);
        Ok(peer)
    }

    /// Dials `addr` over QUIC, authenticates and starts a session.
    pub async fn connect_quic(
        &self,
        addr: SocketAddr,
        expect: Option<Fingerprint>,
    ) -> Result<PublicIdentity> {
        let conn = self.quic_dial(addr).await?;
        self.quic_session(conn, expect).await
    }
}

fn quic_error(e: impl std::fmt::Display) -> NetError {
    NetError::Io(io::Error::new(
        io::ErrorKind::ConnectionAborted,
        e.to_string(),
    ))
}
