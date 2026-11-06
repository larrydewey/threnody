//! Delivery and read receipts that reach a sender who is offline
//! (`docs/appendix-h-offline.md`, *Receipts*).
//!
//! A sealed text from a mailbox, or a message the user displays, earns its
//! sending device a receipt. If that device is connected the receipt goes
//! at once; otherwise it waits here, per device, and goes at the start of
//! the next session with it, or sealed through mailboxes after a random
//! delay, batched into one sealed message. The waiting receipts, the
//! read-receipt switch and when this outbox was created are kept in
//! encrypted state (`receipts-out`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use threnody_core::message::{FEATURE_READ, FEATURE_RECEIPTS};
use threnody_core::receipt::{MAX_IDS, Receipt};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::delivery::Tag;
use crate::node::{Event, Node, lock};

/// A receipt for a device we can't reach live waits a random time in this
/// window before it is sealed for mailboxes, so its deposit doesn't line up
/// with our mailbox fetch, and receipts that come close together go as one.
pub const MIN_DELAY_MS: u64 = 30_000;
pub const MAX_DELAY_MS: u64 = 300_000;
/// How often due receipts are looked at.
pub const TICK: Duration = Duration::from_secs(2);
/// Ids kept per device and kind; the oldest go first.
pub const MAX_PENDING: usize = MAX_IDS;
/// Receipts not sent by then are dropped (a mailbox would expire them too).
pub const MAX_AGE_MS: u64 = crate::mailbox::MAX_HOLD_MS;
/// At most one sealed receipt per device this often. Each one takes a slot
/// in the sender's mailbox queue (100 per recipient) and may wake its
/// phone, so a reader who reads in many short bursts must not crowd out
/// real messages.
pub const MIN_SEALED_GAP_MS: u64 = 15 * 60_000;
pub(crate) const STATE: &str = "receipts-out";
const VERSION: u8 = 1;
/// Flags byte: read receipts are off.
const READS_OFF: u8 = 1;

/// Receipts waiting for one device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pending {
    /// Ids that arrived. One also in `read` goes as read only.
    pub delivered: Vec<u64>,
    /// Ids that were displayed (read implies delivered).
    pub read: Vec<u64>,
    /// When this batch began.
    pub since_ms: u64,
    /// When it may go sealed.
    pub due_ms: u64,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.delivered.is_empty() && self.read.is_empty()
    }

    /// How many messages it names.
    fn len(&self) -> usize {
        let read: HashSet<u64> = self.read.iter().copied().collect();
        self.read.len() + self.delivered.iter().filter(|i| !read.contains(i)).count()
    }

    /// Adds ids, skipping those already here; each list keeps its newest
    /// [`MAX_PENDING`]. Returns whether anything was added.
    fn extend(&mut self, delivered: &[u64], read: &[u64]) -> bool {
        let before = (self.delivered.len(), self.read.len());
        push_new(&mut self.read, read);
        push_new(&mut self.delivered, delivered);
        let added = (self.delivered.len(), self.read.len()) != before;
        cap(&mut self.read);
        cap(&mut self.delivered);
        added
    }

    /// What to send a device whose `Hello` says `features`: with
    /// [`FEATURE_RECEIPTS`], `Receipt`s (at most [`MAX_IDS`] ids per list
    /// each); else with [`FEATURE_READ`], `Read`s for the read ids only
    /// (delivered can't be said to it); else nothing. `conversation` is
    /// ours with that device's account, as `Read` names it.
    pub fn messages(&self, features: u64, conversation: &[u8]) -> Vec<AppMessage> {
        if features & FEATURE_RECEIPTS == 0 {
            return read_messages(features, conversation, &self.read);
        }
        let read: HashSet<u64> = self.read.iter().copied().collect();
        let delivered: Vec<u64> = self
            .delivered
            .iter()
            .copied()
            .filter(|i| !read.contains(i))
            .collect();
        let (d, r): (Vec<&[u64]>, Vec<&[u64]>) = (
            delivered.chunks(MAX_IDS).collect(),
            self.read.chunks(MAX_IDS).collect(),
        );
        (0..d.len().max(r.len()))
            .filter_map(|i| {
                Receipt {
                    delivered: d.get(i).map_or_else(Vec::new, |c| c.to_vec()),
                    read: r.get(i).map_or_else(Vec::new, |c| c.to_vec()),
                }
                .encode()
                .ok()
                .map(AppMessage::Receipt)
            })
            .collect()
    }
}

