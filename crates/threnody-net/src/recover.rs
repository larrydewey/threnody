//! Fast recovery of lost sessions (Appendix N, "Fast recovery").
//!
//! The slow path (`reach`) finds a contact by polling the DHT. This module
//! prepares, during a live session, everything needed to repair it the
//! moment it breaks:
//!
//! - **Paths.** Each side tells the other its addresses, its standby
//!   addresses and its heartbeat (`AppMessage::Paths`), and both derive a
//!   per-session *recovery slot* in the DHT from the session keys.
//! - **Lease.** Silence for three heartbeats ends the session at once, so
//!   both sides notice a loss within seconds of each other.
//! - **Standby path.** A phone on Wi-Fi keeps a second socket bound to its
//!   mobile network (the platform binds it), learns that socket's outside
//!   address, and keeps a hole open to each contact through it. When Wi-Fi
//!   goes, it dials straight through that hole.
//! - **Moving side first.** The platform reports a network change the
//!   moment it happens; the side that moved dials, publishes its new
//!   addresses to the recovery slots, and the other side, whose session
//!   just ended, watches those slots every 1.5 s and probes.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mainline::{MutableItem, SigningKey};
use threnody_core::crypto::random_bytes;
use threnody_core::message::FEATURE_PATHS;
use threnody_core::rendezvous::{
    self, Candidate, CandidateKind, Candidates, FLAG_SYMMETRIC, Paths, record_keys,
};
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};
use crate::quic::{Quic, canonical};
use crate::reach::{Urge, is_global_v6, is_public, krpc_ping, parse_krpc_reply};

/// How long the side that didn't move keeps trying after losing a session.
pub(crate) const RECOVER_FOR: Duration = Duration::from_secs(60);
/// How often it reads the other side's recovery slot meanwhile.
const SLOT_POLL: Duration = Duration::from_millis(1500);
/// The longest silence on a session when cover traffic is off: an empty
/// frame goes out at least this often, so the lease means something.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(10);
/// Missed heartbeats before a session counts as lost.
const LEASE_BEATS: u32 = 3;
/// First gap between keepalives from our standby socket (mobile radio
/// time, so as rare as the mobile network allows). It adapts: shorter when
/// a refresh finds the mapping expired, slowly longer while it holds.
/// AT&T kept idle mappings for 60 to 120 s in testing.
pub const STANDBY_KEEPALIVE: Duration = Duration::from_secs(45);
const STANDBY_KEEPALIVE_MIN: Duration = Duration::from_secs(20);
const STANDBY_KEEPALIVE_MAX: Duration = Duration::from_secs(300);
/// Gap between our probes to contacts' standby addresses, from our main
/// socket. Home routers forget UDP mappings within a minute (one did in
/// under 60 s in testing); refreshing them is cheap on Wi-Fi or wired.
const HOLE_KEEPALIVE: Duration = Duration::from_secs(20);
const STANDBY_WAIT: Duration = Duration::from_millis(1500);
/// The recovery slot record is per session, so it has no epochs.
const SLOT_EPOCH: u64 = 0;

#[derive(Clone, Default)]
struct PeerPaths {
    /// Key of this session's recovery slot (both sides derive it).
    slot: Option<[u8; 32]>,
    /// What the peer last told us.
    theirs: Paths,
    /// Its outside address on our current QUIC session, if any.
    remote: Option<SocketAddr>,
}

struct Standby {
    quic: Arc<Quic>,
    /// Global IPv6 addresses of the standby network.
    ipv6: Vec<IpAddr>,
    candidates: Vec<Candidate>,
    pings: HashMap<[u8; 2], SocketAddr>,
    reflected: HashMap<SocketAddr, SocketAddr>,
}

pub(crate) struct RecoverState {
    peers: Mutex<HashMap<PublicIdentity, PeerPaths>>,
    standby: Mutex<Option<Standby>>,
    /// A socket made for the platform to bind to the standby network.
    pending: Mutex<Option<std::net::UdpSocket>>,
    warm: AtomicBool,
    keepalive_ms: AtomicU64,
    /// The gap adapts (off once set by hand).
    adaptive: AtomicBool,
    /// The next keepalive round includes the standby socket's.
    force: AtomicBool,
    /// Wakes the keepalive loop (a hole to open now).
    kick: Notify,
    /// When the current recovery began, for the timings in the log.
    t0: Mutex<Option<Instant>>,
}

