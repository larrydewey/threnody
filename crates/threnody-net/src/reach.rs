//! Reaching contacts across the internet (Appendix N §2–4).
//!
//! - **Candidates:** our LAN and global IPv6 addresses, and our outside
//!   address as seen by session peers (`Observed`) and by DHT nodes we
//!   ping from the QUIC socket (BEP 42 `ip`), so no STUN server is needed.
//! - **Rendezvous:** for each mutually approved contact without a session,
//!   we store our candidates in the Mainline DHT under keys only the two of
//!   us can derive (`threnody_core::rendezvous`), and poll for theirs.
//! - **Hole punching:** both sides probe each other's candidates from the
//!   QUIC socket, opening both NATs; one side dials over QUIC.
//!
//! Nothing here runs while the feature is off, and personas never use it:
//! a direct path shows each side's address to the other.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mainline::async_dht::AsyncDht;
use mainline::{Dht, MutableItem, SigningKey};
use threnody_core::crypto::random_bytes;
use threnody_core::rendezvous::{
    self, Candidate, CandidateKind, Candidates, FLAG_SYMMETRIC, epoch, record_keys,
};
use threnody_core::{PublicIdentity, now_ms};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};
use crate::quic::canonical;

/// DHT nodes that report the address they see us at (libtorrent's do).
/// Some nodes from the routing table are pinged as well.
pub const DEFAULT_REFLECTORS: &[&str] = &[
    "dht.libtorrent.org:25401",
    "router.bittorrent.com:6881",
    "router.utorrent.com:6881",
];
/// How long punching we start lasts: the contact only joins in once it
/// has polled our record.
const SEEK_FOR: Duration = Duration::from_secs(120);
/// How long a contact we want to reach (a message waits for it, its chat
/// is open) stays marked as sought in our record. Longer than the
/// background poll, so a contact whose app is in the background still
/// sees it once, and answers.
const WANT_FOR: Duration = Duration::from_secs(20 * 60);
/// How long we punch when answering a contact (its record seeks us, or
/// its probe arrived). Our record then seeks it too, and the contact
/// polls every [`ReachConfig::poll_seeking`], so this covers a poll, a
/// read and a write with room to spare.
const ANSWER_FOR: Duration = Duration::from_secs(60);
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// After [`SEEK_FOR`], a contact we still want is probed this often: it
/// keeps our NAT open toward it, so its probes get through whenever it
/// polls our record and answers (from the background, that is late).
const SLOW_PROBE_EVERY: Duration = Duration::from_secs(5);
/// Hearing the contact or new addresses brings back full-rate punching
/// for this long.
const LIVELY_FOR: Duration = Duration::from_secs(60);
/// The dialer waits this long for a probe before dialing anyway.
const DIAL_AFTER: Duration = Duration::from_secs(3);
const REDIAL_EVERY: Duration = Duration::from_secs(5);
/// After a session was lost, the side that didn't move waits this long for
/// the other to dial before dialing itself.
const LOST_DIAL_AFTER: Duration = Duration::from_secs(15);
/// Contacts punched at once.
const MAX_PUNCHES: usize = 4;
/// Candidates probed per contact (so at most 32 probes in flight).
const MAX_TARGETS: usize = 8;
/// Waits before punching a contact again after failures.
const BACKOFF: [Duration; 3] = [
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(900),
];
/// Routing-table nodes pinged for our outside address, besides the reflectors.
const TABLE_REFLECTORS: usize = 6;
const REFLECT_WAIT: Duration = Duration::from_secs(2);
const MAX_REFLEXIVE: usize = 4;
/// Gathering again this soon while our outside address is unknown.
const UNKNOWN_REGATHER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub struct ReachConfig {
    /// DHT bootstrap nodes; `None` uses the public Mainline defaults.
    pub bootstrap: Option<Vec<String>>,
    /// DHT nodes pinged from the QUIC socket to learn our outside address.
    pub reflectors: Vec<String>,
    /// Poll interval for contacts' records while the app is open.
    pub poll_foreground: Duration,
    /// Poll interval in the background.
    pub poll_background: Duration,
    /// Poll interval while we are seeking a contact.
    pub poll_seeking: Duration,
    /// Records are republished at least this often (and when they change).
    pub republish: Duration,
    /// Candidates are gathered again this often (and on network changes).
    pub regather: Duration,
    /// Also offer loopback addresses and keep the DHT on loopback (tests).
    pub loopback: bool,
    /// An always-on, mutually approved contact we keep a session with
    /// (direct or through a relay circuit). It carries our address
    /// updates immediately, and its circuit's death means we moved.
    pub anchor: Option<PublicIdentity>,
}

impl Default for ReachConfig {
    fn default() -> Self {
        Self {
            bootstrap: None,
            reflectors: DEFAULT_REFLECTORS.iter().map(|s| (*s).to_owned()).collect(),
            poll_foreground: Duration::from_secs(20),
            poll_background: Duration::from_secs(900),
            poll_seeking: Duration::from_secs(15),
            republish: Duration::from_secs(1800),
            regather: Duration::from_secs(600),
            loopback: false,
            anchor: None,
        }
    }
}

/// What the UI can show about internet reachability.
#[derive(Clone, Debug, Default)]
pub struct Reachability {
    pub enabled: bool,
    /// Joined the DHT.
    pub online: bool,
    pub candidates: Vec<Candidate>,
    /// Our NAT gives a new outside port per destination.
    pub symmetric: bool,
    /// Contacts being hole-punched right now.
    pub punching: Vec<PublicIdentity>,
}

/// Why we punch toward a contact.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Urge {
    /// Its record is new to us: we start, and ask it to join in.
    Start,
    /// Its record asks for us: join in, even while backing off.
    Answer,
    /// Its probe arrived: it is punching toward us.
    Probed,
    /// Our network changed under a session with it: we know first, so we
    /// dial at once (see `recover`).
    Moved,
    /// Its session ended without warning: probe where it may be now, and
    /// dial only if it hasn't reached us in a few seconds (see `recover`).
    Lost,
}

