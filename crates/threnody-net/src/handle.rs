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

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use mainline::async_dht::AsyncDht;
use mainline::{MutableItem, SigningKey};
use threnody_core::handle::{Handle, HandleRecord, KNOCK_SLOTS, Knock};
use threnody_core::rendezvous::{Candidate, Candidates, FLAG_SYMMETRIC, RecordKeys, epoch};
use threnody_core::{PublicIdentity, now_ms};
use tokio::sync::Notify;

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

#[derive(Default)]
pub(crate) struct HandleState {
    own: Mutex<Option<Own>>,
    /// Probe keys for peers we punch toward through a handle: finders
    /// knocking on ours, and owners of handles we looked up.
    pub(crate) probe_keys: Mutex<HashMap<PublicIdentity, [u8; 32]>>,
    kick: Notify,
}

/// What a published record said: epoch, addresses, flags.
type Digest = (u64, Vec<Candidate>, u64);

struct Own {
    handle: Handle,
    version: u64,
    /// What we last published, and when.
    published: Option<(Digest, Instant)>,
    /// Knocks acted on: (knocker, issued).
    seen: HashSet<(PublicIdentity, u64)>,
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
                Some(Own {
                    handle,
                    version,
                    published: None,
                    seen: HashSet::new(),
                })
            }
            None => None,
        };
        let h = own.as_ref().map(|o| o.handle.clone());
        *lock(&self.handles().own) = own;
        self.handles().kick.notify_one();
        Ok(h)
    }

    /// Our handle, if we publish one.
    pub fn handle(&self) -> Option<Handle> {
        lock(&self.handles().own).as_ref().map(|o| o.handle.clone())
    }

    /// Reads the record behind `handle` and checks it belongs to the
    /// identity it names.
    pub async fn resolve_handle(&self, handle: &Handle) -> Result<HandleRecord> {
        let dht = self.dht().ok_or_else(|| {
            NetError::NotAllowed("finding a handle needs internet reachability".into())
        })?;
        let now = epoch(now_ms() / 1000);
        let mut best: Option<HandleRecord> = None;
        let read = |e: u64| {
            let (dht, rk) = (dht.clone(), handle.record_keys(e));
            async move {
                let v = fetch(&dht, &rk).await?;
                HandleRecord::open(&rk, &v).ok()
            }
        };
        let (a, b) = tokio::join!(read(now), read(now.saturating_sub(1)));
        for r in [a, b].into_iter().flatten() {
            if best
                .as_ref()
                .is_none_or(|b| r.candidates.issued_ms > b.candidates.issued_ms)
            {
                best = Some(r);
            }
        }
        let r = best.ok_or_else(|| NetError::NotFound(format!("no one publishes {handle} now")))?;
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

    /// Finds whoever owns `handle` and opens a session with them: directly
    /// if their addresses answer, else by knocking and punching (they must
    /// be polling: their app open, or a knock seen within minutes).
    pub async fn find_handle(&self, handle: &str) -> Result<PublicIdentity> {
        let handle: Handle = handle
            .parse()
            .map_err(|e: threnody_core::Error| NetError::NotAllowed(e.to_string()))?;
        let rec = self.resolve_handle(&handle).await?;
        let peer = rec.id;
        if peer == self.identity() {
            return Err(NetError::NotAllowed("that's your own handle".into()));
        }
        if self.connected(&peer) {
            return Ok(peer);
        }
        self.emit(Event::ReachNote {
            note: format!("{handle} is {}", peer.fingerprint()),
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
        self.add_probe_key(peer, handle.probe_key());
        self.knock(&handle).await?;
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
            if Instant::now() >= reread {
                reread = Instant::now() + REREAD_EVERY;
                if let Ok(r) = self.resolve_handle(&handle).await
                    && r.id == peer
                    && r.candidates.issued_ms > issued
                {
                    issued = r.candidates.issued_ms;
                    self.emit(Event::ReachNote {
                        note: format!("{handle} moved: {} address(es)", r.candidates.list.len()),
                    });
                    let symmetric = r.candidates.flags & FLAG_SYMMETRIC != 0;
                    self.punch(peer, &r.candidates.list, symmetric, Urge::Start);
                }
                continue;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(NetError::NotFound(format!(
            "{handle} didn't answer; they may need to open the app"
        )))
    }

    fn add_probe_key(&self, peer: PublicIdentity, key: [u8; 32]) {
        let mut keys = lock(&self.handles().probe_keys);
        if keys.len() >= MAX_PROBE_KEYS && !keys.contains_key(&peer) {
            keys.clear();
        }
        keys.insert(peer, key);
    }

    /// Leaves a knock with our addresses in a random slot of `handle`.
    async fn knock(&self, handle: &Handle) -> Result<()> {
        let dht = self
            .dht()
            .ok_or_else(|| NetError::NotAllowed("internet reachability is off".into()))?;
        let candidates = self.own_candidates();
        if candidates.list.is_empty() {
            return Err(NetError::NoRoute("our own address isn't known yet".into()));
        }
        let slot = threnody_core::crypto::random_bytes::<1>()[0] % KNOCK_SLOTS;
        let rk = handle.knock_keys(epoch(now_ms() / 1000), slot);
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

    /// Keeps our handle's record current and answers knocks, polling at
    /// the foreground or background rate.
    pub(crate) async fn handle_loop(&self, cfg: &ReachConfig) {
        loop {
            if self.reach_enabled()
                && let Some(dht) = self.dht()
            {
                self.publish_handle(&dht, cfg).await;
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

    async fn publish_handle(&self, dht: &AsyncDht, cfg: &ReachConfig) {
        let candidates = self.own_candidates();
        if candidates.list.is_empty() {
            return;
        }
        let ep = epoch(now_ms() / 1000);
        let (handle, record, digest) = {
            let own = lock(&self.handles().own);
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
            (o.handle.clone(), record, digest)
        };
        let rk = handle.record_keys(ep);
        let Ok(value) = record.seal(&rk) else { return };
        let note = match put(dht, &rk, &value, record.candidates.issued_ms).await {
            Ok(()) => {
                if let Some(o) = lock(&self.handles().own).as_mut()
                    && o.handle == handle
                {
                    o.published = Some((digest, Instant::now()));
                }
                format!("published handle {handle}")
            }
            Err(e) => format!("publishing handle {handle} failed: {e}"),
        };
        self.emit(Event::ReachNote { note });
    }

    async fn answer_knocks(&self, dht: &AsyncDht) {
        let Some(handle) = self.handle() else { return };
        let now = now_ms();
        let ep = epoch(now / 1000);
        // All slots at once: one at a time, a knock waited up to a minute.
        let mut gets = tokio::task::JoinSet::new();
        for e in [ep, ep.saturating_sub(1)] {
            for slot in 0..KNOCK_SLOTS {
                let (dht, handle) = (dht.clone(), handle.clone());
                gets.spawn(async move {
                    let rk = handle.knock_keys(e, slot);
                    let v = fetch(&dht, &rk).await?;
                    Knock::open(&rk, &v).ok()
                });
            }
        }
        while let Some(r) = gets.join_next().await {
            let Ok(Some(k)) = r else { continue };
            let fresh = now.saturating_sub(k.candidates.issued_ms) < KNOCK_FRESH_MS;
            if k.id == self.identity() || !fresh || self.connected(&k.id) {
                continue;
            }
            let new = lock(&self.handles().own)
                .as_mut()
                .is_some_and(|o| o.seen.insert((k.id, k.candidates.issued_ms)));
            if !new {
                continue;
            }
            self.emit(Event::ReachNote {
                note: format!("{} knocked on {handle}", k.id.fingerprint()),
            });
            self.add_probe_key(k.id, handle.probe_key());
            let symmetric = k.candidates.flags & FLAG_SYMMETRIC != 0;
            self.punch(k.id, &k.candidates.list, symmetric, Urge::Start);
        }
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
