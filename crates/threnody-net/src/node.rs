//! A running Threnody node: listens, dials, runs one task per session,
//! enforces the trust policy and keeps the contact book current.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use threnody_core::crypto::aead::Suite;
use threnody_core::discovery::DISCOVERY_CONTEXT;
use threnody_core::store::{Contacts, Home};
use threnody_core::tunnel::{PSK_CONTEXT, WgKeys, overlay_addr};
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, SecureChannel, now_ms};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::error::{NetError, Result};
use crate::frame::{read_frame, write_frame};
use crate::handshake;

/// Who may hold a session with us (checked right after authentication).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceptPolicy {
    /// Trust on first use: any authenticated identity (spec §5.2 discovery).
    Anyone,
    /// Only identities already in the contact book.
    ContactsOnly,
    /// Only mutually approved contacts.
    ApprovedOnly,
}

pub struct NodeConfig {
    pub home: Home,
    pub identity: Identity,
    pub policy: AcceptPolicy,
    /// When set, each session sends exactly one padded frame per interval,
    /// using cover messages when idle (spec §9, layer 1).
    pub constant_rate: Option<Duration>,
    /// When set, offer WireGuard tunnels on this UDP port to mutually
    /// approved peers (spec §8).
    pub tunnel_port: Option<u16>,
}

/// A 32-byte secret that never prints.
#[derive(Clone)]
pub struct Secret(pub Zeroizing<[u8; 32]>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Something the user interface should know about.
#[derive(Debug)]
pub enum Event {
    Connected {
        peer: PublicIdentity,
        addr: SocketAddr,
        suite: Suite,
        new_contact: bool,
    },
    Message {
        peer: PublicIdentity,
        msg: AppMessage,
    },
    /// The peer's approval of us changed (spec §5.2).
    ApprovalChanged {
        peer: PublicIdentity,
        remote_approved: bool,
        mutual: bool,
    },
    Disconnected {
        peer: PublicIdentity,
        reason: String,
    },
    Rejected {
        addr: SocketAddr,
        reason: String,
    },
    /// A mutually approved peer offered a WireGuard tunnel. `psk` is fresh
    /// for this session; reapply it whenever this event repeats.
    TunnelUp {
        peer: PublicIdentity,
        wg_public: [u8; 32],
        endpoint: SocketAddr,
        overlay: Ipv6Addr,
        psk: Secret,
    },
    /// An approved peer's beacon was seen on the local network.
    Discovered {
        peer: PublicIdentity,
        addr: SocketAddr,
        connected: bool,
    },
    /// An automatic dial (e.g. after discovery) failed.
    DialFailed {
        peer: PublicIdentity,
        addr: SocketAddr,
        reason: String,
    },
    /// Mutual approval ended: remove the peer from the tunnel.
    TunnelDown {
        peer: PublicIdentity,
        wg_public: Option<[u8; 32]>,
    },
}

/// Read-only description of a live session.
#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub peer: PublicIdentity,
    pub addr: SocketAddr,
    pub suite: Suite,
    pub transport: &'static str,
    pub since_ms: u64,
    pub outbound: bool,
}

struct SessionHandle {
    id: u64,
    tx: mpsc::UnboundedSender<AppMessage>,
    info: SessionInfo,
}

struct Shared {
    identity: Identity,
    home: Home,
    contacts: Mutex<Contacts>,
    sessions: Mutex<HashMap<PublicIdentity, SessionHandle>>,
    events: mpsc::UnboundedSender<Event>,
    policy: Mutex<AcceptPolicy>,
    constant_rate: Option<Duration>,
    tunnel: Option<(u16, [u8; 32])>,
    /// WireGuard keys peers offered us, for clean removal on revocation.
    tunnel_peers: Mutex<HashMap<PublicIdentity, [u8; 32]>>,
    next_id: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a lock leaves plain data behind; keep going.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn mutual(&self, peer: &PublicIdentity) -> bool {
        lock(&self.contacts)
            .get(peer)
            .is_some_and(|c| c.mutually_approved())
    }

    fn tunnel_down(&self, peer: &PublicIdentity) {
        if self.tunnel.is_some() {
            let wg_public = lock(&self.tunnel_peers).remove(peer);
            self.emit(Event::TunnelDown {
                peer: *peer,
                wg_public,
            });
        }
    }