struct Punch {
    targets: Mutex<Vec<SocketAddr>>,
    lasts: Duration,
    /// Full-rate punching for this long; slow probes after it (see
    /// [`SLOW_PROBE_EVERY`]), dialing only once the contact is heard.
    brisk: Duration,
    /// A probe from the contact arrived.
    heard: Notify,
    /// New addresses for the contact arrived: dial now.
    retarget: Notify,
    we_dial: bool,
    /// With `we_dial`: wait this long before the first dial.
    dial_after: Duration,
    /// The other side is expected to dial: a probe from it means it is
    /// dialing, so don't dial too.
    defer: bool,
}

struct Published {
    /// What was published: candidates, flags, seeking, epoch, and the
    /// shared key it was published under (a new session changes the key;
    /// the contact keeps only the last few).
    digest: (Vec<Candidate>, u64, Option<u64>, u64, [u8; 32]),
    at: Instant,
}

#[derive(Default)]
pub(crate) struct Inner {
    pub(crate) candidates: Vec<Candidate>,
    symmetric: bool,
    /// Our address as each session peer sees it.
    observed: HashMap<PublicIdentity, SocketAddr>,
    /// DHT pings in flight: transaction id → node.
    pings: HashMap<[u8; 2], SocketAddr>,
    /// Our address as each pinged DHT node sees it.
    reflected: HashMap<SocketAddr, SocketAddr>,
    published: HashMap<PublicIdentity, Published>,
    /// Contacts we want to reach, until (ms).
    seeking: HashMap<PublicIdentity, u64>,
    /// The newest record acted on, per contact.
    seen: HashMap<PublicIdentity, u64>,
    punches: HashMap<PublicIdentity, Arc<Punch>>,
    /// Failures so far and when punching may start again.
    backoff: HashMap<PublicIdentity, (usize, Instant)>,
    /// When we last started punching each contact (ms).
    last_punch: HashMap<PublicIdentity, u64>,
    /// Publishing rounds in a row where every write failed.
    put_failures: u32,
    /// DHT nodes to ping for our outside address, resolved and kept, so a
    /// network change needs no DNS before the pings go out.
    pub(crate) reflectors: Vec<SocketAddr>,
    /// The last DHT routing table, to rejoin from quickly.
    warm_nodes: Vec<String>,
    /// External addresses the router forwarded to us.
    pub(crate) mapped: Vec<SocketAddr>,
}

pub(crate) struct ReachState {
    enabled: AtomicBool,
    foreground: AtomicBool,
    started: AtomicBool,
    dht: Mutex<Option<AsyncDht>>,
    pub(crate) inner: Mutex<Inner>,
    pub(crate) regather: Notify,
    republish: Notify,
    poll: Notify,
    cfg: Mutex<Option<Arc<ReachConfig>>>,
}

impl ReachState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            foreground: AtomicBool::new(true),
            started: AtomicBool::new(false),
            dht: Mutex::new(None),
            inner: Mutex::default(),
            regather: Notify::new(),
            republish: Notify::new(),
            poll: Notify::new(),
            cfg: Mutex::new(None),
        }
    }
}

/// Whether a datagram is a KRPC (DHT) reply.
pub(crate) fn is_krpc_reply(data: &[u8]) -> bool {
    data.len() >= 12
        && data[0] == b'd'
        && data.last() == Some(&b'e')
        && find(data, b"1:y1:r").is_some()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

pub(crate) fn krpc_ping(tid: [u8; 2]) -> Vec<u8> {
    let mut m = b"d1:ad2:id20:".to_vec();
    m.extend_from_slice(&random_bytes::<20>());
    m.extend_from_slice(b"e1:q4:ping1:t2:");
    m.extend_from_slice(&tid);
    m.extend_from_slice(b"1:y1:qe");
    m
}

/// The transaction id and our address (BEP 42 `ip`) from a ping reply.
pub(crate) fn parse_krpc_reply(data: &[u8]) -> Option<([u8; 2], SocketAddr)> {
    let t = find(data, b"1:t2:")? + 5;
    let tid = [*data.get(t)?, *data.get(t + 1)?];
    let addr = if let Some(i) = find(data, b"2:ip6:") {
        let b = data.get(i + 6..i + 12)?;
        SocketAddr::new(
            IpAddr::from([b[0], b[1], b[2], b[3]]),
            u16::from_be_bytes([b[4], b[5]]),
        )
    } else {
        let i = find(data, b"2:ip18:")?;
        let b = data.get(i + 7..i + 25)?;
        let ip: [u8; 16] = b[..16].try_into().ok()?;
        SocketAddr::new(IpAddr::from(ip), u16::from_be_bytes([b[16], b[17]]))
    };
    Some((tid, canonical(addr)))
}

/// The address the OS would send from to reach `probe` (no packet is sent).
fn route_source(bind: &str, probe: &str) -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(probe).ok()?;
    Some(s.local_addr().ok()?.ip())
}

pub(crate) fn is_global_v6(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xe000 == 0x2000)
}

pub(crate) fn is_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64)
        }
        v6 => is_global_v6(v6),
    }
}

impl Node {
    pub(crate) fn rdv(&self) -> &ReachState {
        &self.shared.reach
    }

    /// Whether contacts may be reached across the internet: router-free
    /// rendezvous in the public DHT and hole punching (on by default; never
    /// for anonymous identities).
    pub fn reach_enabled(&self) -> bool {
        self.rdv().enabled.load(Ordering::Relaxed)
    }

    /// Turns internet reachability on or off. An anonymous identity stays
    /// off: a direct path shows its address to the peer.
    pub fn set_reach(&self, on: bool) {
        let on = on && !self.shared.persona;
        if self.rdv().enabled.swap(on, Ordering::Relaxed) != on {
            if !on {
                // Leave the DHT; nothing more is published or fetched.
                *lock(&self.rdv().dht) = None;
                lock(&self.rdv().inner).published.clear();
            }
            self.rdv().regather.notify_one();
            self.rdv().republish.notify_one();
            self.rdv().poll.notify_one();
        }
    }

