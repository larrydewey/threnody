//! A running Threnody node: listens, dials, runs one task per session,
//! enforces the trust policy and keeps the contact book current.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use threnody_core::account::{AccountBook, AccountChain, AccountId};
use threnody_core::crypto::aead::Suite;
use threnody_core::discovery::DISCOVERY_CONTEXT;
use threnody_core::message::{FEATURE_ACKS, MAX_ACK_IDS};
use threnody_core::prekey::{BundleBook, PrekeyBundle, PrekeyStore};
use threnody_core::store::{Contacts, Home};
use threnody_core::tunnel::{PSK_CONTEXT, WgKeys, overlay_addr};
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, SecureChannel, now_ms};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::account::LinkState;
use crate::delivery::{Delivery, Tag, trackable};
use crate::error::{NetError, Result};
use crate::frame::{read_frame, write_frame};
use crate::handshake;
use crate::mailbox::MailboxStore;
use crate::onion::OnionState;
use crate::relay::RelayState;

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
        /// Set when the session runs over a relay circuit through this peer.
        via: Option<PublicIdentity>,
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
    /// A peer account was learned or changed (devices added / removed).
    AccountChanged {
        account: AccountId,
        added: Vec<PublicIdentity>,
        removed: Vec<PublicIdentity>,
    },
    /// A device presented a chain that forks one we hold: possible compromise.
    AccountFork {
        device: PublicIdentity,
        account: AccountId,
    },
    /// We linked a new device into our account.
    DeviceLinked {
        device: PublicIdentity,
    },
    /// A link request failed (bad code, expired, revoked device).
    LinkRejected {
        device: PublicIdentity,
    },
    /// One of our own devices sent us contact changes.
    ContactsSynced {
        from: PublicIdentity,
    },
    /// This device was removed from its account.
    ThisDeviceRemoved,
    /// A nearby approved peer created a Wi-Fi Direct group for us: join
    /// it, then connect to `offer.addr` pinning `peer`.
    WifiDirectOffer {
        peer: PublicIdentity,
        offer: crate::direct::DirectOffer,
    },
    /// A nearby approved peer asks us to create a Wi-Fi Direct group and
    /// offer it ([`Node::offer_wifi_direct`]).
    WifiDirectRequested {
        peer: PublicIdentity,
    },
    /// A peer changed the conversation's disappearing-message timer.
    TimerChanged {
        peer: PublicIdentity,
        secs: Option<u32>,
    },
    /// A sealed message from `from`, delivered by mailbox `via` (Appendix H).
    OfflineMessage {
        from: PublicIdentity,
        via: PublicIdentity,
        msg: AppMessage,
    },
    /// `peer` acknowledged an outgoing message: the history entry
    /// `local_id` (in `group`, else the 1:1 conversation) changed.
    ///
    /// With `relay_for`, it was a group message we forwarded for that
    /// member (`local_id` is its reference): nothing of ours changed, but
    /// the group layer owes it a receipt.
    Delivered {
        peer: PublicIdentity,
        local_id: u64,
        group: Option<[u8; 16]>,
        relay_for: Option<PublicIdentity>,
    },
    /// A mailbox reported what it did with our deposit for `to`.
    DepositReceipt {
        mailbox: PublicIdentity,
        to: PublicIdentity,
        status: crate::mailbox::DepositStatus,
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
    /// For relayed sessions, the neighbour the circuit runs through.
    pub via: Option<PublicIdentity>,
    /// The handshake transcript hash (public; binds link proofs).
    pub session_id: [u8; 32],
    /// Human-readable remote end (IP:port, Bluetooth address, …).
    pub remote: String,
}

/// How a session reaches its peer.
enum Route {
    /// A direct link; `Some(addr)` when we dialed a reusable address.
    Direct(Option<String>),
    /// A relay circuit (Appendix G) through this neighbour.
    Relay(PublicIdentity),
    /// An onion circuit (Appendix I) whose first hop is this neighbour.
    Onion(PublicIdentity),
    /// A direct link over another transport, e.g. Bluetooth LE.
    Link {
        transport: &'static str,
        remote: String,
    },
}