/// Read receipts for `ids`, to a device whose `Hello` says `features`, in
/// messages of at most [`MAX_IDS`] ids each: `Receipt`s if it takes them,
/// else `Read`s if it takes those, else none.
pub(crate) fn read_messages(features: u64, conversation: &[u8], ids: &[u64]) -> Vec<AppMessage> {
    if features & FEATURE_RECEIPTS != 0 {
        ids.chunks(MAX_IDS)
            .filter_map(|c| {
                Receipt {
                    delivered: Vec::new(),
                    read: c.to_vec(),
                }
                .encode()
                .ok()
                .map(AppMessage::Receipt)
            })
            .collect()
    } else if features & FEATURE_READ != 0 {
        ids.chunks(MAX_IDS)
            .map(|c| AppMessage::Read {
                conversation: conversation.to_vec(),
                ids: c.to_vec(),
            })
            .collect()
    } else {
        Vec::new()
    }
}

/// Moves `r`'s read ids into its delivered ones (read implies delivered),
/// as far as a list holds. Returns whether it had any.
fn reads_as_delivered(r: &mut Receipt) -> bool {
    if r.read.is_empty() {
        return false;
    }
    let mut have: HashSet<u64> = r.delivered.iter().copied().collect();
    for i in std::mem::take(&mut r.read) {
        if r.delivered.len() < MAX_IDS && have.insert(i) {
            r.delivered.push(i);
        }
    }
    true
}

/// A `Receipt` payload with its reads made deliveries, for
/// [`crate::delivery::Delivery::rewrite`]: `None` if it has none (or
/// can't be read, which [`Node::receipt_ready`] drops anyway).
fn without_reads(payload: &[u8]) -> Option<Option<AppMessage>> {
    let mut r = Receipt::decode(payload).ok()?;
    if !reads_as_delivered(&mut r) {
        return None;
    }
    Some(r.encode().ok().map(AppMessage::Receipt))
}

fn push_new(to: &mut Vec<u64>, ids: &[u64]) {
    let mut have: HashSet<u64> = to.iter().copied().collect();
    for &i in ids {
        if i != 0 && have.insert(i) {
            to.push(i);
        }
    }
}

fn cap(ids: &mut Vec<u64>) {
    let extra = ids.len().saturating_sub(MAX_PENDING);
    ids.drain(..extra);
}

/// Receipts waiting per device, and the read-receipt switch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Outbox {
    pending: HashMap<PublicIdentity, Pending>,
    /// When we last sealed a receipt for each device (for
    /// [`MIN_SEALED_GAP_MS`]); older ones than that are pruned.
    last_sealed: HashMap<PublicIdentity, u64>,
    /// The read-receipt switch. Persisted, so it holds from the first
    /// instant after a restart. On by default.
    pub reads_on: bool,
    /// When this outbox was first created. Reads of messages older than
    /// this are never queued, so an upgrade doesn't seal the whole backlog.
    pub since_ms: u64,
    /// Changed since last saved.
    pub dirty: bool,
}

impl Outbox {
    /// A fresh outbox (no state yet): reads on, created now, and dirty, so
    /// it is saved and its creation time stays.
    pub fn new(now: u64) -> Self {
        Self {
            pending: HashMap::new(),
            last_sealed: HashMap::new(),
            reads_on: true,
            since_ms: now,
            dirty: true,
        }
    }