    /// Tells rendezvous whether the app is in the foreground (contacts'
    /// records are polled more often then).
    pub fn set_foreground(&self, on: bool) {
        if !self.rdv().foreground.swap(on, Ordering::Relaxed) && on {
            self.rdv().poll.notify_one();
        }
    }

    /// We want to reach `peer` now (a message is waiting, or its chat is
    /// open): tell it through our record, and look for its record at once.
    pub fn seek(&self, peer: &PublicIdentity) {
        if !self.reach_enabled() || self.connected(peer) {
            return;
        }
        let until = now_ms() + WANT_FOR.as_millis() as u64;
        {
            let mut inner = lock(&self.rdv().inner);
            inner.seeking.insert(*peer, until);
            inner.backoff.remove(peer);
        }
        self.rdv().republish.notify_one();
        self.rdv().poll.notify_one();
    }

    /// The network changed: gather candidates again, publish and poll.
    pub fn network_changed(&self) {
        lock(&self.rdv().inner).backoff.clear();
        // DHT write tokens are bound to our address: rejoin from the new
        // one, starting from the nodes we knew, so it takes a moment.
        let old = lock(&self.rdv().dht).take();
        let node = self.clone();
        tokio::spawn(async move {
            if let Some(old) = old {
                let nodes = old.to_bootstrap().await;
                lock(&node.rdv().inner).warm_nodes = nodes;
            }
            node.leave_dht();
            node.rdv().regather.notify_one();
            node.rdv().poll.notify_one();
        });
    }

    pub fn reachability(&self) -> Reachability {
        let inner = lock(&self.rdv().inner);
        Reachability {
            enabled: self.reach_enabled(),
            online: lock(&self.rdv().dht).is_some(),
            candidates: inner.candidates.clone(),
            symmetric: inner.symmetric,
            punching: inner.punches.keys().copied().collect(),
        }
    }

    pub(crate) fn connected(&self, peer: &PublicIdentity) -> bool {
        self.sessions().iter().any(|s| s.peer == *peer)
    }

    /// Mutually approved contacts with discovery keys and no session.
    fn unreached(&self) -> Vec<(PublicIdentity, Vec<[u8; 32]>)> {
        let sessions = self.sessions();
        self.approved()
            .into_iter()
            .filter(|(p, _)| !sessions.iter().any(|s| s.peer == *p))
            .collect()
    }

    /// Mutually approved contacts with discovery keys. Our records stay
    /// current for all of them, connected or not: a session can die
    /// unnoticed, and a stale record sends the contact to an old address.
    fn approved(&self) -> Vec<(PublicIdentity, Vec<[u8; 32]>)> {
        self.contacts()
            .iter()
            .filter(|c| c.mutually_approved() && !c.blocked)
            .filter_map(|c| {
                let keys: Vec<_> = c.recognition_keys().copied().collect();
                (!keys.is_empty()).then_some((c.key, keys))
            })
            .collect()
    }

    fn leave_dht(&self) {
        *lock(&self.rdv().dht) = None;
        let mut inner = lock(&self.rdv().inner);
        inner.published.clear();
        inner.put_failures = 0;
    }

    pub(crate) fn reach_config(&self) -> Option<Arc<ReachConfig>> {
        lock(&self.rdv().cfg).clone()
    }

    pub(crate) fn dht(&self) -> Option<AsyncDht> {
        lock(&self.rdv().dht).clone()
    }

    /// Starts rendezvous and hole punching over the QUIC endpoint
    /// ([`Node::listen_quic`] first). While the feature is off, the tasks
    /// wait and the node stays out of the DHT.
    pub fn start_reach(&self, cfg: ReachConfig) -> Result<()> {
        if self.quic().is_none() {
            return Err(NetError::NotAllowed(
                "internet reachability needs the QUIC endpoint".into(),
            ));
        }
        if self.rdv().started.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let cfg = Arc::new(cfg);
        *lock(&self.rdv().cfg) = Some(cfg.clone());
        if let Some(port) = std::num::NonZeroU16::new(self.quic_addr().map_or(0, |a| a.port())) {
            let node = self.clone();
            tokio::spawn(async move { crate::portmap::run(node, port).await });
        }
        let node = self.clone();
        tokio::spawn(async move { node.keepalive_loop().await });
        let node = self.clone();
        tokio::spawn(async move { node.anchor_loop().await });
        let (node, c) = (self.clone(), cfg.clone());
        tokio::spawn(async move { node.gather_loop(&c).await });
        let (node, c) = (self.clone(), cfg.clone());
        tokio::spawn(async move { node.publish_loop(&c).await });
        let node = self.clone();
        tokio::spawn(async move { node.poll_loop(&cfg).await });
        Ok(())
    }

    /// Waits for `d`, a notification, or shutdown (`false`).
    pub(crate) async fn pause(&self, d: Duration, n: &Notify) -> bool {
        let closed = self.closed();
        tokio::select! {
            () = tokio::time::sleep(d) => true,
            () = n.notified() => true,
            () = closed => false,
        }
    }

    async fn gather_loop(&self, cfg: &ReachConfig) {
        loop {
            if self.reach_enabled() {
                if self.dht().is_none() {
                    let warm = lock(&self.rdv().inner).warm_nodes.clone();
                    match join_dht(cfg, &warm).await {
                        Ok(dht) => *lock(&self.rdv().dht) = Some(dht),
                        Err(e) => eprintln!("threnody: joining the DHT failed: {e}"),
                    }
                    if !self.reach_enabled() {
                        *lock(&self.rdv().dht) = None;
                    }
                }
                self.gather(cfg).await;
                self.rdv().poll.notify_one();
            }
            // Until we know our outside address (the DHT may not have
            // answered yet), try again soon.
            let known = lock(&self.rdv().inner)
                .candidates
                .iter()
                .any(|c| c.kind == CandidateKind::Reflexive);
            let wait = if known || !self.reach_enabled() {
                cfg.regather
            } else {
                cfg.regather.min(UNKNOWN_REGATHER)
            };
            if !self.pause(wait, &self.rdv().regather).await {
                break;
            }
        }
    }