impl Route {
    /// `(dialed address, first hop, transport label)`.
    /// `(dialed address, first hop, transport label, remote description)`.
    fn into_parts(
        self,
        addr: SocketAddr,
    ) -> (Option<String>, Option<PublicIdentity>, &'static str, String) {
        match self {
            Self::Direct(dialed) => (dialed, None, "tcp", addr.to_string()),
            Self::Relay(v) => (None, Some(v), "relay", addr.to_string()),
            Self::Onion(v) => (None, Some(v), "onion", addr.to_string()),
            Self::Link { transport, remote } => (None, None, transport, remote),
        }
    }
}

struct SessionHandle {
    id: u64,
    tx: mpsc::UnboundedSender<AppMessage>,
    info: SessionInfo,
}

pub(crate) struct Shared {
    identity: Identity,
    pub(crate) home: Home,
    pub(crate) contacts: Mutex<Contacts>,
    sessions: Mutex<HashMap<PublicIdentity, SessionHandle>>,
    events: mpsc::UnboundedSender<Event>,
    policy: Mutex<AcceptPolicy>,
    constant_rate: Option<Duration>,
    tunnel: Option<(u16, [u8; 32])>,
    /// WireGuard keys peers offered us, for clean removal on revocation.
    tunnel_peers: Mutex<HashMap<PublicIdentity, [u8; 32]>>,
    pub(crate) relay: Mutex<RelayState>,
    pub(crate) prekeys: Mutex<PrekeyStore>,
    pub(crate) bundles: Mutex<BundleBook>,
    pub(crate) mailbox: Mutex<MailboxStore>,
    pub(crate) onion: Mutex<OnionState>,
    pub(crate) account: Mutex<AccountChain>,
    pub(crate) accounts: Mutex<AccountBook>,
    pub(crate) linking: Mutex<LinkState>,
    /// Shared prekey bundles of our own other devices, to forward.
    pub(crate) siblings: Mutex<HashMap<[u8; 32], PrekeyBundle>>,
    pub(crate) ble: Mutex<crate::discovery::BleState>,
    shutdown: tokio::sync::watch::Sender<bool>,
    next_id: AtomicU64,
    /// Acknowledgement bookkeeping for user content (see `delivery`).
    pub(crate) delivery: Mutex<Delivery>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a lock leaves plain data behind; keep going.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Shared {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub(crate) fn mutual(&self, peer: &PublicIdentity) -> bool {
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

    pub(crate) fn save_state(&self, name: &str, bytes: threnody_core::Result<impl AsRef<[u8]>>) {
        let r = bytes.and_then(|b| self.home.save_state(&self.identity, name, b.as_ref()));
        if let Err(e) = r {
            eprintln!("threnody: failed to save {name}: {e}");
        }
    }

    /// Runs `f` on the acknowledgement state and saves what it changed.
    pub(crate) fn delivery<R>(&self, f: impl FnOnce(&mut Delivery) -> R) -> R {
        let mut d = lock(&self.delivery);
        let r = f(&mut d);
        if std::mem::take(&mut d.unacked_dirty) {
            self.save_state("unacked", Ok(d.encode_unacked()));
        }
        if std::mem::take(&mut d.delivered_dirty) {
            self.save_state("delivered-ids", Ok(d.encode_delivered()));
        }
        r
    }

    pub(crate) fn persist_prekeys(&self, p: &PrekeyStore) {
        self.save_state("prekeys", p.encode());
    }

    pub(crate) fn persist_bundles(&self, b: &BundleBook) {
        self.save_state("bundles", b.encode());
    }

    pub(crate) fn persist_mailbox(&self, m: &MailboxStore) {
        self.save_state("mailbox", m.encode());
    }

    pub(crate) fn save_contacts(&self, c: &Contacts) {
        if let Err(e) = self.home.save_contacts(c) {
            eprintln!("threnody: failed to save contacts: {e}");
        }
    }
}

#[derive(Clone)]
pub struct Node {
    pub(crate) shared: Arc<Shared>,
}

impl Node {
    pub fn new(cfg: NodeConfig) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let contacts = cfg.home.load_contacts()?;
        let now = now_ms();
        let prekeys = match cfg.home.load_state(&cfg.identity, "prekeys")? {
            Some(b) => PrekeyStore::decode(&b)?,
            None => PrekeyStore::new(now),
        };
        let bundles = match cfg.home.load_state(&cfg.identity, "bundles")? {
            Some(b) => BundleBook::decode(&b)?,
            None => BundleBook::default(),
        };
        let account = match cfg.home.load_state(&cfg.identity, "account")? {
            Some(b) => AccountChain::decode(&b)?,
            None => {
                let chain = AccountChain::genesis(&cfg.identity, &device_name())?;
                cfg.home
                    .save_state(&cfg.identity, "account", &chain.encode()?)?;
                chain
            }
        };
        let siblings = match cfg.home.load_state(&cfg.identity, "siblings")? {
            Some(b) => crate::mailbox::decode_bundle_list(&b)?,
            None => HashMap::new(),
        };
        let accounts = match cfg.home.load_state(&cfg.identity, "accounts")? {
            Some(b) => AccountBook::decode(&b)?,
            None => AccountBook::default(),
        };
        let mailbox = match cfg.home.load_state(&cfg.identity, "mailbox")? {
            Some(b) => MailboxStore::decode(&b)?,
            None => MailboxStore::default(),
        };
        let delivery = {
            let load = |n| {
                cfg.home
                    .load_state(&cfg.identity, n)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            };
            Delivery::load(&load("unacked"), &load("delivered-ids"))
        };
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
            relay: Mutex::new(RelayState::default()),
            prekeys: Mutex::new(prekeys),
            bundles: Mutex::new(bundles),
            mailbox: Mutex::new(mailbox),
            onion: Mutex::new(OnionState::default()),
            ble: Mutex::default(),
            account: Mutex::new(account),
            accounts: Mutex::new(accounts),
            linking: Mutex::new(LinkState::default()),
            siblings: Mutex::new(siblings),
            shutdown: tokio::sync::watch::Sender::new(false),
            next_id: AtomicU64::new(1),
            delivery: Mutex::new(delivery),
        };
        let node = Self {
            shared: Arc::new(shared),
        };
        node.start_history_sweep();
        Ok((node, rx))
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
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                let accepted = tokio::select! {
                    a = listener.accept() => a,
                    () = &mut closed => break,
                };
                let Ok((stream, peer_addr)) = accepted else {
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
        self.spawn_session(stream, chan, addr, false, Route::Direct(None));
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
        self.spawn_session(
            stream,
            chan,
            peer_addr,
            true,
            Route::Direct(Some(addr.to_owned())),
        );
        Ok(peer)
    }

    /// Authenticates over an already-connected byte stream from any
    /// transport (e.g. a Bluetooth LE L2CAP channel) as the initiator.
    /// `remote` describes the other end for display.
    pub async fn connect_stream<S>(
        &self,
        mut stream: S,
        transport: &'static str,
        remote: String,
        expect: Option<Fingerprint>,
    ) -> Result<PublicIdentity>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
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
        self.spawn_session(
            stream,
            chan,
            SocketAddr::from(([0, 0, 0, 0], 0)),
            true,
            Route::Link { transport, remote },
        );
        Ok(peer)
    }