    /// `VERSION || flags u8 (1 = reads off) || since u64 BE
    ///  || n u16 BE || n × (device 32 | last sealed u64 BE)
    ///  || *(device 32 | since u64 BE | due u64 BE | nd u16 BE | nr u16 BE
    ///       | nd × id u64 BE | nr × id u64 BE)`.
    /// A damaged tail is skipped. An empty buffer or another version gives
    /// `None`, for the caller to start afresh.
    pub fn load(b: &[u8]) -> Option<Self> {
        let mut r = Reader(b);
        if r.u8()? != VERSION {
            return None;
        }
        let flags = r.u8()?;
        let since_ms = r.u64()?;
        let mut o = Self {
            pending: HashMap::new(),
            last_sealed: HashMap::new(),
            reads_on: flags & READS_OFF == 0,
            since_ms,
            dirty: false,
        };
        let n = r.u16()?;
        for _ in 0..n {
            let (Some(d), Some(t)) = (r.device(), r.u64()) else {
                return Some(o);
            };
            if let Some(d) = d {
                o.last_sealed.insert(d, t);
            }
        }
        while !r.0.is_empty() {
            let Some(batch) = (|| {
                let d = r.device()?;
                let (since_ms, due_ms) = (r.u64()?, r.u64()?);
                let (nd, nr) = (usize::from(r.u16()?), usize::from(r.u16()?));
                let delivered = (0..nd).map(|_| r.u64()).collect::<Option<Vec<_>>>()?;
                let read = (0..nr).map(|_| r.u64()).collect::<Option<Vec<_>>>()?;
                Some((d, since_ms, due_ms, delivered, read))
            })() else {
                break;
            };
            let (Some(d), since_ms, due_ms, delivered, read) = batch else {
                continue;
            };
            let mut p = Pending {
                since_ms,
                due_ms,
                ..Pending::default()
            };
            p.extend(&delivered, if o.reads_on { &read } else { &[] });
            if !p.is_empty() {
                o.pending.insert(d, p);
            }
        }
        Some(o)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION, if self.reads_on { 0 } else { READS_OFF }];
        out.extend_from_slice(&self.since_ms.to_be_bytes());
        let sealed: Vec<_> = self
            .last_sealed
            .iter()
            .take(usize::from(u16::MAX))
            .collect();
        out.extend_from_slice(&(sealed.len() as u16).to_be_bytes());
        for (d, t) in sealed {
            out.extend_from_slice(d.as_bytes());
            out.extend_from_slice(&t.to_be_bytes());
        }
        for (d, p) in &self.pending {
            out.extend_from_slice(d.as_bytes());
            out.extend_from_slice(&p.since_ms.to_be_bytes());
            out.extend_from_slice(&p.due_ms.to_be_bytes());
            // Each list is capped at MAX_PENDING, far below u16::MAX.
            out.extend_from_slice(&(p.delivered.len() as u16).to_be_bytes());
            out.extend_from_slice(&(p.read.len() as u16).to_be_bytes());
            for i in p.delivered.iter().chain(&p.read) {
                out.extend_from_slice(&i.to_be_bytes());
            }
        }
        out
    }

    /// Adds ids for `device`, ignoring those already waiting (and reads
    /// while the switch is off). A new batch (none before) begins `now`
    /// and is due after `delay`; later additions join it.
    pub fn add(
        &mut self,
        device: PublicIdentity,
        delivered: &[u64],
        read: &[u64],
        now: u64,
        delay: u64,
    ) {
        let read = if self.reads_on { read } else { &[] };
        let p = self.pending.entry(device).or_insert_with(|| Pending {
            since_ms: now,
            due_ms: now.saturating_add(delay),
            ..Pending::default()
        });
        if p.extend(delivered, read) {
            self.dirty = true;
        }
        if p.is_empty() {
            self.pending.remove(&device);
        }
    }

    /// Takes `device`'s batch, to send.
    pub fn take(&mut self, device: &PublicIdentity) -> Option<Pending> {
        let p = self.pending.remove(device)?;
        self.dirty = true;
        Some(p)
    }

    /// Puts back a batch that couldn't go, with anything added meanwhile,
    /// due again at `due`.
    pub fn restore(&mut self, device: PublicIdentity, mut p: Pending, due: u64) {
        if !self.reads_on {
            p.read.clear();
        }
        if let Some(newer) = self.pending.remove(&device) {
            p.since_ms = p.since_ms.min(newer.since_ms);
            p.extend(&newer.delivered, &newer.read);
        }
        p.due_ms = due;
        if !p.is_empty() {
            self.pending.insert(device, p);
        }
        self.dirty = true;
    }

    /// The devices whose batch is due at `now` and whose last sealed
    /// receipt is at least [`MIN_SEALED_GAP_MS`] old. Batches older than
    /// [`MAX_AGE_MS`] are dropped. With `all`, every batch counts as due,
    /// whatever its time and gap (for tests).
    pub fn due(&mut self, now: u64, all: bool) -> Vec<PublicIdentity> {
        let (batches, sealed) = (self.pending.len(), self.last_sealed.len());
        self.pending
            .retain(|_, p| now.saturating_sub(p.since_ms) < MAX_AGE_MS);
        self.last_sealed
            .retain(|_, t| now.saturating_sub(*t) < MIN_SEALED_GAP_MS);
        self.dirty |= (batches, sealed) != (self.pending.len(), self.last_sealed.len());
        self.pending
            .iter()
            .filter(|(d, p)| all || (p.due_ms <= now && !self.last_sealed.contains_key(d)))
            .map(|(d, _)| *d)
            .collect()
    }

    /// Records that a receipt was sealed for `device` at `now`.
    pub fn sealed(&mut self, device: PublicIdentity, now: u64) {
        self.last_sealed.insert(device, now);
        self.dirty = true;
    }

    /// Sets the switch. Off also forgets every read still waiting; a batch
    /// left empty goes.
    pub fn set_reads(&mut self, on: bool) {
        if self.reads_on != on {
            self.reads_on = on;
            self.dirty = true;
        }
        if on {
            return;
        }
        for p in self.pending.values_mut() {
            if !p.read.is_empty() {
                p.read.clear();
                self.dirty = true;
            }
        }
        self.pending.retain(|_, p| !p.is_empty());
    }

    /// Drops `device`'s batch (blocked, removed or revoked).
    pub fn forget(&mut self, device: &PublicIdentity) {
        if self.pending.remove(device).is_some() {
            self.dirty = true;
        }
    }

    /// How many messages the waiting receipts name, in all.
    pub fn len(&self) -> usize {
        self.pending.values().map(Pending::len).sum()
    }
}