    /// Keeps a session with the configured always-on anchor contact so
    /// our address updates reach it immediately, in the background too.
    async fn anchor_loop(&self) {
        loop {
            let anchor = self
                .reach_config()
                .and_then(|c| c.anchor)
                .filter(|_| self.reach_enabled());
            if let Some(anchor) = anchor
                && !self.sessions().iter().any(|s| s.peer == anchor)
            {
                let _ = self.reach_peer(&anchor).await;
            }
            if !self
                .pause(Duration::from_secs(5), &self.rdv().regather)
                .await
            {
                break;
            }
        }
    }

    /// DHT nodes to ping for our outside address: the cached list, else
    /// the configured reflectors and some of the routing table (resolved
    /// now and cached for next time).
    pub(crate) async fn reflectors(&self, cfg: &ReachConfig) -> Vec<SocketAddr> {
        let cached = lock(&self.rdv().inner).reflectors.clone();
        if cached.len() >= 2 {
            // Refresh in the background for next time.
            let (node, cfg) = (self.clone(), cfg.clone());
            tokio::spawn(async move {
                let fresh = node.resolve_reflectors(&cfg).await;
                if !fresh.is_empty() {
                    lock(&node.rdv().inner).reflectors = fresh;
                }
            });
            return cached;
        }
        let fresh = self.resolve_reflectors(cfg).await;
        lock(&self.rdv().inner).reflectors = fresh.clone();
        fresh
    }

    async fn resolve_reflectors(&self, cfg: &ReachConfig) -> Vec<SocketAddr> {
        let mut targets = Vec::new();
        for r in &cfg.reflectors {
            if let Ok(addrs) = tokio::net::lookup_host(r.as_str()).await {
                targets.extend(addrs.filter(SocketAddr::is_ipv4).take(1));
            }
        }
        if let Some(dht) = self.dht() {
            let table = dht.to_bootstrap().await;
            targets.extend(
                table
                    .iter()
                    .filter_map(|s| s.parse::<SocketAddr>().ok())
                    .take(TABLE_REFLECTORS),
            );
        }
        targets
    }

    /// Gathers our candidates; republishes when they changed.
    pub(crate) async fn gather(&self, cfg: &ReachConfig) {
        let Some(quic) = self.quic() else { return };
        let port = quic.local.port();
        let mut list = Vec::new();
        if cfg.loopback || quic.local.ip().is_loopback() {
            list.push(Candidate {
                kind: CandidateKind::Local,
                addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            });
        }
        // Addresses the router forwards to us: dialable without a punch.
        for a in lock(&self.rdv().inner).mapped.clone() {
            list.push(Candidate {
                kind: CandidateKind::Reflexive,
                addr: a,
            });
        }
        if let Some(ip) = route_source("0.0.0.0:0", "8.8.8.8:53")
            && !ip.is_loopback()
            && !ip.is_unspecified()
        {
            list.push(Candidate {
                kind: CandidateKind::Local,
                addr: SocketAddr::new(ip, port),
            });
        }
        if quic.can_reach(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST))
            && let Some(ip) = route_source("[::]:0", "[2001:4860:4860::8888]:53")
            && is_global_v6(&ip)
        {
            list.push(Candidate {
                kind: CandidateKind::Ipv6,
                addr: SocketAddr::new(ip, port),
            });
        }

        // Our outside address, from DHT nodes pinged off the QUIC socket.
        let targets = self.reflectors(cfg).await;
        {
            let mut inner = lock(&self.rdv().inner);
            inner.pings.clear();
            inner.reflected.clear();
            for t in &targets {
                let tid: [u8; 2] = random_bytes();
                if quic.send_raw(*t, &krpc_ping(tid)).is_ok() {
                    inner.pings.insert(tid, *t);
                }
            }
        }
        // Two answers are enough (one can't show a symmetric NAT).
        let deadline = Instant::now() + REFLECT_WAIT;
        while !targets.is_empty()
            && Instant::now() < deadline
            && lock(&self.rdv().inner).reflected.len() < 2
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let dht_guess = match self.dht() {
            Some(dht) => dht.info().await.public_address(),
            None => None,
        };