    /// Accepts a session over an already-connected byte stream (responder),
    /// applying the accept policy.
    pub async fn accept_stream<S>(
        &self,
        mut stream: S,
        transport: &'static str,
        remote: String,
    ) -> Result<PublicIdentity>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let chan = handshake::accept(&mut stream, &self.shared.identity).await?;
        self.check_policy(chan.peer())?;
        let peer = *chan.peer();
        self.spawn_session(
            stream,
            chan,
            SocketAddr::from(([0, 0, 0, 0], 0)),
            false,
            Route::Link { transport, remote },
        );
        Ok(peer)
    }

    /// Stops listening, discovery and every session. Persisted state
    /// stays on disk; the node can be recreated from the same home.
    pub fn shutdown(&self) {
        self.shared.shutdown.send_replace(true);
        lock(&self.shared.sessions).clear();
    }

    /// Resolves once [`Node::shutdown`] has been called.
    pub(crate) fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shared.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    }

    pub(crate) fn identity_ref(&self) -> &Identity {
        &self.shared.identity
    }

    /// Address shown for sessions and rejections that arrive over a relay.
    fn relay_addr(&self, via: &PublicIdentity) -> SocketAddr {
        self.sessions()
            .into_iter()
            .find(|s| s.peer == *via && s.via.is_none())
            .map_or(SocketAddr::from(([0, 0, 0, 0], 0)), |s| s.addr)
    }

    pub(crate) fn emit_rejected_relay(&self, via: PublicIdentity, reason: String) {
        let addr = self.relay_addr(&via);
        self.shared.emit(Event::Rejected {
            addr,
            reason: format!("via relay: {reason}"),
        });
    }

    pub(crate) fn spawn_relayed_session<S>(
        &self,
        stream: S,
        chan: SecureChannel,
        via: PublicIdentity,
        outbound: bool,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let addr = self.relay_addr(&via);
        self.spawn_session(stream, chan, addr, outbound, Route::Relay(via));
    }

    pub(crate) fn spawn_onion_session<S>(
        &self,
        stream: S,
        chan: SecureChannel,
        via: PublicIdentity,
        outbound: bool,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let addr = self.relay_addr(&via);
        self.spawn_session(stream, chan, addr, outbound, Route::Onion(via));
    }

    pub(crate) fn check_policy(&self, peer: &PublicIdentity) -> Result<()> {
        if self.is_revoked(peer) {
            return Err(NetError::Refused(format!(
                "{} (device removed from its account)",
                peer.fingerprint()
            )));
        }
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

    /// Queues a message to a connected peer. User content (text, files,
    /// group and mailbox messages) is kept until the peer acknowledges it
    /// and sent again in the next session if this one dies first.
    pub fn send(&self, peer: &PublicIdentity, msg: AppMessage) -> Result<()> {
        self.send_tagged(peer, msg, Tag::NONE)
    }

    /// [`Node::send`], marking the history entry `tag` names delivered
    /// when the peer acknowledges.
    pub fn send_tagged(&self, peer: &PublicIdentity, msg: AppMessage, tag: Tag) -> Result<()> {
        let sessions = lock(&self.shared.sessions);
        let h = sessions.get(peer).ok_or(NetError::Closed)?;
        let msg = if trackable(&msg) {
            self.shared.delivery(|d| d.track(peer, msg, tag))
        } else {
            msg
        };
        h.tx.send(msg).map_err(|_| NetError::Closed)
    }

    /// How many messages to `peer` are waiting for an acknowledgement.
    pub fn unacked(&self, peer: &PublicIdentity) -> usize {
        lock(&self.shared.delivery).unacked(peer)
    }

    /// Sets our approval of `peer`, persists it and tells the peer if online.
    /// Revoking (`false`) takes effect immediately on both ends' mutual state.
    pub fn set_approval(&self, peer: &PublicIdentity, approved: bool) -> Result<()> {
        {
            let mut contacts = lock(&self.shared.contacts);
            contacts.observe(*peer, None, now_ms());
            if let Some(c) = contacts.get_mut(peer) {
                c.local_approved = approved;
                c.approval_changed_ms = now_ms();
                if !approved {
                    c.clear_discovery_keys();
                }
            }
            self.shared.save_contacts(&contacts);
        }
        if !approved {
            self.shared.tunnel_down(peer);
        }
        let _ = self.send(peer, AppMessage::Approval { approved });
        self.push_contact_sync();
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
        route: Route,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (dialed, via, transport, remote) = route.into_parts(addr);
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
            transport,
            since_ms: now_ms(),
            outbound,
            via,
            session_id: *chan.session_id(),
            remote,
        };
        // Whatever the last session didn't get acknowledged goes first.
        for m in lock(&self.shared.delivery).resend(&peer) {
            let _ = tx.send(m);
        }
        // A newer session to the same peer replaces the old one; dropping
        // the old handle's sender ends its task.
        lock(&self.shared.sessions).insert(peer, SessionHandle { id, tx, info });
        self.shared.emit(Event::Connected {
            peer,
            addr,
            suite: chan.suite(),
            new_contact,
            via,
        });

        let node = self.clone();
        tokio::spawn(async move {
            let reason = match run_session(&node, stream, chan, addr, via, rx).await {
                Ok(()) => "closed".to_owned(),
                Err(e) => e.to_string(),
            };
            let current = {
                let mut sessions = lock(&node.shared.sessions);
                // Absent (disconnected locally) or still ours: either way no
                // newer session has replaced this one.
                let current = !sessions.get(&peer).is_some_and(|h| h.id != id);
                if sessions.get(&peer).is_some_and(|h| h.id == id) {
                    sessions.remove(&peer);
                }
                current
            };
            // Circuits relayed over this link die with it (unless a newer
            // session to the same peer already took over).
            if current && via.is_none() {
                node.drop_circuits_of(&peer);
                node.drop_onion_circuits_of(&peer);
            }
            // A replaced session (e.g. Bluetooth upgraded to Wi-Fi Direct)
            // is not a disconnection: the peer is still connected.
            if current {
                node.shared.emit(Event::Disconnected { peer, reason });
            }
        });
    }
}