impl Node {
    /// Whether displayed messages are reported to their senders (on
    /// unless turned off). Kept with the waiting receipts, so it holds
    /// from the first instant after a restart.
    pub fn send_read_receipts(&self) -> bool {
        self.shared.receipts(|o| o.reads_on)
    }

    /// Turns read receipts on or off. Off also drops every read still
    /// waiting to be sent: nothing leaves after the user says no. That
    /// includes receipts already sent, tracked, whose session died before
    /// they were acknowledged: they would go again in the next session, so
    /// their reads become plain deliveries (read implies delivered).
    pub fn set_send_read_receipts(&self, on: bool) {
        self.shared.receipts(|o| o.set_reads(on));
        if !on {
            self.shared.delivery(|d| {
                d.rewrite(|_, m| match m {
                    AppMessage::Receipt(b) => without_reads(b),
                    _ => None,
                });
            });
        }
    }

    /// Tests only: how long a receipt waits before it is sealed (a random
    /// time from `min_ms` to `max_ms`).
    #[doc(hidden)]
    pub fn set_receipt_delay(&self, min_ms: u64, max_ms: u64) {
        *lock(&self.shared.receipt_delay) = (min_ms, max_ms.max(min_ms));
    }

    /// Tests only: sends every waiting receipt that can go now, whatever
    /// its delay.
    #[doc(hidden)]
    pub fn flush_receipts_now(&self) {
        self.flush_receipts(true);
    }

    /// Tests only: how many messages the waiting receipts name.
    #[doc(hidden)]
    pub fn pending_receipts(&self) -> usize {
        self.shared.receipts(|o| o.len())
    }

    /// Tests only: moves when the outbox began (see [`Outbox::since_ms`]).
    #[doc(hidden)]
    pub fn set_receipts_since(&self, since_ms: u64) {
        self.shared.receipts(|o| {
            o.since_ms = since_ms;
            o.dirty = true;
        });
    }

    /// A random wait in the delay window.
    fn receipt_delay(&self) -> u64 {
        let (min, max) = *lock(&self.shared.receipt_delay);
        min + ((max - min) as f64 * crate::push::unit()) as u64
    }

