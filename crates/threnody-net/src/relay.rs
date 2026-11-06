//! Multi-hop relay circuits between mutually approved nodes (spec §7.3, §7.4).
//!
//! A circuit is a chain of hops, each a pair `(peer, circuit id)` local to
//! one link. Every relay keeps only "frames arriving on (P, c) go to
//! (Q, d)". The two endpoints run an ordinary Threnody handshake and
//! session over the circuit, so relays see only end-to-end ciphertext, and
//! each hop additionally wraps it in that link's own ratchet.
//!
//! ```text
//! RelayMsg = { 0: op, 1: circ u64, ? 2: dest fingerprint (20), ? 3: ttl, ? 4: nonce (16), ? 5: frame }
//! op: 1 open, 2 opened, 3 refused, 4 data, 5 close
//! ```
//!
//! Policy: a node relays only between its own mutually approved contacts
//! and accepts circuits only from mutually approved neighbours. Route
//! discovery is a depth-first search bounded by `ttl` (at most
//! [`MAX_TTL`] relays); open nonces are remembered so loops are refused.
//! See `docs/appendix-g-relay.md`.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::crypto::random_bytes;
use threnody_core::{AppMessage, Fingerprint, PublicIdentity};
use tokio::sync::{mpsc, oneshot};

use crate::error::{NetError, Result};
use crate::frame::{read_frame, write_frame};
use crate::handshake;
use crate::node::{Event, FallbackPath, Node, lock};

/// Most relays a circuit may pass through.
pub const MAX_TTL: u8 = 3;
/// Most circuits one neighbour may hold through us.
pub const MAX_CIRCUITS_PER_PEER: usize = 64;
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const NONCE_MEMORY: Duration = Duration::from_secs(600);
const NONCE_PRUNE_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelayMsg {
    Open {
        circ: u64,
        dest: [u8; 20],
        ttl: u8,
        nonce: [u8; 16],
    },
    Opened {
        circ: u64,
    },
    Refused {
        circ: u64,
    },
    Data {
        circ: u64,
        frame: Vec<u8>,
    },
    Close {
        circ: u64,
    },
}

