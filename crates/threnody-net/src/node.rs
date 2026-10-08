//! A running Threnody node: listens, dials, runs one task per session,
//! enforces the trust policy and keeps the contact book current.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Context};
use threnody_core::account::{AccountBook, AccountChain, AccountId};
use threnody_core::crypto::aead::Suite;
use threnody_core::discovery::DISCOVERY_CONTEXT;
use threnody_core::message::{
    FEATURE_ACKS, FEATURE_OBSERVED, FEATURE_PATHS, FEATURES, MAX_ACK_IDS,
};
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
    /// using cover messages when idle (spec §9, layer 1). Can be changed
    /// later with [`Node::set_constant_rate`]; running sessions follow.
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

/// A peer's WireGuard tunnel info.
#[derive(Clone)]
pub struct TunnelPeer {
    pub wg_public: [u8; 32],
    pub endpoint: SocketAddr,
    pub overlay: Ipv6Addr,
    pub psk: Secret,
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
    /// Our own device `from` sent history entries; `added` were new here.
    HistorySynced {
        from: PublicIdentity,
        added: usize,
    },
    /// `peer` (or one of our devices) changed reactions on message `id`.
    Reacted {
        peer: PublicIdentity,
        id: u64,
    },
    /// `peer` says it displayed our outgoing messages, so their ticks can
    /// show "seen".
    Read {
        peer: PublicIdentity,
    },
    /// `peer` changed what it shares of its profile (see `Contact::profile`).
    ProfileChanged {
        peer: PublicIdentity,
    },
    /// `peer` (a persona, to us) revealed its main identity, proven by a
    /// signature from it; `invite` reaches it. Nothing is added: the user
    /// decides.
    IdentityRevealed {
        peer: PublicIdentity,
        identity: PublicIdentity,
        invite: Option<String>,
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
    Typing {
        peer: PublicIdentity,
        active: bool,
    },
    /// A sealed message from `from`, delivered by mailbox `via` (Appendix H).
    OfflineMessage {
        from: PublicIdentity,
        via: PublicIdentity,
        msg: AppMessage,
    },
    /// `peer` edited a message (its own; or, from our own device, ours).
    MessageEdited {
        peer: PublicIdentity,
        id: u64,
    },
    /// `peer` deleted messages: ours on its behalf (its own other device),
    /// or its own for everyone. `count` entries went from our history.
    MessagesDeleted {
        peer: PublicIdentity,
        count: usize,
    },
    /// A message from someone we haven't accepted yet (see
    /// `Node::accept_contact`). It is in history, but shouldn't be shown
    /// with the user's conversations or notified as one.
    MessageRequest {
        peer: PublicIdentity,
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
        /// Left over an onion circuit, so the mailbox didn't learn who
        /// sent it.
        anonymous: bool,
    },
    /// An automatic dial (e.g. after discovery) failed.
    DialFailed {
        peer: PublicIdentity,
        addr: SocketAddr,
        reason: String,
    },
    /// Our addresses for contacts across the internet changed (Appendix N).
    Addresses {
        candidates: Vec<SocketAddr>,
        symmetric: bool,
    },
    /// Hole punching toward `peer` started at `targets`; `dialing` when
    /// this side dials.
    Punching {
        peer: PublicIdentity,
        targets: Vec<SocketAddr>,
        dialing: bool,
    },
    /// What internet rendezvous is doing (records published and found),
    /// for diagnostic logs.
    ReachNote {
        note: String,
    },
    /// Progress with relay directories or volunteering (Appendix P).
    VolunteerNote {
        note: String,
    },
    /// A peer offers us a credential (Appendix O): accept or decline it.
    CredentialOffered {
        offer: crate::cred::CredentialOffer,
    },
    /// A credential we accepted arrived and is kept.
    CredentialReceived {
        peer: PublicIdentity,
        schema: String,
    },
    /// A peer asks us to prove attributes: present or decline.
    CredentialAsked {
        ask: crate::cred::CredentialAsk,
    },
    /// A peer proved attributes we asked for.
    CredentialPresented {
        peer: PublicIdentity,
        id: u64,
        verified: threnody_core::credential::Verified,
    },
    /// A credential exchange failed or was declined.
    CredentialFailed {
        peer: PublicIdentity,
        id: u64,
        reason: String,
    },
    /// Hole punching toward `peer` found no path; relays and mailboxes
    /// still work.
    PunchFailed {
        peer: PublicIdentity,
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
pub(crate) enum Route {
    /// A direct link; `Some(addr)` when we dialed a reusable address.
    Direct(Option<String>),
    /// A relay circuit (Appendix G) through this neighbour.
    Relay(PublicIdentity),
    /// An onion circuit (Appendix I) whose first hop is this neighbour.
    Onion(PublicIdentity),
    /// A direct QUIC connection (Appendix N).
    Quic,
    /// A QUIC connection from our standby endpoint (see `recover`): what
    /// the peer sees there is the standby network's address, not ours.
    QuicStandby,
    /// A direct link over another transport, e.g. Bluetooth LE.
    Link {
        transport: &'static str,
        remote: String,
    },
}

impl Route {
    /// `(dialed address, first hop, transport label, remote description)`.
    fn into_parts(
        self,
        addr: SocketAddr,
    ) -> (Option<String>, Option<PublicIdentity>, &'static str, String) {
        match self {
            Self::Direct(dialed) => (dialed, None, "tcp", addr.to_string()),
            Self::Relay(v) => (None, Some(v), "relay", addr.to_string()),
            Self::Onion(v) => (None, Some(v), "onion", addr.to_string()),
            // Punched paths are ephemeral: nothing to remember for redialing.
            Self::Quic | Self::QuicStandby => (None, None, "quic", addr.to_string()),
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
    /// The cover-traffic interval; sessions watch it (see `run_session`).
    pub(crate) constant_rate: tokio::sync::watch::Sender<Option<Duration>>,
    /// Dial contacts through onion circuits first when possible.
    pub(crate) prefer_onion: std::sync::atomic::AtomicBool,
    /// An anonymous identity (`threnody_core::persona`): no device linking
    /// or renaming, nothing that could tie it to another identity.
    pub(crate) persona: bool,
    /// Our profile attributes; each contact sees those shared with it.
    pub(crate) profile: Mutex<threnody_core::persona::Profile>,
    /// Strip identifying metadata from images we send.
    pub(crate) strip_metadata: std::sync::atomic::AtomicBool,
    /// Disappearing timer (s) for conversations without one; 0 = off.
    pub(crate) default_timer: std::sync::atomic::AtomicU32,
    tunnel: Mutex<Option<(u16, [u8; 32])>>,
    /// WireGuard peers offered us, for config and live apply.
    tunnel_peers: Mutex<HashMap<PublicIdentity, TunnelPeer>>,
    /// Userspace WireGuard (boringtun) state.
    #[cfg(feature = "boringtun")]
    wg_userspace: Mutex<Option<crate::wg_userspace::WgUserspace>>,
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
    /// What each peer's latest `Hello` said it supports.
    pub(crate) peer_features: Mutex<HashMap<PublicIdentity, u64>>,
    /// How far each of our other devices has our history (see `sync`).
    pub(crate) sync: Mutex<crate::sync::SyncState>,
    /// The QUIC endpoint, once listening (see `quic`).
    pub(crate) quic: Mutex<Option<Arc<crate::quic::Quic>>>,
    /// Reaching contacts across the internet (see `reach`).
    pub(crate) reach: crate::reach::ReachState,
    /// Repairing lost sessions fast (see `recover`).
    pub(crate) recover: crate::recover::RecoverState,
    /// Anonymous links (see `anon`).
    pub(crate) anon: Mutex<crate::anon::AnonState>,
    /// Directories, volunteering and relay tokens (see `volunteer`).
    pub(crate) volunteer: crate::volunteer::VolunteerState,
    /// Credentials held and exchanges in progress (see `cred`).
    pub(crate) creds: Mutex<crate::cred::CredState>,
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
        if lock(&self.tunnel).is_some() {
            let removed = lock(&self.tunnel_peers).remove(peer);
            self.emit(Event::TunnelDown {
                peer: *peer,
                wg_public: removed.map(|tp| tp.wg_public),
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
        let persona = threnody_core::persona::is_persona(&cfg.home);
        let profile = cfg
            .home
            .load_state(&cfg.identity, "profile")?
            .map(|b| crate::identity::decode_profile(&b))
            .unwrap_or_default();
        let account = match cfg.home.load_state(&cfg.identity, "account")? {
            Some(b) => AccountChain::decode(&b)?,
            None => {
                // A persona's chain names nothing about the machine.
                let name = if persona {
                    "device".to_owned()
                } else {
                    device_name()
                };
                let chain = AccountChain::genesis(&cfg.identity, &name)?;
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
        let sync = crate::sync::SyncState::decode(
            &cfg.home
                .load_state(&cfg.identity, "history-sync")
                .ok()
                .flatten()
                .unwrap_or_default(),
        );
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
            constant_rate: tokio::sync::watch::Sender::new(cfg.constant_rate),
            prefer_onion: std::sync::atomic::AtomicBool::new(true),
            persona,
            profile: Mutex::new(profile),
            strip_metadata: std::sync::atomic::AtomicBool::new(true),
            default_timer: std::sync::atomic::AtomicU32::new(0),
            tunnel: Mutex::new(tunnel),
            tunnel_peers: Mutex::new(HashMap::new()),
            #[cfg(feature = "boringtun")]
            wg_userspace: Mutex::new(None),
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
            sync: Mutex::new(sync),
            peer_features: Mutex::new(HashMap::new()),
            quic: Mutex::new(None),
            reach: crate::reach::ReachState::new(!persona),
            recover: crate::recover::RecoverState::new(),
            anon: Mutex::default(),
            volunteer: crate::volunteer::VolunteerState::new(),
            creds: Mutex::default(),
        };
        let node = Self {
            shared: Arc::new(shared),
        };
        node.start_history_sweep();
        node.load_recovery();
        node.load_volunteer();
        node.load_credentials();
        node.start_directory_upkeep();
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
        *self.shared.constant_rate.borrow()
    }

    /// Sets the cover-traffic interval (`None` = off). Running sessions
    /// switch to it at once.
    pub fn set_constant_rate(&self, rate: Option<Duration>) {
        self.shared.constant_rate.send_replace(rate);
    }

    /// Whether contacts are reached through onion circuits first, when two
    /// approved relays make one possible (on by default).
    pub fn prefer_onion(&self) -> bool {
        self.shared.prefer_onion.load(Ordering::Relaxed)
    }

    pub fn set_prefer_onion(&self, on: bool) {
        self.shared.prefer_onion.store(on, Ordering::Relaxed);
    }

    /// Whether images we send lose their metadata (location, camera,
    /// times) first (on by default; see `threnody_core::media`).
    pub fn strip_metadata(&self) -> bool {
        self.shared.strip_metadata.load(Ordering::Relaxed)
    }

    pub fn set_strip_metadata(&self, on: bool) {
        self.shared.strip_metadata.store(on, Ordering::Relaxed);
    }

    /// WireGuard listen port, when tunnels are enabled.
    pub fn tunnel_port(&self) -> Option<u16> {
        lock(&self.shared.tunnel).map(|(p, _)| p)
    }

    /// Peers that currently have a tunnel configured.
    pub fn tunnel_peers(&self) -> Vec<PublicIdentity> {
        lock(&self.shared.tunnel_peers).keys().copied().collect()
    }

    /// Returns full tunnel peer info for config generation.
    pub fn tunnel_peers_full(&self) -> Vec<TunnelPeer> {
        lock(&self.shared.tunnel_peers).values().cloned().collect()
    }

    /// Returns tunnel peer entries with their identities for userspace WireGuard.
    pub fn tunnel_peer_entries(&self) -> Vec<(PublicIdentity, TunnelPeer)> {
        lock(&self.shared.tunnel_peers).iter().map(|(k, v)| (*k, v.clone())).collect()
    }

    /// Sets the WireGuard tunnel port. Changing this restarts tunnel state.
    pub fn set_tunnel_port(&self, port: Option<u16>) -> Result<()> {
        let new_tunnel = port.map(|p| (p, *WgKeys::derive(&self.shared.identity).public()));
        *lock(&self.shared.tunnel) = new_tunnel;
        // Clear existing tunnel peers since port changed
        lock(&self.shared.tunnel_peers).clear();
        Ok(())
    }

    /// Applies current tunnel peers to a WireGuard interface via `wg set`.
    /// Requires CAP_NET_ADMIN or root.
    pub fn apply_wireguard(&self, iface: &str) -> Result<()> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let keys = WgKeys::derive(&self.shared.identity);
        let port = self.tunnel_port().unwrap_or(51820);

        // First, set the interface's private key and listen port
        let mut child = Command::new("wg")
            .args([
                "set",
                iface,
                "listen-port",
                &port.to_string(),
                "private-key",
                "/dev/stdin",
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("running wg (is wireguard-tools installed?)")?;
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(keys.secret_base64().as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(anyhow::anyhow!(
                "wg set listen-port/private-key: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }

        // Then add/update each peer
        for peer in self.tunnel_peers_full() {
            let psk = zeroize::Zeroizing::new(threnody_core::tunnel::base64(&peer.psk.0[..]));
            let mut child = Command::new("wg")
                .args([
                    "set",
                    iface,
                    "peer",
                    &threnody_core::tunnel::base64(&peer.wg_public),
                    "preshared-key",
                    "/dev/stdin",
                    "endpoint",
                    &peer.endpoint.to_string(),
                    "allowed-ips",
                    &format!("{}/128", peer.overlay),
                    "persistent-keepalive",
                    "25",
                ])
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .context("running wg")?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(psk.as_bytes())?;
            let status = child.wait_with_output()?;
            if !status.status.success() {
                return Err(anyhow::anyhow!(
                    "wg set peer: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                )
                .into());
            }
        }

        Ok(())
    }

    /// Exports all node state as an encrypted backup (identity, contacts,
    /// history, groups, prekeys, etc.). The backup is encrypted with a key
    /// derived from the identity seed.
    pub fn export_backup(&self) -> Result<Vec<u8>> {
        self.shared.home.export_backup(&self.shared.identity).map_err(|e| NetError::External(anyhow::anyhow!(e)))
    }

    /// Imports a backup, replacing all node state. The backup must have been
    /// created by the same identity.
    pub fn import_backup(&self, backup_data: &[u8]) -> Result<()> {
        self.shared.home.import_backup(&self.shared.identity, backup_data).map_err(|e| NetError::External(anyhow::anyhow!(e)))
    }

    /// Starts the userspace WireGuard implementation (boringtun).
    /// Returns the local UDP port being listened on.
    #[cfg(feature = "boringtun")]
    pub fn start_wg_userspace(&self) -> Result<u16> {
        let mut wg = self.shared.wg_userspace.lock().unwrap();
        if wg.is_some() {
            return Ok(wg.as_ref().unwrap().listen_port());
        }

        let identity = Identity::from_seed(&*self.shared.identity.seed());
        let port = self.tunnel_port().unwrap_or(threnody_core::tunnel::DEFAULT_PORT);
        let mut wg_userspace = crate::wg_userspace::WgUserspace::new(&identity, port)?;

        // Add existing tunnel peers
        for (peer_id, peer) in self.tunnel_peer_entries() {
            let config = crate::wg_userspace::WgPeerConfig {
                peer_identity: peer_id,
                peer_wg_public: peer.wg_public,
                endpoint: peer.endpoint,
                overlay: peer.overlay,
                psk: peer.psk,
            };
            wg_userspace.add_peer(config)?;
        }

        wg_userspace.start()?;
        let listen_port = wg_userspace.listen_port();
        *wg = Some(wg_userspace);
        Ok(listen_port)
    }

    /// Stops the userspace WireGuard implementation.
    #[cfg(feature = "boringtun")]
    pub fn stop_wg_userspace(&self) {
        let mut wg = self.shared.wg_userspace.lock().unwrap();
        if let Some(mut wg_userspace) = wg.take() {
            wg_userspace.stop();
        }
    }

    /// Sends a packet through the userspace WireGuard tunnel to a peer.
    #[cfg(feature = "boringtun")]
    pub fn wg_userspace_send(&self, peer: &PublicIdentity, data: &[u8]) -> Result<()> {
        let mut wg = self.shared.wg_userspace.lock().unwrap();
        if let Some(wg_userspace) = wg.as_mut() {
            let mut out_buf = [0u8; 65536];
            let len = wg_userspace.encapsulate(peer, data, &mut out_buf)?;
            if len > 0 {
                if let Some(socket) = wg_userspace.udp_socket() {
                    if let Some(config) = wg_userspace.peer_config(peer) {
                        let socket = socket.clone();
                        let data = out_buf[..len].to_vec();
                        let endpoint = config.endpoint;
                        tokio::spawn(async move {
                            let _ = socket.send_to(&data, endpoint).await;
                        });
                    }
                }
            }
            Ok(())
        } else {
            Err(crate::error::NetError::External(anyhow!("userspace WireGuard not started")))
        }
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
        tune_tcp(&stream);
        let first = tokio::time::timeout(handshake::HANDSHAKE_TIMEOUT, read_frame(&mut stream))
            .await
            .map_err(|_| NetError::Timeout)??
            .ok_or(NetError::Closed)?;
        // An anonymous link (Appendix P) announces itself before the handshake.
        if first == crate::anon::MAGIC {
            return self.accept_anon(stream, addr).await;
        }
        let chan = handshake::accept_first(&mut stream, &self.shared.identity, &first).await?;
        self.check_policy(chan.peer())?;
        self.spawn_session(stream, chan, addr, false, Route::Direct(None));
        Ok(())
    }

    /// Dials `addr`, authenticates, and starts a session. With `expect`, the
    /// peer must present that fingerprint (pinning from an out-of-band
    /// invite); otherwise the outbound connection is itself the user's
    /// decision to trust on first use.
    pub async fn connect(&self, addr: &str, expect: Option<Fingerprint>) -> Result<PublicIdentity> {
        let mut stream = match TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(e) => {
                // The same address may still answer over QUIC (UDP).
                if self.quic().is_some()
                    && let Some(sa) = tokio::net::lookup_host(addr)
                        .await
                        .ok()
                        .and_then(|mut a| a.next())
                    && let Ok(peer) = self.connect_quic(sa, expect).await
                {
                    return Ok(peer);
                }
                return Err(e.into());
            }
        };
        tune_tcp(&stream);
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

    pub(crate) fn is_closed(&self) -> bool {
        *self.shared.shutdown.borrow()
    }

    /// Resolves once [`Node::shutdown`] has been called.
    pub(crate) fn closed(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shared.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    }

    pub fn identity_ref(&self) -> &Identity {
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
        if contacts.get(peer).is_some_and(|c| c.blocked) {
            return Err(NetError::Refused(format!(
                "{} (blocked)",
                peer.fingerprint()
            )));
        }
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
        self.queue(peer, msg, tag, false)
    }

    /// Sends `msg` tracked (acknowledged, resent if lost) whatever its kind.
    pub fn send_tracked(&self, peer: &PublicIdentity, msg: AppMessage) -> Result<()> {
        self.queue(peer, msg, Tag::NONE, true)
    }

    fn queue(&self, peer: &PublicIdentity, msg: AppMessage, tag: Tag, always: bool) -> Result<()> {
        let sessions = lock(&self.shared.sessions);
        let h = sessions.get(peer).ok_or(NetError::Closed)?;
        let msg = if always || trackable(&msg) {
            self.shared.delivery(|d| d.track(peer, msg, tag))
        } else {
            msg
        };
        h.tx.send(msg).map_err(|_| NetError::Closed)
    }

    /// Keeps `msg` for `peer` until a session with it comes up, when it
    /// goes first (see `spawn_session`), as anything a lost session left
    /// unacknowledged does.
    pub(crate) fn hold(&self, peer: &PublicIdentity, msg: AppMessage, tag: Tag) {
        self.shared.delivery(|d| {
            d.track(peer, msg, tag);
        });
    }

    /// Whether `peer` said (in its latest session) it supports `feature`.
    pub fn supports(&self, peer: &PublicIdentity, feature: u64) -> bool {
        lock(&self.shared.peer_features)
            .get(peer)
            .is_some_and(|f| f & feature != 0)
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
                if approved {
                    // Approving is the strongest way of accepting.
                    c.accepted = true;
                    c.blocked = false;
                } else {
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

    /// Two direct sessions with `peer` that start within a few seconds of
    /// each other came from both sides dialing at once. Newest-wins would
    /// let each side keep a different one and lose the other, over and
    /// over; so both keep the one dialed by the smaller identity key. True
    /// if the new session (`outbound` from our side) is the one to drop.
    fn loses_duplicate(&self, peer: &PublicIdentity, outbound: bool) -> bool {
        let sessions = lock(&self.shared.sessions);
        let Some(old) = sessions.get(peer) else {
            return false;
        };
        if old.info.via.is_some()
            || now_ms().saturating_sub(old.info.since_ms) > DUPLICATE_WINDOW_MS
            || old.info.outbound == outbound
        {
            return false;
        }
        let me = self.identity();
        let dialer = |out: bool| if out { me } else { *peer };
        dialer(old.info.outbound).as_bytes() < dialer(outbound).as_bytes()
    }

    pub(crate) fn spawn_session<S>(
        &self,
        stream: S,
        chan: SecureChannel,
        addr: SocketAddr,
        outbound: bool,
        route: Route,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // Over QUIC the peer's address is its outside address, worth telling it.
        let over = match route {
            Route::Quic => Over::Quic(addr),
            Route::QuicStandby => Over::Standby,
            _ => Over::Other,
        };
        let (dialed, via, transport, remote) = route.into_parts(addr);
        let peer = *chan.peer();
        if via.is_none() && self.loses_duplicate(&peer, outbound) {
            // Both sides dialed at once; the other session stays (dropping
            // this stream closes it).
            return;
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let new_contact = {
            let mut contacts = lock(&self.shared.contacts);
            // Only remember dialable addresses: an inbound source port is ephemeral.
            let is_new = contacts.observe(peer, dialed, now_ms());
            // We dialed them: we want to hear from them.
            if outbound && let Some(c) = contacts.get_mut(&peer) {
                c.accepted = true;
            }
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
        let note = format!(
            "session with {} {} over {transport} at {addr}",
            peer.fingerprint(),
            if outbound { "dialed" } else { "accepted" },
        );
        let old = lock(&self.shared.sessions).insert(peer, SessionHandle { id, tx, info });
        self.shared.emit(Event::ReachNote {
            note: match old {
                Some(o) => format!(
                    "{note}, replacing one {} {} ms old",
                    if o.info.outbound {
                        "dialed"
                    } else {
                        "accepted"
                    },
                    now_ms().saturating_sub(o.info.since_ms)
                ),
                None => note,
            },
        });
        self.shared.emit(Event::Connected {
            peer,
            addr,
            suite: chan.suite(),
            new_contact,
            via,
        });

        let node = self.clone();
        tokio::spawn(async move {
            let result = run_session(&node, stream, chan, addr, via, over, rx).await;
            // Closed on purpose (either side) is not a loss to repair.
            let failed = result.is_err();
            let reason = match result {
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
                if via.is_none() && failed {
                    node.on_session_lost(peer);
                }
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
    over: Over,
    mut outbox: mpsc::UnboundedReceiver<AppMessage>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let observed = match over {
        Over::Quic(a) => Some(a),
        _ => None,
    };
    let on_standby = matches!(over, Over::Standby);
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
    pending.push_back(AppMessage::Hello { features: FEATURES });
    pending.push_back(AppMessage::Approval {
        approved: local_approved,
    });
    // Whether the peer acknowledges `Tracked` messages: known from its
    // first message. Until then, tracked messages wait.
    let mut peer_acks: Option<bool> = None;

    let make_ticker = |rate: Option<Duration>| {
        rate.map(|d| {
            let mut t = tokio::time::interval(d);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        })
    };
    let mut rate = shared.constant_rate.subscribe();
    // Inside a relay or onion circuit the links' cover traffic hides the
    // timing (Appendix I); cover from this session too would send more
    // cells than a constant-rate link carries, and the backlog would grow
    // without end.
    let inner = via.is_some();
    let mut ticker = make_ticker(rate.borrow_and_update().filter(|_| !inner));

    let mut offered = false;
    let mut paths_sent = false;
    // Silence longer than this ends the session (see `recover`): known once
    // the peer's `Paths` says how often it sends.
    let mut lease: Option<Duration> = None;
    // `Paths` that arrived before approval was mutual (the peer's
    // `Approval` can trail them), kept until it is.
    let mut early_paths: Option<Vec<u8>> = None;
    let mut last_rx = tokio::time::Instant::now();
    let setup_until = last_rx + SETUP_BURST;
    let mut heartbeat = tokio::time::interval(node.beat());
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                {
                    let mut contacts = lock(&shared.contacts);
                    if let Some(c) = contacts.get_mut(&peer) {
                        c.set_discovery_key(key);
                    }
                    shared.save_contacts(&contacts);
                }
                if via.is_none() {
                    let slot = *chan.export(threnody_core::rendezvous::RECOVERY_CONTEXT);
                    node.session_up(peer, slot, observed);
                }
                discovery_keyed = true;
            }
            if shared.mutual(&peer)
                && let Some(payload) = early_paths.take()
            {
                lease = node.on_paths(peer, &payload).or(lease);
            }
            // Tell an approved peer how to find us if this session breaks.
            // Over relay and onion circuits too: the endpoints are mutual
            // contacts, and a persistent circuit to an always-on contact
            // (Appendix N) exists precisely to carry these updates fast.
            if !paths_sent
                && peer_acks.is_some()
                && node.supports(&peer, FEATURE_PATHS)
                && shared.mutual(&peer)
            {
                // Ahead of everything but `Hello`: with cover traffic each
                // message waits for a tick, and a short session must still
                // get its lease and recovery paths across.
                // Behind `Hello` and our `Approval`, though: the peer only
                // takes paths from a peer it knows approves it.
                if let Some(m) = node.paths_message() {
                    let at = pending
                        .iter()
                        .rposition(|m| matches!(m, AppMessage::Hello { .. } | AppMessage::Approval { .. }))
                        .map_or(0, |i| i + 1);
                    pending.insert(at, m);
                }
                paths_sent = true;
            }
            // Hand a mutually approved peer fresh prekeys once per session.
            if !prekeys_sent && shared.mutual(&peer) {
                pending.extend(node.prekeys_for(&peer));
                prekeys_sent = true;
            }
            // Offer a tunnel once per session, as soon as approval is mutual.
            // Tunnels need a direct UDP path, so never over relayed sessions.
            if let Some((port, wg_public)) = *lock(&shared.tunnel)
                && via.is_none()
                && !offered
                && shared.mutual(&peer)
            {
                pending.push_back(AppMessage::TunnelOffer { wg_public, port });
                offered = true;
            }
            // Session setup (features, approval, recovery paths) goes at
            // once even under cover traffic: its timing is already visible
            // from the handshake, and a session that dies young must still
            // have its lease and paths.
            if ticker.is_some() && chan.can_send() && setup_until > tokio::time::Instant::now() {
                while matches!(
                    pending.front(),
                    Some(AppMessage::Hello { .. } | AppMessage::Approval { .. } | AppMessage::Paths(_))
                ) {
                    if let Some(m) = pending.pop_front() {
                        write_frame(&mut wr, &chan.seal(&m)?).await?;
                    }
                }
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
                () = lease_expired(lease, last_rx) => {
                    return Err(NetError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "lease expired (peer silent)",
                    )));
                }
                _ = heartbeat.tick(), if ticker.is_none() && paths_sent => {
                    // No cover traffic: an empty frame keeps the peer's lease.
                    if chan.can_send() && pending.is_empty() {
                        write_frame(&mut wr, &chan.seal(&AppMessage::Cover)?).await?;
                    }
                }
                frame = inbox.recv() => {
                    last_rx = tokio::time::Instant::now();
                    let frame = match frame {
                        Some(Ok(f)) => f,
                        Some(Err(NetError::Closed)) | None => return Ok(()),
                        Some(Err(e)) => return Err(e),
                    };
                    // Any authentication failure ends the session: on a
                    // reliable stream it can only mean tampering or a broken peer.
                    let opened = chan.open(&frame)?;
                    if peer_acks.is_none() {
                        let features = match opened {
                            AppMessage::Hello { features } => features,
                            _ => 0,
                        };
                        lock(&shared.peer_features).insert(peer, features);
                        peer_acks = Some(features & FEATURE_ACKS != 0);
                        node.send_profile(&peer);
                        if let Some(addr) = observed
                            && features & FEATURE_OBSERVED != 0
                        {
                            pending.push_back(AppMessage::Observed { addr });
                        }
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
                        AppMessage::Delete { conversation, ids } => node.on_delete(&peer, &conversation, &ids),
                        AppMessage::Edit { conversation, id, body } => node.on_edit(&peer, &conversation, id, &body),
                        AppMessage::Identity(payload) => node.on_identity(peer, &payload),
                        AppMessage::React { conversation, id, emoji, add } => node.on_react(&peer, &conversation, id, &emoji, add),
                        AppMessage::Read { conversation, ids } => node.on_read(&peer, &conversation, &ids),
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
                            if lock(&shared.tunnel).is_some() && shared.mutual(&peer) {
                                let tp = TunnelPeer {
                                    wg_public,
                                    endpoint: SocketAddr::new(addr.ip(), port),
                                    overlay: overlay_addr(&peer),
                                    psk: Secret(chan.export(PSK_CONTEXT)),
                                };
                                lock(&shared.tunnel_peers).insert(peer, tp.clone());
                                shared.emit(Event::TunnelUp {
                                    peer,
                                    wg_public: tp.wg_public,
                                    endpoint: tp.endpoint,
                                    overlay: tp.overlay,
                                    psk: tp.psk,
                                });
                            }
                        }
                        AppMessage::Paths(payload) => {
                            if shared.mutual(&peer) {
                                lease = node.on_paths(peer, &payload).or(lease);
                            } else {
                                early_paths = Some(payload);
                            }
                        }
                        AppMessage::Observed { addr } => {
                            if via.is_none() && !on_standby {
                                node.on_observed(peer, addr);
                            }
                        }
                        AppMessage::Prekeys(payload) => node.on_prekeys(peer, &payload),
                        AppMessage::Credential(payload) => node.on_credential(peer, &payload),
                        // Directory traffic belongs on anonymous links only.
                        AppMessage::Directory(_) => {}
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
                        AppMessage::Typing { active } => {
                            if via.is_none() && !on_standby {
                                shared.emit(Event::Typing { peer, active });
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
                            if matches!(msg, AppMessage::Group(_)) || node.is_accepted(&peer) {
                                shared.emit(Event::Message { peer, msg });
                            } else {
                                shared.emit(Event::MessageRequest { peer, msg });
                            }
                        }
                    }
                }
                out = outbox.recv() => match out {
                    Some(m) => pending.push_back(m),
                    None => return Ok(()), // replaced or disconnected locally
                },
                Ok(()) = rate.changed() => {
                    ticker = make_ticker(rate.borrow_and_update().filter(|_| !inner));
                }
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

/// How long after a session starts its setup messages skip the cover-traffic
/// schedule.
const SETUP_BURST: Duration = Duration::from_secs(5);

/// Sessions with one peer starting this close together are duplicates.
const DUPLICATE_WINDOW_MS: u64 = 5_000;

/// What a session runs over, as far as addresses go.
#[derive(Clone, Copy)]
enum Over {
    /// A main-endpoint QUIC connection: the peer's outside address is
    /// worth telling it.
    Quic(SocketAddr),
    /// Our standby endpoint: what the peer reports seeing is the standby
    /// network's address, not ours.
    Standby,
    Other,
}

/// Resolves when `lease` has passed since `last_rx`; never without a lease.
async fn lease_expired(lease: Option<Duration>, last_rx: tokio::time::Instant) {
    match lease {
        Some(l) => tokio::time::sleep_until(last_rx + l).await,
        None => std::future::pending().await,
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

/// No Nagle delay, and dead links noticed within about a minute: a phone
/// that leaves Wi-Fi sends no FIN, and without this its session would look
/// alive (and block reaching it another way) for many minutes.
fn tune_tcp(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let sock = socket2::SockRef::from(stream);
    let ka = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(30))
        .with_interval(Duration::from_secs(10));
    #[cfg(unix)]
    let ka = ka.with_retries(3);
    let _ = sock.set_tcp_keepalive(&ka);
    // Unacknowledged writes (cover traffic) fail after this long.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = sock.set_tcp_user_timeout(Some(Duration::from_secs(60)));
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