impl RecoverState {
    pub(crate) fn new() -> Self {
        Self {
            peers: Mutex::default(),
            standby: Mutex::default(),
            pending: Mutex::default(),
            warm: AtomicBool::new(false),
            keepalive_ms: AtomicU64::new(STANDBY_KEEPALIVE.as_millis() as u64),
            adaptive: AtomicBool::new(true),
            force: AtomicBool::new(false),
            kick: Notify::new(),
            t0: Mutex::default(),
        }
    }
}

/// The lease for a peer whose heartbeat is `beat_ms`.
pub(crate) fn lease_for(beat_ms: u64) -> Duration {
    Duration::from_millis(beat_ms.clamp(500, 120_000)) * LEASE_BEATS + Duration::from_secs(1)
}

fn slot_key(slot: &[u8; 32], writer: &PublicIdentity) -> rendezvous::RecordKeys {
    record_keys(slot, writer, SLOT_EPOCH)
}

impl Node {
    fn rec(&self) -> &RecoverState {
        &self.shared.recover
    }

    /// Logs a recovery step with the time since the recovery began.
    fn timed(&self, what: impl std::fmt::Display) {
        let ms = lock(&self.rec().t0).map(|t| t.elapsed().as_millis());
        let note = match ms {
            Some(ms) => format!("+{ms} ms: {what}"),
            None => what.to_string(),
        };
        self.emit(Event::ReachNote { note });
    }

    /// Our heartbeat: the cover-traffic interval, else [`HEARTBEAT`].
    pub(crate) fn beat(&self) -> Duration {
        self.constant_rate().unwrap_or(HEARTBEAT).min(HEARTBEAT)
    }