    /// The feature bits `device` last said it has: in a session since we
    /// started, else as saved with the contact. `None` if never heard.
    fn known_features(&self, device: &PublicIdentity) -> Option<u64> {
        let latest = lock(&self.shared.peer_features).get(device).copied();
        latest.or_else(|| {
            lock(&self.shared.contacts)
                .get(device)
                .map(|c| c.features)
                .filter(|f| *f != 0)
        })
    }

    /// Whether `device` may get our receipts: someone else's device, of a
    /// contact we accepted (not blocked), not removed from its account.
    /// Checked when a receipt is queued and again when it goes, so one
    /// blocked, removed or revoked meanwhile gets nothing.
    fn receipt_target_ok(&self, device: &PublicIdentity) -> bool {
        *device != self.identity()
            && !self.is_own_device(device)
            && self.is_accepted(device)
            && !self.is_revoked(device)
    }

    /// Sends `delivered` and `read` to `device` over its session, by what
    /// its `Hello` in this run said: `Receipt`s, or `Read`s for the read
    /// ones to an older peer (delivered can't be said to it). `None` if
    /// there is no session, or its `Hello` hasn't come yet; else whether
    /// anything went.
    fn send_receipt_live(
        &self,
        device: &PublicIdentity,
        delivered: &[u64],
        read: &[u64],
    ) -> Option<bool> {
        if !self.connected(device) {
            return None;
        }
        let features = lock(&self.shared.peer_features).get(device).copied()?;
        let batch = Pending {
            delivered: delivered.to_vec(),
            read: read.to_vec(),
            ..Pending::default()
        };
        if features & FEATURE_RECEIPTS != 0 {
            let msgs = batch.messages(features, &[]);
            Some(
                !msgs.is_empty()
                    && msgs
                        .into_iter()
                        .all(|m| self.send_tracked(device, m).is_ok()),
            )
        } else {
            let conv = self.conversation_for(device).to_bytes();
            let msgs = batch.messages(features, &conv);
            Some(!msgs.is_empty() && msgs.into_iter().all(|m| self.send(device, m).is_ok()))
        }
    }

    /// A tracked `Receipt` (`payload`) about to go to `peer`, whose `Hello`
    /// in this session said `features`. `Ok` with the payload to send:
    /// with read receipts off by now, its reads are only deliveries. `Err`
    /// with what to send instead, untracked: to a device without
    /// [`FEATURE_RECEIPTS`] (an older build, though it had the bit when
    /// this was queued), its reads as a `Read` if it takes one, else
    /// nothing.
    pub(crate) fn receipt_ready(
        &self,
        peer: &PublicIdentity,
        features: u64,
        payload: Vec<u8>,
    ) -> Result<Vec<u8>, Option<AppMessage>> {
        let Ok(mut r) = Receipt::decode(&payload) else {
            return Err(None);
        };
        let changed = !self.send_read_receipts() && reads_as_delivered(&mut r);
        if features & FEATURE_RECEIPTS == 0 {
            let conv = self.conversation_for(peer).to_bytes();
            return Err(read_messages(features, &conv, &r.read).into_iter().next());
        }
        if !changed {
            return Ok(payload);
        }
        r.encode().map_err(|_| None)
    }

    /// Queues receipts for `device`, the device that sent the messages:
    /// at once if it is connected, else to wait for it (see
    /// [`Node::flush_receipts`]). Reads are dropped with read receipts
    /// off. Returns whether anything was sent or queued.
    pub(crate) fn queue_receipt(
        &self,
        device: &PublicIdentity,
        delivered: &[u64],
        read: &[u64],
    ) -> bool {
        if !self.receipt_target_ok(device) {
            return false;
        }
        let read = if self.send_read_receipts() { read } else { &[] };
        if delivered.is_empty() && read.is_empty() {
            return false;
        }
        if let Some(sent) = self.send_receipt_live(device, delivered, read)
            && (sent || self.lacks(device, FEATURE_RECEIPTS))
        {
            return sent;
        }
        // Not connected (or it went down meanwhile): it waits, unless the
        // device is known not to take what we would send it.
        let mut delivered = delivered;
        if let Some(f) = self.known_features(device)
            && f & FEATURE_RECEIPTS == 0
        {
            delivered = &[];
            if f & FEATURE_READ == 0 || read.is_empty() {
                return false;
            }
        }
        let (now, delay) = (now_ms(), self.receipt_delay());
        self.shared
            .receipts(|o| o.add(*device, delivered, read, now, delay));
        true
    }