    fn save_contacts(&self, c: &Contacts) {
        if let Err(e) = self.home.save_contacts(c) {
            eprintln!("threnody: failed to save contacts: {e}");
        }
    }
}

#[derive(Clone)]
pub struct Node {
    shared: Arc<Shared>,
}

impl Node {
    pub fn new(cfg: NodeConfig) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let contacts = cfg.home.load_contacts()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let tunnel = cfg
            .tunnel_port
            .map(|port| (port, *WgKeys::derive(&cfg.identity).public()));
        let shared = Shared {
            identity: cfg.identity,
            home: cfg.home,
            contacts: Mutex::new(contacts),
            sessions: Mutex::new(HashMap::new()),
            events: tx,
            policy: Mutex::new(cfg.policy),
            constant_rate: cfg.constant_rate,
            tunnel,
            tunnel_peers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        };
        Ok((
            Self {
                shared: Arc::new(shared),
            },
            rx,
        ))
    }

    pub(crate) fn emit(&self, e: Event) {
        self.shared.emit(e);
    }

    pub fn identity(&self) -> PublicIdentity {
        self.shared.identity.public()
    }

    pub fn policy(&self) -> AcceptPolicy {
        *lock(&self.shared.policy)
    }

    pub fn set_policy(&self, p: AcceptPolicy) {
        *lock(&self.shared.policy) = p;
    }

    pub fn constant_rate(&self) -> Option<Duration> {
        self.shared.constant_rate
    }

    /// WireGuard listen port, when tunnels are enabled.
    pub fn tunnel_port(&self) -> Option<u16> {
        self.shared.tunnel.map(|(p, _)| p)
    }

    /// Peers that currently have a tunnel configured.
    pub fn tunnel_peers(&self) -> Vec<PublicIdentity> {
        lock(&self.shared.tunnel_peers).keys().copied().collect()
    }

    /// Binds a TCP listener and accepts sessions in the background.
    pub async fn listen(&self, addr: &str) -> Result<SocketAddr> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer_addr)) = listener.accept().await else {
                    continue;
                };
                let node = node.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.run_inbound(stream, peer_addr).await {
                        node.shared.emit(Event::Rejected {
                            addr: peer_addr,
                            reason: e.to_string(),
                        });
                    }
                });
            }
        });
        Ok(local)
    }

    async fn run_inbound(&self, mut stream: TcpStream, addr: SocketAddr) -> Result<()> {
        let _ = stream.set_nodelay(true);
        let chan = handshake::accept(&mut stream, &self.shared.identity).await?;
        self.check_policy(chan.peer())?;
        self.spawn_session(stream, chan, addr, false, None);
        Ok(())
    }

    /// Dials `addr`, authenticates, and starts a session. With `expect`, the
    /// peer must present that fingerprint (pinning from an out-of-band
    /// invite); otherwise the outbound connection is itself the user's
    /// decision to trust on first use.
    pub async fn connect(&self, addr: &str, expect: Option<Fingerprint>) -> Result<PublicIdentity> {
        let mut stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);
        let peer_addr = stream.peer_addr()?;
        let chan = handshake::initiate(&mut stream, &self.shared.identity).await?;
        let peer = *chan.peer();
        if let Some(want) = expect
            && peer.fingerprint() != want
        {
            return Err(NetError::IdentityMismatch {
                expected: want.to_string(),
                got: peer.fingerprint().to_string(),
            });
        }
        self.spawn_session(stream, chan, peer_addr, true, Some(addr.to_owned()));
        Ok(peer)
    }

    fn check_policy(&self, peer: &PublicIdentity) -> Result<()> {
        let contacts = lock(&self.shared.contacts);
        let ok = match self.policy() {
            AcceptPolicy::Anyone => true,
            AcceptPolicy::ContactsOnly => contacts.get(peer).is_some(),
            AcceptPolicy::ApprovedOnly => contacts.get(peer).is_some_and(|c| c.mutually_approved()),
        };
        if ok {
            Ok(())
        } else {
            Err(NetError::Refused(peer.fingerprint().to_string()))
        }
    }

    /// Queues a message to a connected peer.
    pub fn send(&self, peer: &PublicIdentity, msg: AppMessage) -> Result<()> {
        let sessions = lock(&self.shared.sessions);
        let h = sessions.get(peer).ok_or(NetError::Closed)?;
        h.tx.send(msg).map_err(|_| NetError::Closed)
    }

    /// Sets our approval of `peer`, persists it and tells the peer if online.
    /// Revoking (`false`) takes effect immediately on both ends' mutual state.
    pub fn set_approval(&self, peer: &PublicIdentity, approved: bool) -> Result<()> {
        {
            let mut contacts = lock(&self.shared.contacts);
            contacts.observe(*peer, None, now_ms());
            if let Some(c) = contacts.get_mut(peer) {
                c.local_approved = approved;
                if !approved {
                    c.discovery_key = None;
                }
            }
            self.shared.save_contacts(&contacts);
        }
        if !approved {
            self.shared.tunnel_down(peer);
        }
        let _ = self.send(peer, AppMessage::Approval { approved });
        Ok(())
    }

    /// Ends the session with `peer`, if any.
    pub fn disconnect(&self, peer: &PublicIdentity) -> bool {
        lock(&self.shared.sessions).remove(peer).is_some()
    }

    pub fn sessions(&self) -> Vec<SessionInfo> {
        lock(&self.shared.sessions)
            .values()
            .map(|h| h.info.clone())
            .collect()
    }

    pub fn contacts(&self) -> Contacts {
        lock(&self.shared.contacts).clone()
    }

    /// Mutates the contact book and persists the result.
    pub fn update_contacts<R>(&self, f: impl FnOnce(&mut Contacts) -> R) -> R {
        let mut contacts = lock(&self.shared.contacts);
        let r = f(&mut contacts);
        self.shared.save_contacts(&contacts);
        r
    }

    fn spawn_session<S>(
        &self,
        stream: S,
        chan: SecureChannel,
        addr: SocketAddr,
        outbound: bool,
        dialed: Option<String>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let peer = *chan.peer();
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let new_contact = {
            let mut contacts = lock(&self.shared.contacts);
            // Only remember dialable addresses: an inbound source port is ephemeral.
            let is_new = contacts.observe(peer, dialed, now_ms());
            self.shared.save_contacts(&contacts);
            is_new
        };
        let info = SessionInfo {
            peer,
            addr,
            suite: chan.suite(),
            transport: "tcp",
            since_ms: now_ms(),
            outbound,
        };
        // A newer session to the same peer replaces the old one; dropping
        // the old handle's sender ends its task.
        lock(&self.shared.sessions).insert(peer, SessionHandle { id, tx, info });
        self.shared.emit(Event::Connected {
            peer,
            addr,
            suite: chan.suite(),
            new_contact,
        });

        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            let reason = match run_session(&shared, stream, chan, addr, rx).await {
                Ok(()) => "closed".to_owned(),
                Err(e) => e.to_string(),
            };
            let mut sessions = lock(&shared.sessions);
            if sessions.get(&peer).is_some_and(|h| h.id == id) {
                sessions.remove(&peer);
            }
            drop(sessions);
            shared.emit(Event::Disconnected { peer, reason });
        });
    }
}