impl RelayMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(
            64 + if let Self::Data { frame, .. } = self {
                frame.len()
            } else {
                0
            },
            |e| {
                match self {
                    Self::Open {
                        circ,
                        dest,
                        ttl,
                        nonce,
                    } => {
                        e.map_len(5)?;
                        e.u8(0)?.u8(1)?;
                        e.u8(1)?.u64(*circ)?;
                        e.u8(2)?.bytes(dest)?;
                        e.u8(3)?.u8(*ttl)?;
                        e.u8(4)?.bytes(nonce)?;
                    }
                    Self::Opened { circ } | Self::Refused { circ } | Self::Close { circ } => {
                        let op = match self {
                            Self::Opened { .. } => 2,
                            Self::Refused { .. } => 3,
                            _ => 5,
                        };
                        e.map_len(2)?;
                        e.u8(0)?.u8(op)?;
                        e.u8(1)?.u64(*circ)?;
                    }
                    Self::Data { circ, frame } => {
                        e.map_len(3)?;
                        e.u8(0)?.u8(4)?;
                        e.u8(1)?.u64(*circ)?;
                        e.u8(5)?.bytes(frame)?;
                    }
                }
                Ok(())
            },
        )
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut circ, mut dest, mut ttl, mut nonce, mut frame) =
            (None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => circ = Some(d.u64()?),
                2 => dest = Some(fixed_bytes::<20>(d)?),
                3 => ttl = Some(d.u8()?),
                4 => nonce = Some(fixed_bytes::<16>(d)?),
                5 => frame = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let circ = required(circ, "circuit id")?;
        Ok(match required(op, "relay op")? {
            1 => Self::Open {
                circ,
                dest: required(dest, "destination")?,
                ttl: required(ttl, "ttl")?,
                nonce: required(nonce, "nonce")?,
            },
            2 => Self::Opened { circ },
            3 => Self::Refused { circ },
            4 => Self::Data {
                circ,
                frame: required(frame, "frame")?,
            },
            5 => Self::Close { circ },
            other => return Err(threnody_core::Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// Where frames arriving on one hop go.
enum Hop {
    /// Relay: to this peer, on this circuit.
    Forward(PublicIdentity, u64),
    /// We are an endpoint: to the local session's pump.
    Endpoint(mpsc::UnboundedSender<Vec<u8>>),
}

#[derive(Default)]
pub(crate) struct RelayState {
    hops: HashMap<(PublicIdentity, u64), Hop>,
    pending: HashMap<(PublicIdentity, u64), oneshot::Sender<bool>>,
    seen: HashMap<[u8; 16], Instant>,
    /// When `seen` was last pruned.
    pruned: Option<Instant>,
    /// Per neighbour: OPENs still looking for a path. They count against
    /// its circuits, or it could start any number of searches at once.
    opening: HashMap<PublicIdentity, usize>,
}

impl RelayState {
    fn circuits_with(&self, peer: &PublicIdentity) -> usize {
        self.hops.keys().filter(|(p, _)| p == peer).count()
    }

    /// Records an open nonce; false if it was already seen (a loop).
    fn first_sighting(&mut self, nonce: [u8; 16]) -> bool {
        let now = Instant::now();
        // Forgotten nonces are swept now and then, not on every OPEN.
        if self
            .pruned
            .is_none_or(|t| now.duration_since(t) > NONCE_PRUNE_EVERY)
        {
            self.seen
                .retain(|_, t| now.duration_since(*t) < NONCE_MEMORY);
            self.pruned = Some(now);
        }
        match self.seen.insert(nonce, now) {
            None => true,
            Some(t) => now.duration_since(t) >= NONCE_MEMORY,
        }
    }

    /// Takes an OPEN from `from` on its circuit `circ`, if there is room;
    /// [`Self::open_done`] once it has a hop or has failed.
    fn admit_open(&mut self, from: PublicIdentity, circ: u64, nonce: [u8; 16]) -> bool {
        let searching = self.opening.get(&from).copied().unwrap_or(0);
        let ok = self.circuits_with(&from) + searching < MAX_CIRCUITS_PER_PEER
            && !self.hops.contains_key(&(from, circ))
            && self.first_sighting(nonce);
        if ok {
            *self.opening.entry(from).or_default() += 1;
        }
        ok
    }

    fn open_done(&mut self, from: &PublicIdentity) {
        if let Some(n) = self.opening.get_mut(from) {
            *n -= 1;
            if *n == 0 {
                self.opening.remove(from);
            }
        }
    }
}

/// How long an onion attempt may take before falling back to a direct
/// dial.
pub(crate) const ONION_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Circuits through volunteers cross anonymous links with cover traffic,
/// which spaces every message: they take longer to build.
const VOLUNTEER_DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// A background reconnection with no first hop for its circuit dials
/// directly, to bring one up for the others; the others leave it this
/// long before one of them tries instead.
const FIRST_HOP_CLAIM: Duration = Duration::from_secs(5);
/// The longest a background reconnection waits for a first hop before
/// dialing directly anyway.
const FIRST_HOP_WAIT: Duration = Duration::from_secs(30);
/// How often a waiting one looks again.
const FIRST_HOP_POLL: Duration = Duration::from_millis(200);

impl Node {
    fn relay_send(&self, to: &PublicIdentity, msg: &RelayMsg) -> bool {
        msg.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Relay(b)).is_ok())
    }

    /// Handles a relay message that arrived in the session with `from`.
    pub(crate) fn on_relay(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = RelayMsg::decode(payload) else {
            return;
        };
        match msg {
            RelayMsg::Open {
                circ,
                dest,
                ttl,
                nonce,
            } => {
                let admitted = self.shared.mutual(&from)
                    && ttl <= MAX_TTL
                    && lock(&self.shared.relay).admit_open(from, circ, nonce);
                if !admitted {
                    self.relay_send(&from, &RelayMsg::Refused { circ });
                    return;
                }
                let node = self.clone();
                tokio::spawn(async move {
                    node.handle_open(from, circ, dest, ttl, nonce).await;
                    lock(&node.shared.relay).open_done(&from);
                });
            }
            RelayMsg::Opened { circ } | RelayMsg::Refused { circ } => {
                let ok = matches!(msg, RelayMsg::Opened { .. });
                if let Some(tx) = lock(&self.shared.relay).pending.remove(&(from, circ)) {
                    let _ = tx.send(ok);
                }
            }
            RelayMsg::Data { circ, frame } => {
                let target = match lock(&self.shared.relay).hops.get(&(from, circ)) {
                    Some(Hop::Forward(to, c2)) => Some((*to, *c2)),
                    Some(Hop::Endpoint(tx)) => {
                        let _ = tx.send(frame);
                        return;
                    }
                    None => None,
                };
                match target {
                    Some((to, c2)) => {
                        if !self.relay_send(&to, &RelayMsg::Data { circ: c2, frame }) {
                            self.close_circuit(from, circ);
                        }
                    }
                    None => {
                        self.relay_send(&from, &RelayMsg::Close { circ });
                    }
                }
            }
            RelayMsg::Close { circ } => self.close_circuit(from, circ),
        }
    }

    /// Tears down the circuit hop `(peer, circ)` and its partner hop.
    pub(crate) fn close_circuit(&self, peer: PublicIdentity, circ: u64) {
        let partner = {
            let mut st = lock(&self.shared.relay);
            match st.hops.remove(&(peer, circ)) {
                Some(Hop::Forward(to, c2)) => {
                    st.hops.remove(&(to, c2));
                    Some((to, c2))
                }
                // Dropping the endpoint sender ends the pump and its session.
                _ => None,
            }
        };
        if let Some((to, c2)) = partner {
            self.relay_send(&to, &RelayMsg::Close { circ: c2 });
        }
    }

    /// Closes every circuit that runs over the link to `peer` (it went away).
    pub(crate) fn drop_circuits_of(&self, peer: &PublicIdentity) {
        let circs: Vec<u64> = lock(&self.shared.relay)
            .hops
            .keys()
            .filter(|(p, _)| p == peer)
            .map(|(_, c)| *c)
            .collect();
        for c in circs {
            self.close_circuit(*peer, c);
        }
        lock(&self.shared.relay)
            .pending
            .retain(|(p, _), _| p != peer);
    }

    async fn handle_open(
        &self,
        from: PublicIdentity,
        circ: u64,
        dest: [u8; 20],
        ttl: u8,
        nonce: [u8; 16],
    ) {
        if dest == self.identity().fingerprint().0 {
            self.accept_circuit(from, circ);
            return;
        }
        let dest = Fingerprint(dest);
        match self.extend(dest, ttl, nonce, &[from]).await {
            Some((next, c2)) => {
                {
                    let mut st = lock(&self.shared.relay);
                    st.hops.insert((from, circ), Hop::Forward(next, c2));
                    st.hops.insert((next, c2), Hop::Forward(from, circ));
                }
                if !self.relay_send(&from, &RelayMsg::Opened { circ }) {
                    self.close_circuit(from, circ);
                }
            }
            None => {
                self.relay_send(&from, &RelayMsg::Refused { circ });
            }
        }
    }

    /// Finds the next hop towards `dest`: the destination itself if it is
    /// a live, mutually approved neighbour; otherwise each approved
    /// neighbour in turn while `ttl` allows another relay.
    async fn extend(
        &self,
        dest: Fingerprint,
        ttl: u8,
        nonce: [u8; 16],
        exclude: &[PublicIdentity],
    ) -> Option<(PublicIdentity, u64)> {
        let live: Vec<PublicIdentity> = self
            .sessions()
            .iter()
            .filter(|s| s.via.is_none())
            .map(|s| s.peer)
            .collect();
        let candidates: Vec<(PublicIdentity, u8)> = if let Some(d) = live
            .iter()
            .find(|p| p.fingerprint() == dest && self.shared.mutual(p))
        {
            vec![(*d, 0)]
        } else if ttl > 0 {
            live.into_iter()
                .filter(|p| {
                    !exclude.contains(p) && p.fingerprint() != dest && self.shared.mutual(p)
                })
                .map(|p| (p, ttl - 1))
                .collect()
        } else {
            vec![]
        };
        for (hop, hop_ttl) in candidates {
            let circ = u64::from_le_bytes(random_bytes());
            let (tx, rx) = oneshot::channel();
            lock(&self.shared.relay).pending.insert((hop, circ), tx);
            let open = RelayMsg::Open {
                circ,
                dest: dest.0,
                ttl: hop_ttl,
                nonce,
            };
            if !self.relay_send(&hop, &open) {
                lock(&self.shared.relay).pending.remove(&(hop, circ));
                continue;
            }
            match tokio::time::timeout(OPEN_TIMEOUT * u32::from(hop_ttl + 1), rx).await {
                Ok(Ok(true)) => return Some((hop, circ)),
                _ => {
                    lock(&self.shared.relay).pending.remove(&(hop, circ));
                }
            }
        }
        None
    }

    /// Endpoint side: binds the circuit to a fresh session (we respond).
    fn accept_circuit(&self, from: PublicIdentity, circ: u64) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        self.bind_endpoint(from, circ, ours);
        if !self.relay_send(&from, &RelayMsg::Opened { circ }) {
            self.close_circuit(from, circ);
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let mut stream = theirs;
            let result = async {
                let chan = handshake::accept(&mut stream, node.identity_ref()).await?;
                node.check_policy(chan.peer())?;
                Ok::<_, NetError>(chan)
            }
            .await;
            match result {
                Ok(chan) => node.spawn_relayed_session(stream, chan, from, false),
                Err(e) => {
                    node.close_circuit(from, circ);
                    node.emit_rejected_relay(from, e.to_string());
                }
            }
        });
    }

    /// Wires one end of an in-memory stream to circuit hop `(peer, circ)`.
    fn bind_endpoint(&self, peer: PublicIdentity, circ: u64, stream: tokio::io::DuplexStream) {
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        lock(&self.shared.relay)
            .hops
            .insert((peer, circ), Hop::Endpoint(tx));
        let node = self.clone();
        tokio::spawn(async move {
            pump_frames(stream, rx, |frame| {
                node.relay_send(&peer, &RelayMsg::Data { circ, frame })
            })
            .await;
            node.close_circuit(peer, circ);
        });
    }

    /// Reaches `dest` through relays and starts an authenticated session
    /// with it. The destination's fingerprint is pinned.
    pub async fn connect_relayed(&self, dest: Fingerprint) -> Result<PublicIdentity> {
        if dest == self.identity().fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        let nonce: [u8; 16] = random_bytes();
        lock(&self.shared.relay).first_sighting(nonce);
        // As originator we never hand the destination a direct open: a
        // direct session would not need a relay.
        let (via, circ) = self
            .extend_from_origin(dest, nonce)
            .await
            .ok_or_else(|| NetError::NoRoute(dest.to_string()))?;
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        self.bind_endpoint(via, circ, ours);
        let mut stream = theirs;
        let chan = match handshake::initiate(&mut stream, self.identity_ref(), Some(dest)).await {
            Ok(c) => c,
            Err(e) => {
                self.close_circuit(via, circ);
                return Err(e);
            }
        };
        let peer = *chan.peer();
        self.spawn_relayed_session(stream, chan, via, true);
        Ok(peer)
    }

    /// Connects directly to `addr` if one is given, falling back to relays
    /// when that fails or there is no address (spec §7.3). Relaying needs
    /// the destination's fingerprint, `pin`, which a direct dial also
    /// enforces.
    pub async fn reach(
        &self,
        addr: Option<&str>,
        pin: Option<Fingerprint>,
    ) -> Result<PublicIdentity> {
        self.reach_with(addr, pin, true).await
    }

    /// [`Node::reach`]; without `onion`, no onion circuit is tried (the
    /// caller tried one already).
    async fn reach_with(
        &self,
        addr: Option<&str>,
        pin: Option<Fingerprint>,
        onion: bool,
    ) -> Result<PublicIdentity> {
        if let Some(fp) = pin
            && let Some(p) = self.reach_hidden(addr, fp, onion).await
        {
            return Ok(p);
        }
        let direct = match addr {
            Some(a) => self.connect(a, pin).await,
            None => Err(NetError::NoRoute("no address".into())),
        };
        // Only a contact we could have hidden from (we know who it is)
        // falls back.
        let fell_back = |p: PublicIdentity, path| {
            if pin.is_some() {
                self.fell_back(p, path);
            }
            p
        };
        match (direct, pin) {
            (Ok(p), _) => Ok(fell_back(p, FallbackPath::Direct)),
            (Err(NetError::IdentityMismatch { expected, got }), _) => {
                Err(NetError::IdentityMismatch { expected, got })
            }
            (Err(_), Some(fp)) => self
                .connect_relayed(fp)
                .await
                .map(|p| fell_back(p, FallbackPath::Relayed)),
            (Err(e), None) => Err(e),
        }
    }

    /// Tells the app that `peer` was reached in the open after hidden
    /// paths failed, if we wanted them (with onion routing off, going
    /// direct is the user's choice).
    pub(crate) fn fell_back(&self, peer: PublicIdentity, path: FallbackPath) {
        if self.prefer_onion() {
            self.emit(Event::Fallback { peer, path });
        }
    }

    /// Reaches `fp` without showing either side's address to the other,
    /// when we can: an onion circuit through approved contacts, else
    /// through volunteer relays toward `addr`.
    pub(crate) async fn reach_hidden(
        &self,
        addr: Option<&str>,
        fp: Fingerprint,
        onion: bool,
    ) -> Option<PublicIdentity> {
        if !self.prefer_onion() {
            return None;
        }
        // Metadata first (spec §9 layer 2): with two approved relays, an
        // onion circuit keeps any single relay, and anyone watching our
        // network, from seeing who we talk to.
        if onion
            && self.onion_possible(fp)
            && let Ok(Ok(p)) =
                tokio::time::timeout(ONION_DIAL_TIMEOUT, self.connect_onion(fp, 2)).await
        {
            return Some(p);
        }
        // Without enough approved contacts, through volunteer relays from
        // our directories (Appendix P): the destination then sees a relay's
        // address, not ours.
        if let Some(a) = addr
            && !self.volunteer_routes(fp, a, &[]).is_empty()
            && let Ok(Ok(p)) =
                tokio::time::timeout(VOLUNTEER_DIAL_TIMEOUT, self.connect_volunteer(fp, a)).await
        {
            return Some(p);
        }
        None
    }

    /// Whether a two-relay onion path to `dest` could exist: a live,
    /// mutually approved neighbour plus another approved contact.
    pub(crate) fn onion_possible(&self, dest: Fingerprint) -> bool {
        self.first_hops(dest, 0) && self.onion_relays(dest)
    }

    /// Whether we have a live direct session with a mutually approved
    /// neighbour other than `dest`, begun at or after `since_ms` or over
    /// a radio link (which a change of network leaves alone).
    fn first_hops(&self, dest: Fingerprint, since_ms: u64) -> bool {
        self.sessions().into_iter().any(|s| {
            s.via.is_none()
                && s.peer.fingerprint() != dest
                && (s.since_ms >= since_ms || s.addr.ip().is_unspecified())
                && self.shared.mutual(&s.peer)
        })
    }

    /// Whether we have the two mutually approved contacts besides `dest`
    /// that a two-relay circuit to it needs.
    fn onion_relays(&self, dest: Fingerprint) -> bool {
        let me = self.identity();
        self.contacts()
            .iter()
            .filter(|c| c.mutually_approved() && c.fingerprint() != dest && c.key != me)
            .take(2)
            .count()
            >= 2
    }

    /// Onion first for background reconnections (Appendix I, *Background
    /// reconnection*): reaches mutually approved `peer` through an onion
    /// circuit when onion routing is on and one can be built. True once a
    /// session with `peer` begun at or after `since_ms` is up; false when
    /// the caller should go direct. When the circuit lacks only a first
    /// hop (none since the network last changed, say, or just after
    /// start-up), one reconnection at a time goes direct to bring one up,
    /// and the others wait for it, at most [`FIRST_HOP_WAIT`].
    pub(crate) async fn reconnect_hidden(&self, peer: &PublicIdentity, since_ms: u64) -> bool {
        let fp = peer.fingerprint();
        // A call with `peer` runs now: it needs its path back at once,
        // and datagrams for its media, so it goes as before.
        let calling = self.call().is_some_and(|c| c.peer == *peer);
        if !self.prefer_onion() || !self.shared.mutual(peer) || calling || !self.onion_relays(fp) {
            return false;
        }
        let up = || {
            self.sessions()
                .iter()
                .any(|s| s.peer == *peer && s.since_ms >= since_ms)
        };
        let start = Instant::now();
        loop {
            if up() {
                return true;
            }
            let fresh = self.rdv().changed_ms.load(Ordering::Relaxed);
            if self.first_hops(fp, fresh) {
                if let Ok(Ok(_)) =
                    tokio::time::timeout(ONION_DIAL_TIMEOUT, self.connect_onion(fp, 2)).await
                {
                    return true;
                }
                self.emit(Event::ReachNote {
                    note: format!("no onion circuit to {fp}: reconnecting directly"),
                });
                return up();
            }
            if start.elapsed() >= FIRST_HOP_WAIT || self.claim_first_hop(peer) {
                return false;
            }
            tokio::select! {
                () = tokio::time::sleep(FIRST_HOP_POLL) => {}
                () = self.closed() => return false,
            }
        }
    }

    /// Takes the turn to dial `peer` directly as the first hop, unless
    /// another contact has it (for [`FIRST_HOP_CLAIM`]).
    fn claim_first_hop(&self, peer: &PublicIdentity) -> bool {
        let mut c = lock(&self.rdv().first_hop);
        if c.is_some_and(|(p, t)| p != *peer && t.elapsed() < FIRST_HOP_CLAIM) {
            return false;
        }
        *c = Some((*peer, Instant::now()));
        true
    }

    /// [`Node::reach_peer`] for reconnecting in the background (apps
    /// call it on start and when the network comes back): onion first as
    /// [`Node::reconnect_hidden`] says, then volunteer relays, a direct
    /// dial and relay circuits.
    pub async fn reconnect_peer(&self, peer: &PublicIdentity) -> Result<()> {
        if self.connected(peer) || self.reconnect_hidden(peer, 0).await {
            return Ok(());
        }
        let addr = self.contacts().get(peer).and_then(|c| c.last_addr.clone());
        self.reach_with(addr.as_deref(), Some(peer.fingerprint()), false)
            .await
            .map(|_| ())
    }

    /// Makes sure there is a session with `peer`: an existing one, a
    /// direct dial to its last known address, or a relayed circuit.
    pub async fn reach_peer(&self, peer: &PublicIdentity) -> Result<()> {
        if self.sessions().iter().any(|s| s.peer == *peer) {
            return Ok(());
        }
        let addr = self.contacts().get(peer).and_then(|c| c.last_addr.clone());
        self.reach(addr.as_deref(), Some(peer.fingerprint()))
            .await
            .map(|_| ())
    }

    async fn extend_from_origin(
        &self,
        dest: Fingerprint,
        nonce: [u8; 16],
    ) -> Option<(PublicIdentity, u64)> {
        let neighbours: Vec<PublicIdentity> = self
            .sessions()
            .iter()
            .filter(|s| {
                s.via.is_none() && s.peer.fingerprint() != dest && self.shared.mutual(&s.peer)
            })
            .map(|s| s.peer)
            .collect();
        for hop in neighbours {
            let circ = u64::from_le_bytes(random_bytes());
            let (tx, rx) = oneshot::channel();
            lock(&self.shared.relay).pending.insert((hop, circ), tx);
            let open = RelayMsg::Open {
                circ,
                dest: dest.0,
                ttl: MAX_TTL - 1,
                nonce,
            };
            if !self.relay_send(&hop, &open) {
                continue;
            }
            match tokio::time::timeout(OPEN_TIMEOUT * u32::from(MAX_TTL + 1), rx).await {
                Ok(Ok(true)) => return Some((hop, circ)),
                _ => {
                    lock(&self.shared.relay).pending.remove(&(hop, circ));
                }
            }
        }
        None
    }
}