    fn our_paths(&self) -> Paths {
        let main = lock(&self.rdv().inner).candidates.clone();
        let standby = if self.rec().warm.load(Ordering::Relaxed) {
            lock(&self.rec().standby)
                .as_ref()
                .map(|s| s.candidates.clone())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Paths {
            main,
            standby,
            beat_ms: self.beat().as_millis() as u64,
        }
    }

    /// The `Paths` message for a peer, when rendezvous is on.
    pub(crate) fn paths_message(&self) -> Option<AppMessage> {
        if !self.reach_enabled() {
            return None;
        }
        self.our_paths().encode().ok().map(AppMessage::Paths)
    }

    /// Tells every approved peer on a direct session our current paths.
    pub(crate) fn broadcast_paths(&self) {
        let Some(msg) = self.paths_message() else {
            return;
        };
        for s in self.sessions() {
            if s.via.is_none()
                && self.supports(&s.peer, FEATURE_PATHS)
                && self.shared.mutual(&s.peer)
            {
                let _ = self.send(&s.peer, msg.clone());
            }
        }
    }

    /// A direct session with an approved peer is up: remember its
    /// recovery slot and, over QUIC, its outside address.
    pub(crate) fn session_up(
        &self,
        peer: PublicIdentity,
        slot: [u8; 32],
        remote: Option<SocketAddr>,
    ) {
        let mut peers = lock(&self.rec().peers);
        let p = peers.entry(peer).or_default();
        p.slot = Some(slot);
        if remote.is_some() {
            p.remote = remote.map(canonical);
        }
    }

    /// The peer's `Paths`; returns its lease.
    pub(crate) fn on_paths(&self, peer: PublicIdentity, payload: &[u8]) -> Option<Duration> {
        if !self.shared.mutual(&peer) {
            return None;
        }
        let paths = Paths::decode(payload).ok()?;
        let lease = lease_for(paths.beat_ms);
        let new_standby = {
            let mut peers = lock(&self.rec().peers);
            let p = peers.entry(peer).or_default();
            let changed = p.theirs.standby != paths.standby;
            p.theirs = paths;
            changed && !p.theirs.standby.is_empty()
        };
        if new_standby {
            let addrs: Vec<String> = lock(&self.rec().peers)
                .get(&peer)
                .map(|p| {
                    p.theirs
                        .standby
                        .iter()
                        .map(|c| c.addr.to_string())
                        .collect()
                })
                .unwrap_or_default();
            self.emit(Event::ReachNote {
                note: format!(
                    "{} keeps a standby path at {}",
                    peer.fingerprint(),
                    addrs.join(", ")
                ),
            });
            // Open our side of its standby hole now, not at the next keepalive.
            self.rec().kick.notify_one();
        }
        Some(lease)
    }

    /// A direct session with `peer` ended. Unless we are the side that
    /// moved (that side dials), probe where the peer may be now and watch
    /// its recovery slot.
    pub(crate) fn on_session_lost(&self, peer: PublicIdentity) {
        if !self.reach_enabled() || !self.shared.mutual(&peer) || self.is_closed() {
            return;
        }
        // A duplicate session dropped by the other side ends with an error
        // just as its twin takes over: give the twin a moment.
        let node = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            if !node.connected(&peer) {
                node.recover_lost(peer);
            }
        });
    }

    fn recover_lost(&self, peer: PublicIdentity) {
        let Some(paths) = lock(&self.rec().peers).get(&peer).cloned() else {
            return;
        };
        let lost_ms = now_ms();
        {
            let mut t0 = lock(&self.rec().t0);
            if t0.is_none_or(|t| t.elapsed() > RECOVER_FOR) {
                *t0 = Some(Instant::now());
            }
        }
        self.timed(format_args!("lost session with {}", peer.fingerprint()));
        let mut targets: Vec<Candidate> = paths.theirs.standby.clone();
        targets.extend(paths.remote.map(|addr| Candidate {
            kind: CandidateKind::Reflexive,
            addr,
        }));
        targets.extend(paths.theirs.main.iter().copied());
        if !targets.is_empty() {
            self.punch(peer, &targets, false, Urge::Lost);
        }
        if let Some(slot) = paths.slot {
            let node = self.clone();
            tokio::spawn(async move { node.watch_slot(peer, slot, lost_ms).await });
        }
    }

    /// Reads `peer`'s recovery slot every [`SLOT_POLL`] until it is back or
    /// [`RECOVER_FOR`] has passed; new addresses there go to the punch.
    async fn watch_slot(&self, peer: PublicIdentity, slot: [u8; 32], lost_ms: u64) {
        let start = Instant::now();
        let rk = slot_key(&slot, &peer);
        let key = SigningKey::from_bytes(&rk.signing_seed)
            .verifying_key()
            .to_bytes();
        let mut seen = 0;
        while start.elapsed() < RECOVER_FOR && !self.is_closed() {
            if self.connected(&peer) {
                self.timed(format_args!("{} is back", peer.fingerprint()));
                return;
            }
            if let Some(dht) = self.dht()
                && let Some(item) = dht.get_mutable_most_recent(&key, Some(&rk.salt)).await
                && let Ok(rec) = rendezvous::open_record(&rk, item.value())
                && rec.issued_ms + 10_000 >= lost_ms
                && rec.issued_ms > seen
            {
                seen = rec.issued_ms;
                self.timed(format_args!(
                    "{}'s recovery slot: {} address(es)",
                    peer.fingerprint(),
                    rec.list.len()
                ));
                self.punch(peer, &rec.list, rec.flags & FLAG_SYMMETRIC != 0, Urge::Lost);
            }
            tokio::time::sleep(SLOT_POLL).await;
        }
    }

    /// The platform's default network changed. With `to_standby`, it is
    /// now the network our standby socket is bound to (Wi-Fi went, mobile
    /// data took over): dial every approved peer through its open standby
    /// hole at once. In any case, learn our new addresses, write them to
    /// every recovery slot and punch toward the peers we lost.
    pub fn network_switched(&self, to_standby: bool) {
        *lock(&self.rec().t0) = Some(Instant::now());
        self.timed(if to_standby {
            "network changed to the standby network"
        } else {
            "network changed"
        });
        let peers: Vec<(PublicIdentity, PeerPaths)> = lock(&self.rec().peers)
            .iter()
            .filter(|(p, _)| self.shared.mutual(p))
            .map(|(p, pp)| (*p, pp.clone()))
            .collect();
        let standby = lock(&self.rec().standby).as_ref().map(|s| s.quic.clone());
        if to_standby && let Some(quic) = standby {
            for (peer, pp) in &peers {
                let mut targets: Vec<SocketAddr> = pp
                    .remote
                    .into_iter()
                    .filter(|a| is_public(&a.ip()))
                    .collect();
                for c in &pp.theirs.main {
                    if c.kind != CandidateKind::Local && !targets.contains(&c.addr) {
                        targets.push(c.addr);
                    }
                }
                if targets.is_empty() {
                    continue;
                }
                let (node, quic, peer) = (self.clone(), quic.clone(), *peer);
                tokio::spawn(async move { node.dial_through(&quic, peer, &targets).await });
            }
        }
        if !self.reach_enabled() {
            return;
        }
        self.network_changed();
        let node = self.clone();
        tokio::spawn(async move { node.announce_move(peers).await });
    }

    /// Dials `peer` at `targets` from the standby endpoint.
    async fn dial_through(&self, quic: &Arc<Quic>, peer: PublicIdentity, targets: &[SocketAddr]) {
        let mut set = JoinSet::new();
        for t in targets {
            let (node, quic, t) = (self.clone(), quic.clone(), *t);
            set.spawn(async move { node.quic_dial_via(&quic, t).await });
        }
        let mut last = NetError::Closed;
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(conn)) => {
                    set.abort_all();
                    match self
                        .quic_session(conn, Some(peer.fingerprint()), true)
                        .await
                    {
                        Ok(_) => self.timed(format_args!(
                            "reconnected to {} through the standby path",
                            peer.fingerprint()
                        )),
                        Err(e) => self.timed(format_args!("standby session failed: {e}")),
                    }
                    return;
                }
                Ok(Err(e)) => last = e,
                Err(_) => {}
            }
        }
        self.timed(format_args!(
            "standby dial to {} failed: {last}",
            peer.fingerprint()
        ));
    }

    /// After a network change: learn our new addresses, write them to each
    /// peer's recovery slot (asking it to come to us), and punch.
    async fn announce_move(&self, peers: Vec<(PublicIdentity, PeerPaths)>) {
        let Some(cfg) = self.reach_config() else {
            return;
        };
        self.gather(&cfg).await;
        let list = lock(&self.rdv().inner).candidates.clone();
        self.timed(format_args!("new addresses: {} known", list.len()));
        // Probe and dial at once; the slot write below runs meanwhile.
        for (peer, pp) in &peers {
            let mut targets: Vec<Candidate> = pp
                .remote
                .map(|addr| Candidate {
                    kind: CandidateKind::Reflexive,
                    addr,
                })
                .into_iter()
                .collect();
            targets.extend(pp.theirs.main.iter().copied());
            if !targets.is_empty() {
                self.punch(*peer, &targets, false, Urge::Moved);
            }
        }
        // Rejoining the DHT happens alongside; wait for it briefly.
        let mut dht = None;
        for _ in 0..200 {
            dht = self.dht();
            if dht.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let me = self.identity();
        let now = now_ms();
        if let Some(dht) = dht {
            let mut puts = JoinSet::new();
            for (peer, pp) in &peers {
                let Some(slot) = pp.slot else { continue };
                let record = Candidates {
                    list: list.clone(),
                    issued_ms: now,
                    flags: 0,
                    seeking_until_ms: Some(now + RECOVER_FOR.as_millis() as u64),
                };
                let (dht, peer) = (dht.clone(), *peer);
                puts.spawn(async move {
                    let rk = slot_key(&slot, &me);
                    let value = rendezvous::seal_record(&rk, &record).ok()?;
                    let signer = SigningKey::from_bytes(&rk.signing_seed);
                    let seq = i64::try_from(record.issued_ms).unwrap_or(i64::MAX);
                    let item = MutableItem::new(signer, &value, seq, Some(&rk.salt));
                    Some((peer, dht.put_mutable(item, None).await.is_ok()))
                });
            }
            while let Some(r) = puts.join_next().await {
                if let Ok(Some((peer, ok))) = r {
                    self.timed(format_args!(
                        "recovery slot for {} {}",
                        peer.fingerprint(),
                        if ok { "written" } else { "not written" }
                    ));
                }
            }
        }
    }

    /// Makes a UDP socket for the standby network and returns its file
    /// descriptor. The platform binds it to that network (on Android,
    /// `Network.bindSocket`) and then calls [`Node::standby_bound`].
    #[cfg(unix)]
    pub fn standby_socket(&self) -> Result<i32> {
        use std::os::fd::AsRawFd;
        let sock = crate::quic::bind("[::]:0".parse().map_err(|_| NetError::Closed)?)
            .or_else(|_| crate::quic::bind(SocketAddr::from(([0, 0, 0, 0], 0))))?;
        let fd = sock.as_raw_fd();
        *lock(&self.rec().pending) = Some(sock);
        Ok(fd)
    }

    /// The socket from [`Node::standby_socket`] is bound to the standby
    /// network, whose global IPv6 addresses are `ipv6`. Opens a QUIC
    /// endpoint on it and learns its outside address.
    pub fn standby_bound(&self, ipv6: Vec<IpAddr>) -> Result<()> {
        let sock = lock(&self.rec().pending)
            .take()
            .ok_or_else(|| NetError::NotAllowed("no standby socket was made".into()))?;
        let v6 = sock.local_addr()?.is_ipv6();
        let quic = self.open_endpoint(sock, v6, true)?;
        *lock(&self.rec().standby) = Some(Standby {
            quic,
            ipv6: ipv6.into_iter().filter(is_global_v6).collect(),
            candidates: Vec::new(),
            pings: HashMap::new(),
            reflected: HashMap::new(),
        });
        let node = self.clone();
        tokio::spawn(async move { node.learn_standby().await });
        Ok(())
    }

    /// The standby network went away.
    pub fn standby_lost(&self) {
        if lock(&self.rec().standby).take().is_some() {
            self.broadcast_paths();
        }
    }

    /// Whether to keep standby holes open (a keepalive per contact about
    /// every [`STANDBY_KEEPALIVE`]; it costs mobile radio time). Apps turn it on
    /// while a fast switch matters: the app is open, a chat was active
    /// recently, or the Wi-Fi signal is weakening.
    pub fn set_standby_warm(&self, on: bool) {
        if self.rec().warm.swap(on, Ordering::Relaxed) != on {
            self.broadcast_paths();
            if on {
                self.rec().force.store(true, Ordering::Relaxed);
                self.rec().kick.notify_one();
            }
        }
    }

    /// Fixes the gap between keepalives from the standby socket (it no
    /// longer adapts).
    pub fn set_standby_keepalive(&self, gap: Duration) {
        self.rec().adaptive.store(false, Ordering::Relaxed);
        self.rec()
            .keepalive_ms
            .store(gap.as_millis().max(1000) as u64, Ordering::Relaxed);
        self.rec().force.store(true, Ordering::Relaxed);
        self.rec().kick.notify_one();
    }

    /// The standby mapping held (`true`) or had expired since the last
    /// refresh: adapt the gap.
    fn adapt_keepalive(&self, held: bool) {
        if !self.rec().adaptive.load(Ordering::Relaxed) {
            return;
        }
        let cur = self.rec().keepalive_ms.load(Ordering::Relaxed);
        let next = if held { cur + cur / 10 } else { cur * 2 / 3 }.clamp(
            STANDBY_KEEPALIVE_MIN.as_millis() as u64,
            STANDBY_KEEPALIVE_MAX.as_millis() as u64,
        );
        self.rec().keepalive_ms.store(next, Ordering::Relaxed);
        if !held {
            self.emit(Event::ReachNote {
                note: format!(
                    "standby mapping had expired; keepalive now every {} s",
                    next / 1000
                ),
            });
        }
    }

    /// Pings DHT nodes from the standby socket to learn its outside address,
    /// and tells peers when it changed (after a long idle gap the mobile
    /// network may hand out a new port).
    async fn learn_standby(&self) {
        let Some(cfg) = self.reach_config() else {
            return;
        };
        let targets = self.reflectors(&cfg).await;
        let Some(quic) = lock(&self.rec().standby).as_ref().map(|s| s.quic.clone()) else {
            return;
        };
        {
            let mut st = lock(&self.rec().standby);
            let Some(s) = st.as_mut() else { return };
            s.pings.clear();
            s.reflected.clear();
            for t in &targets {
                let tid: [u8; 2] = random_bytes();
                if quic.send_raw(*t, &krpc_ping(tid)).is_ok() {
                    s.pings.insert(tid, *t);
                }
            }
        }
        let deadline = Instant::now() + STANDBY_WAIT;
        while Instant::now() < deadline
            && lock(&self.rec().standby)
                .as_ref()
                .is_some_and(|s| s.reflected.len() < 2)
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let list = {
            let mut st = lock(&self.rec().standby);
            let Some(s) = st.as_mut() else { return };
            let port = quic.local.port();
            let mut list: Vec<Candidate> = s
                .ipv6
                .iter()
                .map(|ip| Candidate {
                    kind: CandidateKind::Ipv6,
                    addr: SocketAddr::new(*ip, port),
                })
                .collect();
            for a in s.reflected.values() {
                if is_public(&a.ip()) && !list.iter().any(|c| c.addr == *a) {
                    list.push(Candidate {
                        kind: CandidateKind::Reflexive,
                        addr: *a,
                    });
                }
            }
            s.pings.clear();
            // Same IP, new port: the mapping expired between refreshes.
            let reflexive = |l: &[Candidate]| {
                l.iter()
                    .find(|c| c.kind == CandidateKind::Reflexive)
                    .map(|c| c.addr)
            };
            if let (Some(old), Some(new)) = (reflexive(&s.candidates), reflexive(&list))
                && old.ip() == new.ip()
            {
                let held = old.port() == new.port();
                drop(st);
                self.adapt_keepalive(held);
                st = lock(&self.rec().standby);
            }
            let Some(s) = st.as_mut() else { return };
            if s.candidates == list {
                return;
            }
            s.candidates = list.clone();
            list
        };
        let addrs: Vec<String> = list.iter().map(|c| c.addr.to_string()).collect();
        self.emit(Event::ReachNote {
            note: format!("standby path addresses: {}", addrs.join(", ")),
        });
        self.broadcast_paths();
        self.rec().kick.notify_one();
    }

    pub(crate) fn on_standby_krpc(&self, src: SocketAddr, data: &[u8]) {
        if let Some((tid, ours)) = parse_krpc_reply(data) {
            let mut st = lock(&self.rec().standby);
            if let Some(s) = st.as_mut()
                && s.pings.get(&tid) == Some(&src)
            {
                s.reflected.insert(src, ours);
            }
        }
    }

    /// Keeps standby holes open: our standby socket probes each approved
    /// peer's main addresses (while warm), and our main socket probes each
    /// peer's standby addresses (while it keeps them warm).
    pub(crate) async fn keepalive_loop(&self) {
        let mut last_standby: Option<Instant> = None;
        loop {
            if self.reach_enabled() {
                let gap = Duration::from_millis(self.rec().keepalive_ms.load(Ordering::Relaxed));
                let due = self.rec().force.swap(false, Ordering::Relaxed)
                    || last_standby.is_none_or(|t| t.elapsed() >= gap);
                self.keepalive_round(due);
                if due {
                    last_standby = Some(Instant::now());
                }
            }
            if !self.pause(HOLE_KEEPALIVE, &self.rec().kick).await {
                break;
            }
        }
    }

    /// With `standby_due`, our standby socket refreshes its holes (and
    /// checks its outside address); our main socket always refreshes ours
    /// toward contacts' standby addresses.
    fn keepalive_round(&self, standby_due: bool) {
        let standby_due = standby_due
            && self.rec().warm.load(Ordering::Relaxed)
            && lock(&self.rec().standby).is_some();
        if standby_due {
            let node = self.clone();
            tokio::spawn(async move { node.learn_standby().await });
        }
        let me = self.identity();
        let contacts = self.contacts();
        let standby = lock(&self.rec().standby).as_ref().map(|s| s.quic.clone());
        let main = self.quic();
        let peers: Vec<(PublicIdentity, PeerPaths)> = lock(&self.rec().peers)
            .iter()
            .map(|(p, pp)| (*p, pp.clone()))
            .collect();
        for (peer, pp) in peers {
            let Some(d) = contacts
                .get(&peer)
                .filter(|c| c.mutually_approved())
                .and_then(|c| c.discovery_key)
            else {
                continue;
            };
            if standby_due && let Some(s) = &standby {
                let targets = pp
                    .remote
                    .into_iter()
                    .chain(pp.theirs.main.iter().map(|c| c.addr))
                    .filter(|a| is_public(&a.ip()));
                for t in targets.take(8) {
                    let _ = s.send_raw(t, &rendezvous::probe(&d, &me));
                }
            }
            if let Some(m) = &main {
                for c in pp.theirs.standby.iter().take(8) {
                    let _ = m.send_raw(c.addr, &rendezvous::probe(&d, &me));
                }
            }
        }
    }
}
