//! Handles (Appendix S): publishing ours, answering knocks, and finding
//! someone by theirs.
//!
//! The owner keeps an encrypted record under its handle's keys in the DHT
//! (its identity and addresses) and polls the handle's knock slots. A
//! finder reads the record, checks the handle's code against the identity
//! in it, leaves a knock with its own addresses, and both sides punch
//! toward each other as contacts do (Appendix N), with probes keyed by the
//! handle. The session that results is an ordinary one with a stranger: a
//! message request until accepted.
//!
//! Invite links carry a secret that names a meeting place of the same kind
//! (`threnody_core::invite`), so an invite made on mobile data, whose
//! addresses take no dial, still finds its maker. The fingerprint in the
//! link is what a record there must match.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use std::net::IpAddr;

use mainline::async_dht::AsyncDht;
use mainline::{MutableItem, SigningKey};
use threnody_core::handle::{Handle, HandleRecord, KNOCK_SLOTS, Knock, Meeting};
use threnody_core::invite::{InviteKey, InviteLink, MAX_ADDRS};
use threnody_core::rendezvous::{
    Candidate, CandidateKind, Candidates, FLAG_SYMMETRIC, RecordKeys, epoch,
};
use threnody_core::{Fingerprint, PublicIdentity, now_ms};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};
use crate::reach::{ReachConfig, Urge, get_latest};

/// Knocks older than this are ignored: the finder has stopped punching.
const KNOCK_FRESH_MS: u64 = 5 * 60 * 1000;
/// How long a finder waits for the session.
const FIND_FOR: Duration = Duration::from_secs(120);
/// How long a finder tries the owner's addresses directly first.
const DIRECT_FOR: Duration = Duration::from_secs(5);
/// How often a finder re-reads the owner's record while punching.
const REREAD_EVERY: Duration = Duration::from_secs(10);
/// Most peers kept with handle probe keys.
const MAX_PROBE_KEYS: usize = 64;
/// How long a finder waits for the DHT and its own addresses after the app
/// starts: an invite scanned at once shouldn't fail for that.
const READY_WAIT: Duration = Duration::from_secs(15);
/// Each address in an invite gets this long to answer.
const ADDR_DIAL_FOR: Duration = Duration::from_secs(6);
/// Addresses are dialed this far apart, best first (as Happy Eyeballs
/// does), so two that reach the same device rarely both make a session.
const ADDR_STAGGER: Duration = Duration::from_millis(300);
/// QUIC follows TCP to the same address after this: firewalls (most IPv6
/// ones on mobile networks) drop TCP silently, and UDP may still pass.
const QUIC_AFTER: Duration = Duration::from_millis(500);
/// Where our invite key is kept.
pub const INVITE_KEY_STATE: &str = "invite-key";

#[derive(Default)]
pub(crate) struct HandleState {
    own: Mutex<Option<Own>>,
    /// The meeting place our invite links name, once one was made.
    invite: Mutex<Option<Own>>,
    /// Probe keys for peers we punch toward through a handle: finders
    /// knocking on ours, and owners of handles we looked up.
    pub(crate) probe_keys: Mutex<HashMap<PublicIdentity, [u8; 32]>>,
    pub(crate) kick: Notify,
}

/// What a published record said: epoch, addresses, flags.
type Digest = (u64, Vec<Candidate>, u64);

/// A meeting place in the DHT that we publish.
#[derive(Clone, PartialEq, Eq)]
enum Spot {
    Handle(Handle),
    Invite(InviteKey),
}

impl Meeting for Spot {
    fn base(&self) -> [u8; 32] {
        match self {
            Self::Handle(h) => h.base(),
            Self::Invite(k) => k.base(),
        }
    }
}

impl std::fmt::Display for Spot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handle(h) => write!(f, "handle {h}"),
            Self::Invite(_) => f.write_str("invite"),
        }
    }
}

struct Own {
    spot: Spot,
    version: u64,
    /// What we last published, and when.
    published: Option<(Digest, Instant)>,
    /// Knocks acted on: (knocker, issued).
    seen: HashSet<(PublicIdentity, u64)>,
}