/// Moves whole frames between `stream` and a circuit hop: each frame read
/// goes to `send` (which returns false to stop), and each one from
/// `inbound` is written. Returns when either side ends.
///
/// Reading and writing run as two loops side by side, never as one
/// `select!` per frame: a frame read is two reads, and dropping it halfway
/// when a frame came the other way would lose bytes and garble the stream.
async fn pump_frames<S>(
    stream: S,
    mut inbound: mpsc::UnboundedReceiver<Vec<u8>>,
    mut send: impl FnMut(Vec<u8>) -> bool,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let reading = async {
        while let Ok(Some(f)) = read_frame(&mut rd).await {
            if !send(f) {
                break;
            }
        }
    };
    let writing = async {
        while let Some(f) = inbound.recv().await {
            if write_frame(&mut wr, &f).await.is_err() {
                break;
            }
        }
    };
    tokio::select! {
        () = reading => {}
        () = writing => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn opens_still_searching_count_against_the_cap() {
        let mut st = RelayState::default();
        let peer = threnody_core::Identity::generate().public();
        let other = threnody_core::Identity::generate().public();
        for c in 0..MAX_CIRCUITS_PER_PEER as u64 {
            assert!(st.admit_open(peer, c, random_bytes()));
        }
        // None has a hop yet: each is still searching for a path.
        assert!(!st.admit_open(peer, 1000, random_bytes()));
        assert!(st.admit_open(other, 1000, random_bytes()));
        st.open_done(&peer);
        assert!(st.admit_open(peer, 1001, random_bytes()));
        // A nonce is taken once.
        let nonce = random_bytes();
        assert!(st.admit_open(other, 1, nonce));
        assert!(!st.admit_open(other, 2, nonce));
    }

    #[tokio::test]
    async fn a_frame_read_in_pieces_survives_frames_the_other_way() {
        let (ours, theirs) = tokio::io::duplex(256);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let pump = tokio::spawn(pump_frames(ours, in_rx, move |f| out_tx.send(f).is_ok()));
        let (mut rd, mut wr) = tokio::io::split(theirs);
        // A frame much bigger than the pipe, so the pump reads it in pieces
        // and is still waiting for the rest when a frame comes the other way.
        let frame: Vec<u8> = (0..4000u32).map(|i| i as u8).collect();
        wr.write_all(&(frame.len() as u32).to_be_bytes())
            .await
            .unwrap();
        wr.write_all(&frame[..2000]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        in_tx.send(vec![9; 10]).unwrap();
        let mut back = [0u8; 14];
        rd.read_exact(&mut back).await.unwrap();
        assert_eq!(&back[4..], &[9; 10]);
        wr.write_all(&frame[2000..]).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("the frame never came through")
            .unwrap();
        assert_eq!(got, frame);
        drop(in_tx);
        pump.await.unwrap();
    }

    #[test]
    fn relay_messages_round_trip() {
        for m in [
            RelayMsg::Open {
                circ: 7,
                dest: [1; 20],
                ttl: 2,
                nonce: [3; 16],
            },
            RelayMsg::Opened { circ: u64::MAX },
            RelayMsg::Refused { circ: 0 },
            RelayMsg::Data {
                circ: 9,
                frame: vec![1, 2, 3],
            },
            RelayMsg::Close { circ: 1 },
        ] {
            assert_eq!(RelayMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(RelayMsg::decode(&[0xa1, 0x00, 0x01]).is_err());
    }
}