        let mut inner = lock(&self.rdv().inner);
        let answered: Vec<String> = inner.reflected.values().map(ToString::to_string).collect();
        let note = format!(
            "{} of {} DHT nodes reported our address: {}",
            answered.len(),
            inner.pings.len(),
            answered.join(", ")
        );
        inner.pings.clear();
        let contacts = self.contacts();
        // Only approved peers' observations count: anyone else could steer
        // our probes at an address of its choosing.
        let observed = inner
            .observed
            .iter()
            .filter(|(p, _)| contacts.get(p).is_some_and(|c| c.mutually_approved()))
            .map(|(_, a)| *a);
        let seen: Vec<SocketAddr> = inner.reflected.values().copied().chain(observed).collect();
        let mut reflexive: Vec<SocketAddr> = Vec::new();
        for a in seen.iter().filter(|a| is_public(&a.ip()) && a.is_ipv4()) {
            if !reflexive.contains(a) {
                reflexive.push(*a);
            }
        }
        // Two observers seeing one address at different ports: symmetric NAT.
        let symmetric = reflexive.iter().any(|a| {
            reflexive
                .iter()
                .any(|b| a.ip() == b.ip() && a.port() != b.port())
        });
        if reflexive.is_empty()
            && let Some(g) = dht_guess
            && is_public(&IpAddr::V4(*g.ip()))
        {
            // Many NATs keep the port: the DHT's view of our IP, our port.
            reflexive.push(SocketAddr::new(IpAddr::V4(*g.ip()), port));
        }
        list.extend(
            reflexive
                .into_iter()
                .take(MAX_REFLEXIVE)
                .map(|addr| Candidate {
                    kind: CandidateKind::Reflexive,
                    addr,
                }),
        );
        let mut unique = Vec::new();
        for c in list {
            if !unique.iter().any(|u: &Candidate| u.addr == c.addr) {
                unique.push(c);
            }
        }
        unique.sort_by_key(|c| c.kind.rank());
        unique.truncate(rendezvous::MAX_CANDIDATES);
        if unique != inner.candidates || symmetric != inner.symmetric {
            inner.candidates = unique;
            inner.symmetric = symmetric;
            let candidates = inner.candidates.iter().map(|c| c.addr).collect();
            drop(inner);
            self.emit(Event::ReachNote { note });
            self.emit(Event::Addresses {
                candidates,
                symmetric,
            });
            self.broadcast_paths();
            self.rdv().republish.notify_one();
        }
    }

    async fn publish_loop(&self, cfg: &ReachConfig) {
        loop {
            if self.reach_enabled()
                && let Some(dht) = self.dht()
            {
                self.publish_due(&dht, cfg).await;
            }
            if !self
                .pause(Duration::from_secs(60), &self.rdv().republish)
                .await
            {
                break;
            }
        }
    }

    /// Publishes our record for every contact whose record is missing,
    /// stale or out of date.
    async fn publish_due(&self, dht: &AsyncDht, cfg: &ReachConfig) {
        let now = now_ms();
        let ep = epoch(now / 1000);
        let me = self.identity();
        let mut puts = JoinSet::new();
        {
            let mut inner = lock(&self.rdv().inner);
            if inner.candidates.is_empty() {
                return;
            }
            inner.seeking.retain(|_, until| *until > now);
            let flags = if inner.symmetric { FLAG_SYMMETRIC } else { 0 };
            for (peer, keys) in self.approved() {
                let seeking = inner.seeking.get(&peer).copied();
                let digest = (inner.candidates.clone(), flags, seeking, ep, keys[0]);
                let fresh = inner
                    .published
                    .get(&peer)
                    .is_some_and(|p| p.digest == digest && p.at.elapsed() < cfg.republish);
                if fresh {
                    continue;
                }
                let record = Candidates {
                    list: inner.candidates.clone(),
                    issued_ms: now,
                    flags,
                    seeking_until_ms: seeking,
                };
                inner.published.insert(
                    peer,
                    Published {
                        digest,
                        at: Instant::now(),
                    },
                );
                // The current key: the contact recognises it whichever of
                // its recent keys it calls current.
                let d = keys[0];
                let dht = dht.clone();
                puts.spawn(async move {
                    let rk = record_keys(&d, &me, ep);
                    let value = rendezvous::seal_record(&rk, &record).ok()?;
                    let signer = SigningKey::from_bytes(&rk.signing_seed);
                    let seq = i64::try_from(record.issued_ms).unwrap_or(i64::MAX);
                    let item = MutableItem::new(signer, &value, seq, Some(&rk.salt));
                    let r = dht.put_mutable(item, None).await.map_err(|e| e.to_string());
                    Some((peer, r.map(|_| record.list.len())))
                });
            }
        }
        let (mut tried, mut failed) = (0, 0);
        while let Some(r) = puts.join_next().await {
            let Ok(Some((peer, r))) = r else { continue };
            tried += 1;
            let note = match r {
                Ok(n) => format!("published {n} address(es) for {}", peer.fingerprint()),
                Err(e) => {
                    failed += 1;
                    // Try again on the next round.
                    lock(&self.rdv().inner).published.remove(&peer);
                    format!("publishing for {} failed: {e}", peer.fingerprint())
                }
            };
            self.emit(Event::ReachNote { note });
        }
        if tried > 0 {
            let rejoin = {
                let mut inner = lock(&self.rdv().inner);
                inner.put_failures = if failed == tried {
                    inner.put_failures + 1
                } else {
                    0
                };
                inner.put_failures >= 2
            };
            if rejoin {
                // Nobody accepts our writes: our address probably changed
                // under the DHT client (stale write tokens). Rejoin.
                self.emit(Event::ReachNote {
                    note: "DHT writes keep failing; rejoining".into(),
                });
                self.leave_dht();
                self.rdv().regather.notify_one();
            }
        }
    }

    /// Contacts we seek and have no session with.
    fn sought(&self) -> Vec<PublicIdentity> {
        let now = now_ms();
        let sought: Vec<PublicIdentity> = lock(&self.rdv().inner)
            .seeking
            .iter()
            .filter(|(_, t)| **t > now)
            .map(|(p, _)| *p)
            .collect();
        sought.into_iter().filter(|p| !self.connected(p)).collect()
    }

    /// Polls every contact's record at the foreground or background rate,
    /// and the records of contacts we seek more often in between.
    async fn poll_loop(&self, cfg: &ReachConfig) {
        let mut last_full: Option<Instant> = None;
        let mut woken = true;
        loop {
            let every = if self.rdv().foreground.load(Ordering::Relaxed) {
                cfg.poll_foreground
            } else {
                cfg.poll_background
            };
            let full = woken || last_full.is_none_or(|t| t.elapsed() >= every);
            if self.reach_enabled()
                && let Some(dht) = self.dht()
            {
                let only = (!full).then(|| self.sought());
                self.poll_all(&dht, cfg, only.as_deref()).await;
                if full {
                    last_full = Some(Instant::now());
                }
            }
            let next_full = every.saturating_sub(last_full.map_or(Duration::ZERO, |t| t.elapsed()));
            let wait = if self.sought().is_empty() {
                next_full
            } else {
                cfg.poll_seeking.min(next_full)
            };
            let closed = self.closed();
            woken = tokio::select! {
                () = tokio::time::sleep(wait) => false,
                () = self.rdv().poll.notified() => true,
                () = closed => break,
            };
        }
    }

    /// Fetches the records of contacts we have no session with (only
    /// those in `only`, if given), and punches those that are fresh and
    /// new to us, or seeking us.
    async fn poll_all(&self, dht: &AsyncDht, cfg: &ReachConfig, only: Option<&[PublicIdentity]>) {
        let mut gets = JoinSet::new();
        for (peer, keys) in self.unreached() {
            if only.is_some_and(|o| !o.contains(&peer)) {
                continue;
            }
            let dht = dht.clone();
            gets.spawn(async move { (peer, fetch(&dht, &peer, &keys).await) });
        }
        let now = now_ms();
        // Records are republished every `republish`: older means gone.
        let max_age = cfg.republish.as_millis() as u64 * 3 / 2;
        while let Some(r) = gets.join_next().await {
            let Ok((peer, Some(rec))) = r else { continue };
            self.emit(Event::ReachNote {
                note: format!(
                    "record from {}: {} address(es), {} s old{}",
                    peer.fingerprint(),
                    rec.list.len(),
                    now.saturating_sub(rec.issued_ms) / 1000,
                    if rec.seeking_until_ms.is_some_and(|t| t > now) {
                        ", seeking us"
                    } else {
                        ""
                    }
                ),
            });
            if now.saturating_sub(rec.issued_ms) > max_age {
                continue;
            }
            let seeking = rec.seeking_until_ms.is_some_and(|t| t > now);
            let newer = lock(&self.rdv().inner)
                .seen
                .get(&peer)
                .is_none_or(|last| rec.issued_ms > *last);
            // A contact we want (a message waits) is punched toward its
            // last addresses even when they are not new: it answers late
            // from the background, and our probes must be going out then.
            let wanted = !newer && {
                let inner = lock(&self.rdv().inner);
                !inner.punches.contains_key(&peer)
                    && inner
                        .seeking
                        .get(&peer)
                        .is_some_and(|t| *t > now + SEEK_FOR.as_millis() as u64 / 2)
            };
            if !newer && !wanted {
                continue;
            }
            let urge = if seeking { Urge::Answer } else { Urge::Start };
            {
                // Written since our last attempt failed: the contact is
                // (back) online, so don't wait out the backoff, unless
                // attempts keep failing anyway.
                let mut inner = lock(&self.rdv().inner);
                let fresh = inner
                    .last_punch
                    .get(&peer)
                    .is_some_and(|t| rec.issued_ms > *t);
                if fresh && inner.backoff.get(&peer).is_some_and(|(n, _)| *n < 2) {
                    inner.backoff.remove(&peer);
                }
            }
            // A record we couldn't act on (backing off, too busy) stays new.
            if self.punch(peer, &rec.list, rec.flags & FLAG_SYMMETRIC != 0, urge) {
                let mut inner = lock(&self.rdv().inner);
                inner.seen.insert(peer, rec.issued_ms);
                // Tell the contact we answer: it may have stopped punching
                // (its app in the background polled us late), and our
                // record seeking it makes it join in at its next poll.
                if urge == Urge::Answer && inner.seeking.get(&peer).is_none_or(|t| *t <= now) {
                    inner
                        .seeking
                        .insert(peer, now + ANSWER_FOR.as_millis() as u64);
                    drop(inner);
                    self.rdv().republish.notify_one();
                }
            }
        }
    }

    /// Starts hole punching toward `peer` at `candidates`, unless it is
    /// connected, already being punched, backing off, or too many are
    /// (see [`Urge`] for the exceptions). True if it started.
    pub(crate) fn punch(
        &self,
        peer: PublicIdentity,
        candidates: &[Candidate],
        their_symmetric: bool,
        urge: Urge,
    ) -> bool {
        let Some(quic) = self.quic() else {
            return false;
        };
        if !self.reach_enabled() || (self.connected(&peer) && urge != Urge::Moved) {
            return false;
        }
        let mut sorted = candidates.to_vec();
        sorted.sort_by_key(|c| c.kind.rank());
        let mut targets: Vec<SocketAddr> = Vec::new();
        for a in sorted.iter().map(|c| c.addr) {
            if quic.can_reach(a.ip()) && !targets.contains(&a) && targets.len() < MAX_TARGETS {
                targets.push(a);
            }
        }
        let punch = {
            let mut inner = lock(&self.rdv().inner);
            let backing_off = inner
                .backoff
                .get(&peer)
                .is_some_and(|(_, until)| Instant::now() < *until);
            let recovering = matches!(urge, Urge::Moved | Urge::Lost);
            let running = inner.punches.contains_key(&peer);
            if targets.is_empty()
                || (backing_off && urge == Urge::Start && !running)
                || (inner.punches.len() >= MAX_PUNCHES && !recovering && !running)
            {
                return false;
            }
            if let Some(p) = inner.punches.get(&peer) {
                // Already punching (say, toward where it was before it went
                // away): aim it at the new places too, and dial now.
                let mut t = lock(&p.targets);
                for a in targets.iter().rev() {
                    if !t.contains(a) {
                        t.insert(0, *a);
                    }
                }
                t.truncate(MAX_TARGETS);
                p.retarget.notify_one();
                return true;
            }
            let lasts = match urge {
                Urge::Start => {
                    // Ask the contact to join in: our record says we seek it.
                    let now = now_ms();
                    let t = inner.seeking.entry(peer).or_default();
                    *t = (*t).max(now + SEEK_FOR.as_millis() as u64);
                    // Wanted longer (a message waits): keep the way open.
                    SEEK_FOR.max(Duration::from_millis(*t - now))
                }
                Urge::Lost => crate::recover::RECOVER_FOR,
                _ => ANSWER_FOR,
            };
            // Exactly one side dials: the one with a stable outside port if
            // only one has, else the one with the smaller key.
            let (we_dial, dial_after) = match urge {
                Urge::Moved => (true, Duration::ZERO),
                Urge::Lost => (true, LOST_DIAL_AFTER),
                _ => (
                    match (inner.symmetric, their_symmetric) {
                        (true, false) => false,
                        (false, true) => true,
                        _ => self.identity().as_bytes() < peer.as_bytes(),
                    },
                    DIAL_AFTER,
                ),
            };
            let p = Arc::new(Punch {
                targets: Mutex::new(targets),
                lasts,
                brisk: lasts.min(SEEK_FOR),
                heard: Notify::new(),
                retarget: Notify::new(),
                we_dial,
                dial_after,
                // On a loss neither side caused, the smaller key dials once
                // it hears the other; the larger leaves it to that side.
                defer: urge == Urge::Lost && self.identity().as_bytes() > peer.as_bytes(),
            });
            inner.punches.insert(peer, p.clone());
            inner.last_punch.insert(peer, now_ms());
            p
        };
        if urge == Urge::Start {
            self.rdv().republish.notify_one();
            self.rdv().poll.notify_one();
        }
        self.emit(Event::Punching {
            peer,
            targets: lock(&punch.targets).clone(),
            dialing: punch.we_dial,
        });
        let node = self.clone();
        tokio::spawn(async move {
            let ok = node.run_punch(peer, &punch).await;
            let mut inner = lock(&node.rdv().inner);
            inner.punches.remove(&peer);
            if ok {
                inner.backoff.remove(&peer);
                inner.seeking.remove(&peer);
            } else {
                node.emit(Event::PunchFailed { peer });
                let fails = inner.backoff.get(&peer).map_or(0, |(n, _)| *n);
                let wait = BACKOFF[fails.min(BACKOFF.len() - 1)];
                inner
                    .backoff
                    .insert(peer, (fails + 1, Instant::now() + wait));
            }
        });
        true
    }

    /// Probes for as long as the punch lasts and (as the dialer) dials; true once a
    /// session with `peer` exists.
    async fn run_punch(&self, peer: PublicIdentity, punch: &Punch) -> bool {
        let Some(quic) = self.quic() else {
            return false;
        };
        let me = self.identity();
        let start = Instant::now();
        let mut next_dial = start + punch.dial_after;
        let mut heard: Option<Instant> = None;
        // When we last heard the contact or got new addresses for it.
        let mut lively = start;
        let mut probed: Option<Instant> = None;
        let mut dialing: Option<tokio::task::JoinHandle<()>> = None;
        let mut tick = tokio::time::interval(PROBE_EVERY);
        let closed = self.closed();
        tokio::pin!(closed);
        let ok = loop {
            // However it came about (a punched path, Wi-Fi back), done.
            if self.connected(&peer) {
                break true;
            }
            if start.elapsed() >= punch.lasts || !self.reach_enabled() {
                break false;
            }
            tokio::select! {
                _ = tick.tick() => {}
                () = punch.retarget.notified() => {
                    // A dial to the old places may take seconds to fail.
                    if let Some(h) = dialing.take() {
                        h.abort();
                    }
                    next_dial = Instant::now();
                    heard = None;
                    lively = Instant::now();
                }
                () = punch.heard.notified() => {
                    lively = Instant::now();
                    if punch.defer {
                        heard = Some(Instant::now());
                    } else {
                        next_dial = Instant::now();
                    }
                }
                () = &mut closed => break false,
            }
            let Some(d) = self.contacts().get(&peer).and_then(|c| c.discovery_key) else {
                break false;
            };
            let brisk = start.elapsed() < punch.brisk || lively.elapsed() < LIVELY_FOR;
            if !brisk && probed.is_some_and(|t| t.elapsed() < SLOW_PROBE_EVERY) {
                continue;
            }
            probed = Some(Instant::now());
            let targets = lock(&punch.targets).clone();
            for t in &targets {
                let _ = quic.send_raw(*t, &rendezvous::probe(&d, &me));
            }
            let idle = dialing.as_ref().is_none_or(|h| h.is_finished());
            let quiet = heard.is_none_or(|h| h.elapsed() > Duration::from_secs(5));
            if punch.we_dial && brisk && idle && quiet && Instant::now() >= next_dial {
                next_dial = Instant::now() + REDIAL_EVERY;
                let node = self.clone();
                dialing = Some(tokio::spawn(async move {
                    let r = match node.race(&targets).await {
                        Ok(conn) => {
                            node.quic_session(conn, Some(peer.fingerprint()), false)
                                .await
                        }
                        Err(e) => Err(e),
                    };
                    if let Err(e) = r {
                        node.emit(Event::ReachNote {
                            note: format!("dialing {} failed: {e}", peer.fingerprint()),
                        });
                    }
                }));
            }
        };
        if !ok && let Some(h) = dialing {
            h.abort();
        }
        ok
    }

    /// Dials every target at once; the first QUIC connection wins.
    async fn race(&self, targets: &[SocketAddr]) -> Result<quinn::Connection> {
        let mut set = JoinSet::new();
        for t in targets {
            let (node, t) = (self.clone(), *t);
            set.spawn(async move { node.quic_dial(t).await });
        }
        let mut last = NetError::Closed;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(conn)) => {
                    set.abort_all();
                    return Ok(conn);
                }
                Ok(Err(e)) => last = e,
                Err(_) => {}
            }
        }
        Err(last)
    }

    /// A non-QUIC datagram on the QUIC socket: a DHT reply or a probe.
    pub(crate) fn on_foreign(&self, src: SocketAddr, data: &[u8], standby: bool) {
        if standby && is_krpc_reply(data) {
            self.on_standby_krpc(src, data);
            return;
        }
        if is_krpc_reply(data) {
            if let Some((tid, ours)) = parse_krpc_reply(data) {
                let mut inner = lock(&self.rdv().inner);
                if inner.pings.get(&tid) == Some(&src) {
                    inner.reflected.insert(src, ours);
                }
            }
            return;
        }
        if !self.reach_enabled() {
            return;
        }
        let contacts = self.contacts();
        let keys = contacts
            .iter()
            .filter(|c| c.mutually_approved())
            .flat_map(|c| c.recognition_keys().map(move |k| (&c.key, k)));
        let Some(peer) = rendezvous::recognise_probe(data, keys) else {
            return;
        };
        let punch = lock(&self.rdv().inner).punches.get(&peer).cloned();
        match punch {
            Some(p) => {
                // Where the probe came from is a path that works (with a
                // symmetric NAT, the only one): try it first.
                let new = {
                    let mut t = lock(&p.targets);
                    let new = !t.contains(&src);
                    if new {
                        t.insert(0, src);
                        t.truncate(MAX_TARGETS);
                    }
                    new
                };
                if new {
                    self.emit(Event::ReachNote {
                        note: format!("probe from {} at {src}", peer.fingerprint()),
                    });
                }
                p.heard.notify_one();
            }
            None => {
                self.punch(
                    peer,
                    &[Candidate {
                        kind: CandidateKind::Reflexive,
                        addr: src,
                    }],
                    false,
                    Urge::Probed,
                );
            }
        }
    }

    /// A session peer told us the address it sees us at.
    pub(crate) fn on_observed(&self, peer: PublicIdentity, addr: SocketAddr) {
        let addr = canonical(addr);
        let changed = lock(&self.rdv().inner).observed.insert(peer, addr) != Some(addr);
        if changed && self.shared.mutual(&peer) && self.reach_enabled() {
            self.rdv().regather.notify_one();
        }
    }
}