impl Own {
    fn new(spot: Spot, version: u64) -> Self {
        Self {
            spot,
            version,
            published: None,
            seen: HashSet::new(),
        }
    }
}

impl Node {
    fn handles(&self) -> &HandleState {
        &self.rdv().handles
    }

    /// Publishes our handle for `name` at `version` (a new version gives a
    /// new code, and the old handle stops working within the hour), or
    /// stops (`None`). Slow: computing the code takes an Argon2id run.
    pub fn set_handle(&self, name: Option<&str>, version: u64) -> Result<Option<Handle>> {
        let own = match name {
            Some(n) => {
                let handle = Handle::new(&self.identity(), n, version)
                    .map_err(|e| NetError::NotAllowed(e.to_string()))?;
                Some(Own::new(Spot::Handle(handle), version))
            }
            None => None,
        };
        *lock(&self.handles().own) = own;
        self.handles().kick.notify_one();
        Ok(self.handle())
    }

    /// Our handle, if we publish one.
    pub fn handle(&self) -> Option<Handle> {
        match lock(&self.handles().own).as_ref().map(|o| &o.spot) {
            Some(Spot::Handle(h)) => Some(h.clone()),
            _ => None,
        }
    }

    /// The secret our invite links carry, made (and saved) the first time
    /// it is asked for. From then on we keep our addresses in the meeting
    /// place it names and answer knocks there, as for a handle.
    pub fn invite_key(&self) -> InviteKey {
        if let Some(k) = self.load_invite() {
            return k;
        }
        let k = InviteKey::generate();
        self.use_invite_key(k);
        k
    }

    /// Replaces our invite key: invites made so far keep only their
    /// addresses, and stop finding us once we move.
    pub fn reset_invite_key(&self) -> InviteKey {
        let k = InviteKey::generate();
        self.use_invite_key(k);
        k
    }

    fn use_invite_key(&self, k: InviteKey) {
        self.shared
            .save_state(INVITE_KEY_STATE, Ok(k.as_bytes().as_slice()));
        *lock(&self.handles().invite) = Some(Own::new(Spot::Invite(k), 0));
        self.handles().kick.notify_one();
    }

    /// Our invite key, from memory or saved by an earlier run; `None` if no
    /// invite was ever made.
    fn load_invite(&self) -> Option<InviteKey> {
        let mut invite = lock(&self.handles().invite);
        if let Some(Own {
            spot: Spot::Invite(k),
            ..
        }) = invite.as_ref()
        {
            return Some(*k);
        }
        let saved = self
            .shared
            .home
            .load_state(self.identity_ref(), INVITE_KEY_STATE)
            .ok()
            .flatten()?;
        let k = InviteKey::from_bytes(&saved).ok()?;
        *invite = Some(Own::new(Spot::Invite(k), 0));
        Some(k)
    }

    /// An invite link for this device: its addresses (global IPv6 first,
    /// then `addr`, the one the app suggests, then the rest of what we know)
    /// and, with internet reachability on, our invite key.
    pub fn invite_link(&self, addr: Option<&str>) -> InviteLink {
        let candidates = lock(&self.rdv().inner).candidates.clone();
        let of = |kind: CandidateKind| {
            candidates
                .iter()
                .filter(move |c| c.kind == kind && dialable(c.addr.ip()))
                .map(|c| c.addr.to_string())
        };
        let mut addrs: Vec<String> = Vec::new();
        let all = of(CandidateKind::Ipv6)
            .chain(addr.map(str::to_owned))
            .chain(of(CandidateKind::Local))
            .chain(of(CandidateKind::Reflexive));
        for a in all {
            if !addrs.contains(&a) && addrs.len() < MAX_ADDRS {
                addrs.push(a);
            }
        }
        InviteLink {
            fingerprint: self.identity().fingerprint(),
            addrs,
            key: self.reach_enabled().then(|| self.invite_key()),
        }
    }