async fn run_session<S>(
    shared: &Shared,
    stream: S,
    mut chan: SecureChannel,
    addr: SocketAddr,
    mut outbox: mpsc::UnboundedReceiver<AppMessage>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = *chan.peer();
    let (mut rd, mut wr) = tokio::io::split(stream);

    // Frame reads are not cancellation-safe, so they get their own task.
    let (in_tx, mut inbox) = mpsc::channel::<Result<Vec<u8>>>(16);
    let reader = tokio::spawn(async move {
        loop {
            let r = read_frame(&mut rd).await;
            let stop = !matches!(r, Ok(Some(_)));
            let item = match r {
                Ok(Some(f)) => Ok(f),
                Ok(None) => Err(NetError::Closed),
                Err(e) => Err(e),
            };
            if in_tx.send(item).await.is_err() || stop {
                break;
            }
        }
    });

    let local_approved = lock(&shared.contacts)
        .get(&peer)
        .is_some_and(|c| c.local_approved);
    // Responder cannot send until the initiator's first ratchet message.
    let mut pending: VecDeque<AppMessage> = VecDeque::new();
    if chan.can_send() {
        pending.push_back(AppMessage::Hello);
    }
    pending.push_back(AppMessage::Approval {
        approved: local_approved,
    });

    let mut ticker = shared.constant_rate.map(|d| {
        let mut t = tokio::time::interval(d);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t
    });

    let mut offered = false;
    let mut discovery_keyed = false;
    let result: Result<()> = async {
        loop {
            // Refresh the pairwise LAN discovery key once per session.
            if !discovery_keyed && shared.mutual(&peer) {
                let key = *chan.export(DISCOVERY_CONTEXT);
                let mut contacts = lock(&shared.contacts);
                if let Some(c) = contacts.get_mut(&peer) {
                    c.discovery_key = Some(key);
                }
                shared.save_contacts(&contacts);
                discovery_keyed = true;
            }
            // Offer a tunnel once per session, as soon as approval is mutual.
            if let Some((port, wg_public)) = shared.tunnel
                && !offered
                && shared.mutual(&peer)
            {
                pending.push_back(AppMessage::TunnelOffer { wg_public, port });
                offered = true;
            }
            if ticker.is_none() && chan.can_send() {
                while let Some(m) = pending.pop_front() {
                    write_frame(&mut wr, &chan.seal(&m)?).await?;
                }
            }
            tokio::select! {
                frame = inbox.recv() => {
                    let frame = match frame {
                        Some(Ok(f)) => f,
                        Some(Err(NetError::Closed)) | None => return Ok(()),
                        Some(Err(e)) => return Err(e),
                    };
                    // Any authentication failure ends the session: on a
                    // reliable stream it can only mean tampering or a broken peer.
                    match chan.open(&frame)? {
                        AppMessage::Hello | AppMessage::Cover => {}
                        AppMessage::Approval { approved } => {
                            let changed = {
                                let mut contacts = lock(&shared.contacts);
                                let changed = contacts.get_mut(&peer).and_then(|c| {
                                    (c.remote_approved != approved).then(|| {
                                        c.remote_approved = approved;
                                        c.mutually_approved()
                                    })
                                });
                                if changed.is_some() {
                                    shared.save_contacts(&contacts);
                                }
                                changed
                            };
                            if let Some(mutual) = changed {
                                shared.emit(Event::ApprovalChanged { peer, remote_approved: approved, mutual });
                                if !mutual {
                                    shared.tunnel_down(&peer);
                                    offered = false;
                                    discovery_keyed = false;
                                    let mut contacts = lock(&shared.contacts);
                                    if let Some(c) = contacts.get_mut(&peer) {
                                        c.discovery_key = None;
                                    }
                                    shared.save_contacts(&contacts);
                                }
                            }
                        }
                        AppMessage::TunnelOffer { wg_public, port } => {
                            // Spec §8: tunnels only between mutually approved devices.
                            if shared.tunnel.is_some() && shared.mutual(&peer) {
                                lock(&shared.tunnel_peers).insert(peer, wg_public);
                                shared.emit(Event::TunnelUp {
                                    peer,
                                    wg_public,
                                    endpoint: SocketAddr::new(addr.ip(), port),
                                    overlay: overlay_addr(&peer),
                                    psk: Secret(chan.export(PSK_CONTEXT)),
                                });
                            }
                        }
                        msg @ (AppMessage::Text { .. } | AppMessage::File { .. }) => {
                            shared.emit(Event::Message { peer, msg });
                        }
                    }
                }
                out = outbox.recv() => match out {
                    Some(m) => pending.push_back(m),
                    None => return Ok(()), // replaced or disconnected locally
                },
                () = tick(&mut ticker) => {
                    if chan.can_send() {
                        let m = pending.pop_front().unwrap_or(AppMessage::Cover);
                        write_frame(&mut wr, &chan.seal(&m)?).await?;
                    }
                }
            }
        }
    }
    .await;
    reader.abort();
    result
}

/// Waits for the next constant-rate tick, or forever when rate limiting is off.
async fn tick(t: &mut Option<tokio::time::Interval>) {
    match t {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}