async fn join_dht(cfg: &ReachConfig, warm: &[String]) -> std::io::Result<AsyncDht> {
    let mut b = Dht::builder();
    if let Some(bs) = &cfg.bootstrap {
        b.bootstrap(bs);
    }
    if !warm.is_empty() {
        b.extra_bootstrap(warm);
    }
    if cfg.loopback {
        b.bind_address(Ipv4Addr::LOCALHOST);
    }
    let dht = tokio::task::spawn_blocking(move || b.build())
        .await
        .map_err(std::io::Error::other)??;
    Ok(dht.as_async())
}

/// The newest record `peer` published for us, under any of our shared
/// discovery keys, this epoch or the last.
async fn fetch(dht: &AsyncDht, peer: &PublicIdentity, keys: &[[u8; 32]]) -> Option<Candidates> {
    let ep = epoch(now_ms() / 1000);
    // Usually the record is under our current shared key and this hour:
    // one read. Only if that misses, try the older keys and the last hour.
    if let Some(d) = keys.first()
        && let Some(c) = fetch_one(dht, peer, d, ep).await
    {
        return Some(c);
    }
    let mut gets = JoinSet::new();
    for (i, d) in keys.iter().enumerate() {
        for e in [ep, ep.saturating_sub(1)] {
            if i == 0 && e == ep {
                continue;
            }
            let (dht, d, peer) = (dht.clone(), *d, *peer);
            gets.spawn(async move { fetch_one(&dht, &peer, &d, e).await });
        }
    }
    let mut best: Option<Candidates> = None;
    while let Some(r) = gets.join_next().await {
        if let Ok(Some(c)) = r
            && best.as_ref().is_none_or(|b| c.issued_ms > b.issued_ms)
        {
            best = Some(c);
        }
    }
    best
}