    /// The receipts waiting for `peer`, whose `Hello` just said
    /// `features`: to send first thing in this session. A `Receipt` is
    /// tracked, so it is sent again if the session dies.
    pub(crate) fn receipts_for(&self, peer: &PublicIdentity, features: u64) -> Vec<AppMessage> {
        if !self.receipt_target_ok(peer) {
            self.shared.receipts(|o| o.forget(peer));
            return Vec::new();
        }
        let Some(mut p) = self.shared.receipts(|o| o.take(peer)) else {
            return Vec::new();
        };
        if !self.send_read_receipts() {
            p.read.clear();
        }
        let conv = self.conversation_for(peer).to_bytes();
        p.messages(features, &conv)
            .into_iter()
            .map(|m| match m {
                AppMessage::Receipt(_) => self.shared.delivery(|d| d.track(peer, m, Tag::NONE)),
                m => m,
            })
            .collect()
    }

    /// Sends the receipts that are due (with `all`, every one): over a
    /// session with their device if there is one, else sealed through
    /// mailboxes, if the device takes them and a mailbox is reachable.
    /// One that can't go yet waits for a session, a mailbox or its expiry.
    pub(crate) fn flush_receipts(&self, all: bool) {
        let now = now_ms();
        for d in self.shared.receipts(|o| o.due(now, all)) {
            if !self.receipt_target_ok(&d) {
                self.shared.receipts(|o| o.forget(&d));
                continue;
            }
            if self.connected(&d) {
                // Queued as its session came up, before its `Hello`: it
                // goes now (what the device can't take is dropped). Until
                // we have heard from it at all, it waits.
                if lock(&self.shared.peer_features).contains_key(&d)
                    && let Some(p) = self.shared.receipts(|o| o.take(&d))
                {
                    self.queue_receipt(&d, &p.delivered, &p.read);
                }
                continue;
            }
            // Sealing spends one of our prekeys for it: only when a mailbox
            // can take the result. With onion routing on, only one an onion
            // circuit reaches: a receipt goes without the user doing
            // anything, so it must not tell a mailbox whom we write to.
            if !self
                .known_features(&d)
                .is_some_and(|f| f & FEATURE_RECEIPTS != 0)
                || !self.can_send_offline(&d)
            {
                continue;
            }
            let (mailboxes, hidden) = self.deposit_targets(&d);
            if mailboxes.is_empty() || (self.prefer_onion() && hidden.is_empty()) {
                continue;
            }
            let Some(mut p) = self.shared.receipts(|o| o.take(&d)) else {
                continue;
            };
            if !self.send_read_receipts() {
                p.read.clear();
            }
            // Each one deposited is the placement's: if every mailbox
            // declines it (one that doesn't hold for `d`, or whose queue
            // for it is full), it waits, tracked, for a session with `d`.
            let (mut sent, mut left) = (false, Pending::default());
            for m in p.messages(FEATURE_RECEIPTS, &[]) {
                if self.send_offline_hidden(&d, &m, Tag::NONE).is_ok() {
                    sent = true;
                } else if let AppMessage::Receipt(b) = &m
                    && let Ok(r) = Receipt::decode(b)
                {
                    left.extend(&r.delivered, &r.read);
                }
            }
            left.since_ms = p.since_ms;
            let delay = self.receipt_delay();
            self.shared.receipts(|o| {
                if sent {
                    o.sealed(d, now);
                }
                if !left.is_empty() {
                    o.restore(d, left, now.saturating_add(delay));
                }
            });
        }
    }