    /// The newest record at `spot`, this hour's or the last.
    async fn read_record(&self, dht: &AsyncDht, spot: &impl Meeting) -> Option<HandleRecord> {
        let now = epoch(now_ms() / 1000);
        let read = |e: u64| {
            let (dht, rk) = (dht.clone(), spot.record_keys(e));
            async move {
                let v = fetch(&dht, &rk).await?;
                HandleRecord::open(&rk, &v).ok()
            }
        };
        let (a, b) = tokio::join!(read(now), read(now.saturating_sub(1)));
        [a, b]
            .into_iter()
            .flatten()
            .max_by_key(|r| r.candidates.issued_ms)
    }

    /// Reads the record behind `handle` and checks it belongs to the
    /// identity it names.
    pub async fn resolve_handle(&self, handle: &Handle) -> Result<HandleRecord> {
        let dht = self.ready_dht().await?;
        let r = self
            .read_record(&dht, handle)
            .await
            .ok_or_else(|| NetError::NotFound(format!("no one publishes {handle} now")))?;
        let (h, id, v) = (handle.clone(), r.id, r.version);
        let ok = tokio::task::spawn_blocking(move || h.belongs_to(&id, v))
            .await
            .unwrap_or(false);
        if !ok {
            return Err(NetError::NotAllowed(format!(
                "the record under {handle} isn't its owner's (forged?)"
            )));
        }
        Ok(r)
    }