/// `peer`'s record under discovery key `d` in epoch `e`.
async fn fetch_one(
    dht: &AsyncDht,
    peer: &PublicIdentity,
    d: &[u8; 32],
    e: u64,
) -> Option<Candidates> {
    let rk = record_keys(d, peer, e);
    let key = SigningKey::from_bytes(&rk.signing_seed)
        .verifying_key()
        .to_bytes();
    let item = get_latest(dht, &key, &rk.salt).await?;
    rendezvous::open_record(&rk, item.value()).ok()
}

/// How long a DHT read collects answers. A full lookup can take many
/// seconds when some nodes don't answer; the closest nodes usually answer
/// within a second or two, and a later poll catches anything missed.
const READ_WAIT: Duration = Duration::from_secs(3);

/// The newest value stored under `key` and `salt` that arrives within
/// [`READ_WAIT`].
pub(crate) async fn get_latest(dht: &AsyncDht, key: &[u8; 32], salt: &[u8]) -> Option<MutableItem> {
    use futures_lite::StreamExt;
    let mut items = dht.get_mutable(key, Some(salt), None);
    let mut best: Option<MutableItem> = None;
    let deadline = tokio::time::sleep(READ_WAIT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            item = items.next() => match item {
                Some(i) => {
                    if best.as_ref().is_none_or(|b| i.seq() > b.seq()) {
                        best = Some(i);
                    }
                }
                None => break,
            },
            () = &mut deadline => break,
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn krpc_replies_are_told_apart_and_parsed() {
        let mut r = b"d2:ip6:".to_vec();
        r.extend_from_slice(&[203, 0, 113, 5, 0x1d, 0x1a]);
        r.extend_from_slice(b"1:rd2:id20:");
        r.extend_from_slice(&[7; 20]);
        r.extend_from_slice(b"e1:t2:ab1:y1:re");
        assert!(is_krpc_reply(&r));
        let (tid, addr) = parse_krpc_reply(&r).unwrap();
        assert_eq!(&tid, b"ab");
        assert_eq!(addr, "203.0.113.5:7450".parse().unwrap());
        assert!(!is_krpc_reply(&krpc_ping(*b"xy")), "a query is not a reply");
        assert!(!is_krpc_reply(&[0x40; 1200]));
    }

    #[test]
    fn public_addresses_exclude_private_and_shared_ranges() {
        for a in [
            "10.1.2.3",
            "192.168.1.1",
            "100.64.0.1",
            "127.0.0.1",
            "fe80::1",
        ] {
            assert!(!is_public(&a.parse().unwrap()), "{a}");
        }
        for a in ["203.0.113.1", "2001:db8::1"] {
            assert!(is_public(&a.parse().unwrap()), "{a}");
        }
    }
}