    /// Starts looking for due receipts every [`TICK`].
    pub(crate) fn start_receipt_flush(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            let mut tick = tokio::time::interval(TICK);
            loop {
                tokio::select! {
                    _ = tick.tick() => node.flush_receipts(false),
                    () = &mut closed => break,
                }
            }
        });
    }

    /// Handles a `Receipt` from `from`, live or sealed: our outgoing
    /// messages it names in our conversation with its account were
    /// delivered (read ones too), and the read ones displayed.
    pub(crate) fn on_receipt(&self, from: &PublicIdentity, payload: &[u8]) {
        if self.is_own_device(from) {
            return;
        }
        let Ok(r) = Receipt::decode(payload) else {
            return;
        };
        let conv = self.conversation_for(from);
        let ids: Vec<u64> = r.delivered.iter().chain(&r.read).copied().collect();
        let changed = self
            .shared
            .home
            .mark_delivered_ids(self.identity_ref(), conv, &ids, now_ms())
            .unwrap_or_default();
        self.push_ticks(conv, &changed);
        for local_id in changed {
            self.emit(Event::Delivered {
                peer: *from,
                local_id,
                group: None,
                relay_for: None,
            });
        }
        if !r.read.is_empty() {
            self.on_read(from, &[], &r.read);
        }
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|[b]| b)
    }
    fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_be_bytes)
    }
    fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_be_bytes)
    }
    /// A device key: `None` if the bytes ran out, `Some(None)` if they
    /// aren't a valid key.
    fn device(&mut self) -> Option<Option<PublicIdentity>> {
        self.take::<32>()
            .map(|b| PublicIdentity::from_bytes(&b).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use threnody_core::Identity;

    fn dev() -> PublicIdentity {
        Identity::generate().public()
    }

    #[test]
    fn outbox_dedups_and_read_implies_delivered() {
        let d = dev();
        let mut o = Outbox::new(0);
        o.add(d, &[1, 1, 2], &[], 10, 5);
        o.add(d, &[2], &[], 11, 5);
        assert_eq!(o.len(), 2);
        // Read for 1: it goes as read only, still one message.
        o.add(d, &[], &[1], 12, 5);
        assert_eq!(o.len(), 2);
        let p = o.take(&d).unwrap();
        let sent = p.messages(FEATURE_RECEIPTS, &[]);
        assert_eq!(
            sent,
            [AppMessage::Receipt(
                Receipt {
                    delivered: vec![2],
                    read: vec![1],
                }
                .encode()
                .unwrap()
            )]
        );
        assert_eq!(o.len(), 0);
    }

    #[test]
    fn batch_due_time_is_set_once() {
        let d = dev();
        let mut o = Outbox::new(0);
        o.add(d, &[1], &[], 100, 50);
        o.add(d, &[2], &[], 120, 1);
        assert!(o.due(149, false).is_empty());
        assert_eq!(o.due(150, false), [d]);
        let mut o2 = Outbox::new(0);
        o2.add(d, &[1], &[], 100, 50_000);
        assert_eq!(o2.due(101, true), [d]);
    }

    #[test]
    fn sealed_gap_holds_back_the_next_batch() {
        let d = dev();
        let mut o = Outbox::new(0);
        let t = 1_000_000;
        o.sealed(d, t);
        o.add(d, &[1], &[], t, 60_000);
        assert!(o.due(t + 60_000, false).is_empty());
        assert!(o.due(t + MIN_SEALED_GAP_MS - 1, false).is_empty());
        assert_eq!(o.due(t + MIN_SEALED_GAP_MS, false), [d]);
    }

    #[test]
    fn outbox_survives_a_restart() {
        let (d, e) = (dev(), dev());
        let mut o = Outbox::new(77);
        o.add(d, &[1, 2], &[3], 100, 10);
        o.add(e, &[], &[4], 200, 20);
        o.sealed(e, 150);
        o.set_reads(false);
        o.set_reads(true);
        o.add(e, &[], &[5], 210, 0);
        let b = o.encode();
        let mut back = Outbox::load(&b).unwrap();
        o.dirty = false;
        assert_eq!(back, o);
        assert_eq!(back.since_ms, 77);
        assert_eq!(back.due(130, false), [d], "times kept, gap kept for e");

        // Reads off is kept too.
        let mut off = Outbox::new(5);
        off.set_reads(false);
        assert!(!Outbox::load(&off.encode()).unwrap().reads_on);

        // A damaged tail: what is intact loads.
        let mut one = Outbox::new(1);
        one.add(d, &[9], &[], 3, 4);
        let full = one.encode();
        let mut both = full.clone();
        both.extend_from_slice(&e.as_bytes()[..20]);
        let cut = Outbox::load(&both).unwrap();
        assert_eq!(cut.len(), 1);
        assert_eq!(Outbox::load(&full[..full.len() - 3]).unwrap().len(), 0);

        // Nothing, or another version: start afresh.
        assert!(Outbox::load(&[]).is_none());
        let mut other = full.clone();
        other[0] = VERSION + 1;
        assert!(Outbox::load(&other).is_none());
        let fresh = Outbox::new(9);
        assert!(fresh.reads_on && fresh.dirty && fresh.since_ms == 9);
    }

    #[test]
    fn outbox_is_bounded_and_expires() {
        let d = dev();
        let mut o = Outbox::new(0);
        for i in 1..=600 {
            o.add(d, &[i], &[i + 10_000], 1, 1);
        }
        let p = o.take(&d).unwrap();
        assert_eq!(p.delivered.len(), MAX_PENDING);
        assert_eq!(p.read.len(), MAX_PENDING);
        assert_eq!(p.delivered.first(), Some(&89), "the oldest went");
        assert_eq!(p.delivered.last(), Some(&600));

        o.add(d, &[1], &[], 1_000, 0);
        assert_eq!(o.due(1_000 + MAX_AGE_MS - 1, false), [d]);
        assert!(o.due(1_000 + MAX_AGE_MS, false).is_empty());
        assert_eq!(o.len(), 0, "dropped");
        assert!(o.dirty);
    }

    #[test]
    fn turning_reads_off_drops_pending_reads() {
        let (d, e) = (dev(), dev());
        let mut o = Outbox::new(0);
        o.add(d, &[1], &[2], 0, 0);
        o.add(e, &[], &[3], 0, 0);
        o.dirty = false;
        o.set_reads(false);
        assert!(o.dirty && !o.reads_on);
        assert_eq!(o.len(), 1);
        assert!(o.take(&e).is_none(), "an empty batch goes");
        assert_eq!(o.take(&d).unwrap().read, Vec::<u64>::new());
        // While off, reads aren't taken in at all.
        o.add(d, &[], &[4], 0, 0);
        assert_eq!(o.len(), 0);
    }

    #[test]
    fn legacy_peer_gets_read_and_no_delivered() {
        let p = Pending {
            delivered: vec![1, 2],
            read: (10..1110).collect(),
            ..Pending::default()
        };
        let conv = vec![0; 33];
        let legacy = p.messages(FEATURE_READ, &conv);
        assert_eq!(legacy.len(), 3);
        let mut ids: Vec<u64> = Vec::new();
        for m in &legacy {
            let AppMessage::Read {
                conversation,
                ids: chunk,
            } = m
            else {
                panic!("{m:?}");
            };
            assert_eq!(conversation, &conv);
            assert!(chunk.len() <= MAX_IDS);
            ids.extend(chunk);
        }
        assert_eq!(ids, p.read);
        assert!(p.messages(0, &conv).is_empty());

        let new = p.messages(FEATURE_RECEIPTS | FEATURE_READ, &conv);
        assert_eq!(new.len(), 3);
        let (mut delivered, mut read) = (Vec::new(), Vec::new());
        for m in &new {
            let AppMessage::Receipt(b) = m else {
                panic!("{m:?}");
            };
            let r = Receipt::decode(b).unwrap();
            assert!(r.read.len() <= MAX_IDS && r.delivered.len() <= MAX_IDS);
            delivered.extend(r.delivered);
            read.extend(r.read);
        }
        assert_eq!((delivered, read), (vec![1, 2], p.read.clone()));

        let live = read_messages(FEATURE_READ, &conv, &p.read);
        assert_eq!(live, legacy);
        assert_eq!(read_messages(FEATURE_RECEIPTS, &conv, &p.read).len(), 3);
        assert!(read_messages(0, &conv, &p.read).is_empty());
    }
}