    /// The DHT, waiting a little while it is still being joined.
    async fn ready_dht(&self) -> Result<AsyncDht> {
        let off = || NetError::NotAllowed("this needs internet reachability".into());
        if !self.reach_enabled() {
            return Err(off());
        }
        let deadline = Instant::now() + READY_WAIT;
        loop {
            if let Some(d) = self.dht() {
                return Ok(d);
            }
            if Instant::now() >= deadline || !self.reach_enabled() {
                return Err(off());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Finds whoever owns `handle` and opens a session with them: directly
    /// if their addresses answer, else by knocking and punching (they must
    /// be polling: their app open, or a knock seen within minutes).
    pub async fn find_handle(&self, handle: &str) -> Result<PublicIdentity> {
        let handle: Handle = handle
            .parse()
            .map_err(|e: threnody_core::Error| NetError::NotAllowed(e.to_string()))?;
        let rec = self.resolve_handle(&handle).await?;
        if rec.id == self.identity() {
            return Err(NetError::NotAllowed("that's your own handle".into()));
        }
        self.meet(&Spot::Handle(handle), rec).await
    }

    /// Connects to whoever made `link`: through onion circuits when they
    /// can be had (as `reach` does), else at the link's addresses (all of
    /// them, IPv6 first, over TCP and QUIC), else at the meeting place its
    /// key names, knocking and punching as for a handle, and last through
    /// approved relays.
    pub async fn reach_invite(&self, link: &InviteLink) -> Result<PublicIdentity> {
        let fp = link.fingerprint;
        if fp == self.identity().fingerprint() {
            return Err(NetError::NotAllowed("that's your own invite".into()));
        }
        if let Some(s) = self.sessions().iter().find(|s| s.peer.fingerprint() == fp) {
            return Ok(s.peer);
        }
        let first = link.addrs.first().map(String::as_str);
        if let Some(p) = self.reach_hidden(first, fp).await {
            return Ok(p);
        }
        let mut mismatch = None;
        let mut last = match self.dial_any(&link.addrs, fp).await {
            Ok(p) => return Ok(p),
            Err(e @ NetError::IdentityMismatch { .. }) => {
                // A private address can be someone else's on another
                // network; worth saying only if nothing else works.
                mismatch = Some(e);
                None
            }
            Err(e) => Some(e),
        };
        if let Some(key) = link.key
            && self.reach_enabled()
        {
            match self.find_invite(key, fp).await {
                Ok(p) => return Ok(p),
                Err(e) => last = Some(e),
            }
        }
        match self.connect_relayed(fp).await {
            Ok(p) => Ok(p),
            Err(e) => Err(mismatch.or(last).unwrap_or(e)),
        }
    }

    /// Finds whoever keeps a record under the invite key `key`, which must
    /// be `fp`, and opens a session with them.
    async fn find_invite(&self, key: InviteKey, fp: Fingerprint) -> Result<PublicIdentity> {
        let dht = self.ready_dht().await?;
        let rec = self.read_record(&dht, &key).await.ok_or_else(|| {
            NetError::NotFound("they haven't been online lately; ask them to open Threnody".into())
        })?;
        if rec.id.fingerprint() != fp {
            return Err(NetError::NotAllowed(
                "the record for this invite isn't its maker's (forged?)".into(),
            ));
        }
        self.meet(&Spot::Invite(key), rec).await
    }

    /// Dials each address (best first, a little apart), over TCP and, a
    /// moment later, QUIC; the first session with `fp` wins.
    async fn dial_any(&self, addrs: &[String], fp: Fingerprint) -> Result<PublicIdentity> {
        let mut set = JoinSet::new();
        for (i, a) in addrs.iter().enumerate() {
            let (node, a) = (self.clone(), a.clone());
            let wait = ADDR_STAGGER * u32::try_from(i).unwrap_or(u32::MAX);
            set.spawn(async move {
                tokio::time::sleep(wait).await;
                tokio::time::timeout(ADDR_DIAL_FOR, node.dial_both(&a, fp))
                    .await
                    .unwrap_or(Err(NetError::Timeout))
            });
        }
        let mut last = NetError::NoRoute("no address".into());
        while let Some(r) = set.join_next().await {
            match r {
                Ok(Ok(p)) => {
                    set.abort_all();
                    return Ok(p);
                }
                // Keep a mismatch over a mere failure: it's what matters.
                Ok(Err(e)) if !matches!(last, NetError::IdentityMismatch { .. }) => last = e,
                _ => {}
            }
        }
        Err(last)
    }

    /// One address over TCP, and over QUIC shortly after; the first to
    /// make a session wins.
    async fn dial_both(&self, addr: &str, fp: Fingerprint) -> Result<PublicIdentity> {
        let tcp = self.connect_tcp(addr, Some(fp));
        let quic = async {
            tokio::time::sleep(QUIC_AFTER).await;
            let sa = tokio::net::lookup_host(addr)
                .await?
                .next()
                .ok_or_else(|| NetError::NoRoute(format!("{addr} has no address")))?;
            self.connect_quic(sa, Some(fp)).await
        };
        tokio::pin!(tcp, quic);
        let (mut tcp_done, mut quic_done) = (false, self.quic().is_none());
        loop {
            let e = tokio::select! {
                r = &mut tcp, if !tcp_done => {
                    tcp_done = true;
                    r
                }
                r = &mut quic, if !quic_done => {
                    quic_done = true;
                    r
                }
            };
            match e {
                Ok(p) => return Ok(p),
                Err(e) if tcp_done && quic_done => return Err(e),
                Err(_) => {}
            }
        }
    }

    /// Meets the owner of `spot`, whose record is `rec`: dials its
    /// addresses that take a dial, else knocks and punches while it answers.
    async fn meet(&self, spot: &Spot, rec: HandleRecord) -> Result<PublicIdentity> {
        let peer = rec.id;
        if self.connected(&peer) {
            return Ok(peer);
        }
        self.emit(Event::ReachNote {
            note: format!("{spot} is {}", peer.fingerprint()),
        });
        // Addresses that take a dial (a public IPv6 address, a mapped port).
        if let Some(quic) = self.quic() {
            let mut targets: Vec<_> = rec.candidates.list.clone();
            targets.sort_by_key(|c| c.kind.rank());
            let targets: Vec<_> = targets
                .iter()
                .map(|c| c.addr)
                .filter(|a| quic.can_reach(a.ip()))
                .collect();
            if !targets.is_empty()
                && let Ok(Ok(conn)) = tokio::time::timeout(DIRECT_FOR, self.race(&targets)).await
                && self
                    .quic_session(conn, Some(peer.fingerprint()), false)
                    .await
                    .is_ok()
            {
                return Ok(peer);
            }
        }
        // Knock, and punch while they answer.
        self.add_probe_key(peer, spot.probe_key());
        self.knock(spot).await?;
        let symmetric = rec.candidates.flags & FLAG_SYMMETRIC != 0;
        self.punch(peer, &rec.candidates.list, symmetric, Urge::Start);
        let deadline = Instant::now() + FIND_FOR;
        let mut issued = rec.candidates.issued_ms;
        let mut reread = Instant::now() + REREAD_EVERY;
        while Instant::now() < deadline {
            if self.connected(&peer) {
                return Ok(peer);
            }
            // The owner may have moved since (a phone off Wi-Fi): aim at
            // its new addresses too, or our side of the NAT stays shut.
            // The record must still name the same owner, whose key the
            // first one was checked against.
            if Instant::now() >= reread {
                reread = Instant::now() + REREAD_EVERY;
                if let Some(dht) = self.dht()
                    && let Some(r) = self.read_record(&dht, spot).await
                    && r.id == peer
                    && r.candidates.issued_ms > issued
                {
                    issued = r.candidates.issued_ms;
                    self.emit(Event::ReachNote {
                        note: format!("{spot} moved: {} address(es)", r.candidates.list.len()),
                    });
                    let symmetric = r.candidates.flags & FLAG_SYMMETRIC != 0;
                    self.punch(peer, &r.candidates.list, symmetric, Urge::Start);
                }
                continue;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(NetError::NotFound(match spot {
            Spot::Handle(h) => format!("{h} didn't answer; they may need to open the app"),
            Spot::Invite(_) => "they didn't answer; they may need to open the app".into(),
        }))
    }

    fn add_probe_key(&self, peer: PublicIdentity, key: [u8; 32]) {
        let mut keys = lock(&self.handles().probe_keys);
        if keys.len() >= MAX_PROBE_KEYS && !keys.contains_key(&peer) {
            keys.clear();
        }
        keys.insert(peer, key);
    }

    /// Leaves a knock with our addresses in a random slot of `spot`.
    async fn knock(&self, spot: &impl Meeting) -> Result<()> {
        let dht = self.ready_dht().await?;
        // Just after start (or a network change) our addresses may not be
        // known yet: give gathering a moment.
        let deadline = Instant::now() + READY_WAIT;
        let candidates = loop {
            let c = self.own_candidates();
            if !c.list.is_empty() {
                break c;
            }
            if Instant::now() >= deadline {
                return Err(NetError::NoRoute("our own address isn't known yet".into()));
            }
            self.rdv().regather.notify_one();
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        let slot = threnody_core::crypto::random_bytes::<1>()[0] % KNOCK_SLOTS;
        let rk = spot.knock_keys(epoch(now_ms() / 1000), slot);
        let value = Knock {
            id: self.identity(),
            candidates,
        }
        .seal(&rk)
        .map_err(|e| NetError::NotAllowed(e.to_string()))?;
        put(&dht, &rk, &value, now_ms())
            .await
            .map_err(|e| NetError::NoRoute(format!("knocking failed: {e}")))
    }

    fn own_candidates(&self) -> Candidates {
        let inner = lock(&self.rdv().inner);
        Candidates {
            list: inner.candidates.clone(),
            issued_ms: now_ms(),
            flags: if inner.symmetric { FLAG_SYMMETRIC } else { 0 },
            seeking_until_ms: None,
        }
    }

    /// Keeps our handle's and our invite's records current and answers
    /// knocks, polling at the foreground or background rate.
    pub(crate) async fn handle_loop(&self, cfg: &ReachConfig) {
        // Invites made in an earlier run still lead here.
        self.load_invite();
        loop {
            if self.reach_enabled()
                && let Some(dht) = self.dht()
            {
                let state = self.handles();
                tokio::join!(
                    self.publish_spot(&dht, cfg, &state.own),
                    self.publish_spot(&dht, cfg, &state.invite),
                );
                self.answer_knocks(&dht).await;
            }
            let wait = if self.rdv().foreground.load(Ordering::Relaxed) {
                cfg.poll_foreground
            } else {
                cfg.poll_background
            };
            if !self.pause(wait, &self.handles().kick).await {
                break;
            }
        }
    }

    async fn publish_spot(&self, dht: &AsyncDht, cfg: &ReachConfig, slot: &Mutex<Option<Own>>) {
        let candidates = self.own_candidates();
        if candidates.list.is_empty() {
            return;
        }
        let ep = epoch(now_ms() / 1000);
        let (spot, record, digest) = {
            let own = lock(slot);
            let Some(o) = own.as_ref() else { return };
            let digest = (ep, candidates.list.clone(), candidates.flags);
            if o.published
                .as_ref()
                .is_some_and(|(d, at)| *d == digest && at.elapsed() < cfg.republish)
            {
                return;
            }
            let record = HandleRecord {
                id: self.identity(),
                version: o.version,
                candidates,
            };
            (o.spot.clone(), record, digest)
        };
        let rk = spot.record_keys(ep);
        let Ok(value) = record.seal(&rk) else { return };
        let note = match put(dht, &rk, &value, record.candidates.issued_ms).await {
            Ok(()) => {
                if let Some(o) = lock(slot).as_mut()
                    && o.spot == spot
                {
                    o.published = Some((digest, Instant::now()));
                }
                format!("published {spot}")
            }
            Err(e) => format!("publishing {spot} failed: {e}"),
        };
        self.emit(Event::ReachNote { note });
    }

    async fn answer_knocks(&self, dht: &AsyncDht) {
        let state = self.handles();
        let spots: Vec<(&Mutex<Option<Own>>, Spot)> = [&state.own, &state.invite]
            .into_iter()
            .filter_map(|m| lock(m).as_ref().map(|o| (m, o.spot.clone())))
            .collect();
        if spots.is_empty() {
            return;
        }
        let now = now_ms();
        let ep = epoch(now / 1000);
        // Last hour's slots only while a knock left there can still be
        // fresh: half the reads the rest of the time.
        let into_hour_ms = now % 3_600_000;
        let mut epochs = vec![ep];
        if into_hour_ms < KNOCK_FRESH_MS {
            epochs.push(ep.saturating_sub(1));
        }
        // All slots at once: one at a time, a knock waited up to a minute.
        let mut gets = JoinSet::new();
        for (i, (_, spot)) in spots.iter().enumerate() {
            for &e in &epochs {
                for slot in 0..KNOCK_SLOTS {
                    let (dht, rk) = (dht.clone(), spot.knock_keys(e, slot));
                    gets.spawn(async move {
                        let v = fetch(&dht, &rk).await?;
                        Knock::open(&rk, &v).ok().map(|k| (i, k))
                    });
                }
            }
        }
        while let Some(r) = gets.join_next().await {
            let Ok(Some((i, k))) = r else { continue };
            let (slot, spot) = &spots[i];
            let fresh = now.saturating_sub(k.candidates.issued_ms) < KNOCK_FRESH_MS;
            if k.id == self.identity() || !fresh || self.connected(&k.id) {
                continue;
            }
            let new = lock(slot)
                .as_mut()
                .is_some_and(|o| o.spot == *spot && o.seen.insert((k.id, k.candidates.issued_ms)));
            if !new {
                continue;
            }
            self.emit(Event::ReachNote {
                note: format!("{} knocked on our {spot}", k.id.fingerprint()),
            });
            self.add_probe_key(k.id, spot.probe_key());
            let symmetric = k.candidates.flags & FLAG_SYMMETRIC != 0;
            self.punch(k.id, &k.candidates.list, symmetric, Urge::Start);
        }
    }
}

/// Whether an address of ours is worth putting in an invite: not loopback,
/// link-local or the 464XLAT address (192.0.0.0/29) phones on IPv6-only
/// networks have, which nobody else can reach.
fn dialable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.octets()[..3] == [192, 0, 0])
        }
        IpAddr::V6(_) => crate::reach::is_global_v6(&ip),
    }
}

async fn fetch(dht: &AsyncDht, rk: &RecordKeys) -> Option<Vec<u8>> {
    let key = SigningKey::from_bytes(&rk.signing_seed)
        .verifying_key()
        .to_bytes();
    get_latest(dht, &key, &rk.salt)
        .await
        .map(|i| i.value().to_vec())
}

async fn put(
    dht: &AsyncDht,
    rk: &RecordKeys,
    value: &[u8],
    seq_ms: u64,
) -> std::result::Result<(), String> {
    let signer = SigningKey::from_bytes(&rk.signing_seed);
    let seq = i64::try_from(seq_ms).unwrap_or(i64::MAX);
    let item = MutableItem::new(signer, value, seq, Some(&rk.salt));
    dht.put_mutable(item, None)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