async fn run_session<S>(
    node: &Node,
    stream: S,
    mut chan: SecureChannel,
    addr: SocketAddr,
    via: Option<PublicIdentity>,
    mut outbox: mpsc::UnboundedReceiver<AppMessage>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let shared: &Shared = &node.shared;
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
    // Each side's first message is its Hello, so the other learns its
    // features before anything else.
    let mut pending: VecDeque<AppMessage> = VecDeque::new();
    pending.push_back(AppMessage::Hello {
        features: FEATURE_ACKS,
    });
    pending.push_back(AppMessage::Approval {
        approved: local_approved,
    });
    // Whether the peer acknowledges `Tracked` messages: known from its
    // first message. Until then, tracked messages wait.
    let mut peer_acks: Option<bool> = None;

    let mut ticker = shared.constant_rate.map(|d| {
        let mut t = tokio::time::interval(d);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t
    });

    let mut offered = false;
    let mut discovery_keyed = false;
    let mut prekeys_sent = false;
    // Our account chain (and, for own devices, our contacts), then anything
    // mailboxes held for this peer.
    pending.extend(node.account_hello(&peer));
    {
        // Held deliveries leave the mailbox store now; tracking keeps them
        // until the peer has them.
        let held = node.mailbox_for(&peer);
        shared.delivery(|d| pending.extend(held.into_iter().map(|m| d.track(&peer, m, Tag::NONE))));
    }
    let result: Result<()> = async {
        loop {
            // Refresh the pairwise LAN discovery key once per session.
            if !discovery_keyed && shared.mutual(&peer) {
                let key = *chan.export(DISCOVERY_CONTEXT);
                let mut contacts = lock(&shared.contacts);
                if let Some(c) = contacts.get_mut(&peer) {
                    c.set_discovery_key(key);
                }
                shared.save_contacts(&contacts);
                discovery_keyed = true;
            }
            // Hand a mutually approved peer fresh prekeys once per session.
            if !prekeys_sent && shared.mutual(&peer) {
                pending.extend(node.prekeys_for(&peer));
                prekeys_sent = true;
            }
            // Offer a tunnel once per session, as soon as approval is mutual.
            // Tunnels need a direct UDP path, so never over relayed sessions.
            if let Some((port, wg_public)) = shared.tunnel
                && via.is_none()
                && !offered
                && shared.mutual(&peer)
            {
                pending.push_back(AppMessage::TunnelOffer { wg_public, port });
                offered = true;
            }
            if ticker.is_none() && chan.can_send() {
                let mut waiting = VecDeque::new();
                while let Some(m) = pending.pop_front() {
                    match ready(shared, &peer, m, peer_acks) {
                        Ok(m) => write_frame(&mut wr, &chan.seal(&m)?).await?,
                        Err(m) => waiting.push_back(m),
                    }
                }
                pending = waiting;
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
                    let opened = chan.open(&frame)?;
                    if peer_acks.is_none() {
                        peer_acks = Some(matches!(
                            opened,
                            AppMessage::Hello { features } if features & FEATURE_ACKS != 0
                        ));
                    }
                    let msg = match opened {
                        AppMessage::Tracked { id, inner } => {
                            // Acknowledge every copy (the last ack may have
                            // been lost), deliver only the first.
                            match pending.back_mut() {
                                Some(AppMessage::Ack(ids)) if ids.len() < MAX_ACK_IDS => ids.push(id),
                                _ => pending.push_back(AppMessage::Ack(vec![id])),
                            }
                            shared.delivery(|d| d.first_delivery(&peer, id)).then_some(*inner)
                        }
                        AppMessage::Ack(ids) => {
                            for tag in shared.delivery(|d| d.acked(&peer, &ids)) {
                                node.mark_delivered(&peer, tag);
                            }
                            None
                        }
                        m => Some(m),
                    };
                    let Some(msg) = msg else { continue };
                    match msg {
                        AppMessage::Hello { .. }
                        | AppMessage::Cover
                        | AppMessage::Tracked { .. }
                        | AppMessage::Ack(_) => {}
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
                                    prekeys_sent = false;
                                    let mut contacts = lock(&shared.contacts);
                                    if let Some(c) = contacts.get_mut(&peer) {
                                        c.clear_discovery_keys();
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
                        AppMessage::Prekeys(payload) => node.on_prekeys(peer, &payload),
                        AppMessage::Account(payload) => node.on_account(peer, &payload),
                        AppMessage::Mailbox(payload) => node.on_mailbox(peer, &payload),
                        AppMessage::Onion(payload) => {
                            if via.is_none() {
                                node.on_onion(peer, &payload);
                            }
                        }
                        AppMessage::Direct(payload) => {
                            if via.is_none() {
                                node.on_direct(peer, &payload);
                            }
                        }
                        AppMessage::Relay(payload) => {
                            // Circuits only ride on direct links, never nest.
                            if via.is_none() {
                                node.on_relay(peer, &payload);
                            }
                        }
                        msg @ (AppMessage::Text { .. } | AppMessage::File { .. } | AppMessage::Group(_)) => {
                            node.record_incoming(&peer, &msg, false);
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
                        // The first message that can go now, else cover.
                        let mut m = AppMessage::Cover;
                        for _ in 0..pending.len() {
                            let Some(next) = pending.pop_front() else { break };
                            match ready(shared, &peer, next, peer_acks) {
                                Ok(r) => {
                                    m = r;
                                    break;
                                }
                                Err(w) => pending.push_back(w),
                            }
                        }
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

/// What to send for `m` now: tracked messages wait until we know whether
/// the peer acknowledges them, and go out plain (untracked) if it doesn't.
fn ready(
    shared: &Shared,
    peer: &PublicIdentity,
    m: AppMessage,
    peer_acks: Option<bool>,
) -> std::result::Result<AppMessage, AppMessage> {
    match (m, peer_acks) {
        (m @ AppMessage::Tracked { .. }, None) => Err(m),
        (AppMessage::Tracked { id, inner }, Some(false)) => {
            shared.delivery(|d| d.forget(peer, id));
            Ok(*inner)
        }
        (m, _) => Ok(m),
    }
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

/// A default device name: the host name, else "device".
fn device_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "device".to_owned())
}
