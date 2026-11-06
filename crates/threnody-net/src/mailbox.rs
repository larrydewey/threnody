//! Store-and-forward mailboxes for sealed messages
//! (`docs/appendix-h-offline.md`).
//!
//! ```text
//! MailboxMsg = { 0: op, ? 1: to (32), ? 2: sealed, ? 3: status, ? 4: ref }
//! op: 1 deposit, 2 deliver, 3 receipt (status 0 declined, 1 held, 2 delivered now)
//! ```
//!
//! A deposit may carry a `ref`, which its receipt repeats.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::delivery::Tag;
use crate::error::{NetError, Result};
use crate::history::Recorded;
use crate::node::{Event, FallbackPath, Node, Shared, lock};

/// Per-recipient limits for held messages.
pub const MAX_HELD_MESSAGES: usize = 100;
pub const MAX_HELD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HOLD_MS: u64 = 14 * 24 * 60 * 60 * 1000;
/// How long a direct deposit waits for its receipt before the sender
/// counts it declined.
pub const RECEIPT_WAIT: Duration = Duration::from_secs(120);
const STATE_VERSION: u64 = 1;

/// What a mailbox did with a deposit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositStatus {
    Declined = 0,
    Held = 1,
    Delivered = 2,
}

impl DepositStatus {
    pub(crate) fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Declined,
            1 => Self::Held,
            2 => Self::Delivered,
            _ => return None,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MailboxMsg {
    Deposit {
        to: [u8; 32],
        sealed: Vec<u8>,
        reference: Option<u64>,
    },
    Deliver {
        sealed: Vec<u8>,
    },
    Receipt {
        to: [u8; 32],
        status: DepositStatus,
        reference: Option<u64>,
    },
}

impl MailboxMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let size = match self {
            Self::Deposit { sealed, .. } | Self::Deliver { sealed } => sealed.len(),
            Self::Receipt { .. } => 0,
        } + 64;
        cbor::to_vec(size, |e| {
            match self {
                Self::Deposit {
                    to,
                    sealed,
                    reference,
                } => {
                    e.map_len(3 + usize::from(reference.is_some()))?
                        .u8(0)?
                        .u8(1)?;
                    e.u8(1)?.bytes(to)?;
                    e.u8(2)?.bytes(sealed)?;
                    if let Some(r) = reference {
                        e.u8(4)?.u64(*r)?;
                    }
                }
                Self::Deliver { sealed } => {
                    e.map_len(2)?.u8(0)?.u8(2)?;
                    e.u8(2)?.bytes(sealed)?;
                }
                Self::Receipt {
                    to,
                    status,
                    reference,
                } => {
                    e.map_len(3 + usize::from(reference.is_some()))?
                        .u8(0)?
                        .u8(3)?;
                    e.u8(1)?.bytes(to)?;
                    e.u8(3)?.u8(*status as u8)?;
                    if let Some(r) = reference {
                        e.u8(4)?.u64(*r)?;
                    }
                }
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut to, mut sealed, mut status, mut reference) =
            (None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => to = Some(fixed_bytes::<32>(d)?),
                2 => sealed = Some(d.bytes()?.to_vec()),
                3 => status = Some(d.u8()?),
                4 => reference = Some(d.u64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(op, "mailbox op")? {
            1 => Self::Deposit {
                to: required(to, "recipient")?,
                sealed: required(sealed, "sealed message")?,
                reference,
            },
            2 => Self::Deliver {
                sealed: required(sealed, "sealed message")?,
            },
            3 => Self::Receipt {
                to: required(to, "recipient")?,
                status: match required(status, "deposit status")? {
                    0 => DepositStatus::Declined,
                    1 => DepositStatus::Held,
                    2 => DepositStatus::Delivered,
                    _ => return Err(threnody_core::Error::Malformed("deposit status")),
                },
                reference,
            },
            other => return Err(threnody_core::Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// Sealed messages held for absent contacts.
#[derive(Default)]
pub(crate) struct MailboxStore {
    held: HashMap<[u8; 32], Vec<Held>>,
}

struct Held {
    at: u64,
    sealed: Vec<u8>,
    /// Out to the recipient in this run, waiting for its acknowledgement
    /// (not saved: after a restart it goes again).
    sent: bool,
}

/// Whether a delivery of `sealed` is too large for the acknowledgement
/// state to keep across a restart (see [`crate::delivery::MAX_PERSISTED`]),
/// so the store keeps it until the recipient acknowledges it. With room to
/// spare for the `Deliver` and `Tracked` framing.
fn kept_until_acked(sealed: &[u8]) -> bool {
    sealed.len() + 256 > crate::delivery::MAX_PERSISTED
}

impl MailboxStore {
    fn hold(&mut self, to: [u8; 32], sealed: Vec<u8>, now: u64, sent: bool) -> bool {
        let q = self.held.entry(to).or_default();
        q.retain(|h| now.saturating_sub(h.at) < MAX_HOLD_MS);
        let bytes: usize = q.iter().map(|h| h.sealed.len()).sum();
        if q.len() >= MAX_HELD_MESSAGES
            || bytes + sealed.len() > MAX_HELD_BYTES
            || q.iter().any(|h| h.sealed == sealed)
        {
            return false;
        }
        q.push(Held {
            at: now,
            sealed,
            sent,
        });
        true
    }

    /// What to deliver to `to` now. Small messages leave the store (the
    /// acknowledgement state keeps them until `to` has them); large ones
    /// stay until `to` acknowledges them ([`MailboxStore::delivered`]),
    /// going out once per run. Also says whether the store changed.
    pub(crate) fn take(&mut self, to: &PublicIdentity, now: u64) -> (Vec<Vec<u8>>, bool) {
        let Some(q) = self.held.get_mut(to.as_bytes()) else {
            return (Vec::new(), false);
        };
        let before = q.len();
        let mut out = Vec::new();
        q.retain_mut(|h| {
            if now.saturating_sub(h.at) >= MAX_HOLD_MS {
                return false;
            }
            if !kept_until_acked(&h.sealed) {
                out.push(std::mem::take(&mut h.sealed));
                return false;
            }
            if !h.sent {
                h.sent = true;
                out.push(h.sealed.clone());
            }
            true
        });
        let changed = q.len() != before;
        if q.is_empty() {
            self.held.remove(to.as_bytes());
        }
        (out, changed)
    }

    /// `to` acknowledged `sealed`: our copy goes. True if we had one.
    pub(crate) fn delivered(&mut self, to: &PublicIdentity, sealed: &[u8]) -> bool {
        let Some(q) = self.held.get_mut(to.as_bytes()) else {
            return false;
        };
        let before = q.len();
        q.retain(|h| h.sealed != sealed);
        let gone = q.len() != before;
        if q.is_empty() {
            self.held.remove(to.as_bytes());
        }
        gone
    }

    /// Drops what has been held too long, for every recipient. True if
    /// anything went.
    pub(crate) fn prune(&mut self, now: u64) -> bool {
        let before = self.held_for();
        self.held.retain(|_, q| {
            q.retain(|h| now.saturating_sub(h.at) < MAX_HOLD_MS);
            !q.is_empty()
        });
        self.held_for() != before
    }

    /// Drops everything held for `to`. True if there was something.
    pub(crate) fn forget(&mut self, to: &PublicIdentity) -> bool {
        self.held.remove(to.as_bytes()).is_some()
    }

    pub(crate) fn held_for(&self) -> usize {
        self.held.values().map(Vec::len).sum()
    }

    pub(crate) fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let size: usize = self
            .held
            .values()
            .flatten()
            .map(|h| h.sealed.len() + 48)
            .sum::<usize>()
            + 64;
        cbor::to_vec(size, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(STATE_VERSION)?;
            e.u8(1)?.array_len(self.held_for())?;
            for (to, q) in &self.held {
                for h in q {
                    e.array_len(3)?.bytes(to)?.u64(h.at)?.bytes(&h.sealed)?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let mut me = Self::default();
        let mut ver = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 3 {
                            return Err(threnody_core::Error::Malformed("mailbox entry"));
                        }
                        let to = fixed_bytes::<32>(d)?;
                        let t = d.u64()?;
                        me.held.entry(to).or_default().push(Held {
                            at: t,
                            sealed: d.bytes()?.to_vec(),
                            sent: false,
                        });
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(STATE_VERSION) {
            return Err(threnody_core::Error::Malformed("mailbox version"));
        }
        Ok(me)
    }
}

/// Our sealed messages waiting to hear whether any mailbox took them: one
/// that every mailbox declines (or none answers for) waits for a session
/// with its recipient instead, as if it had never been sealed. Saved, so
/// one still waiting at a restart does so too.
#[derive(Default)]
pub(crate) struct Placements {
    open: HashMap<u64, Placement>,
    /// Direct deposits waiting for a receipt, per mailbox and recipient,
    /// oldest first (a mailbox that doesn't repeat `ref` answers in order).
    /// Every direct deposit is here, placed or not, so that order holds.
    waiting: HashMap<(PublicIdentity, [u8; 32]), VecDeque<Sent>>,
    /// `open` changed since last saved.
    dirty: bool,
    /// Overrides [`RECEIPT_WAIT`].
    receipt_wait: Option<Duration>,
}

/// A direct deposit's `ref`, and when it went.
type Sent = (u64, Instant);

struct Placement {
    dest: PublicIdentity,
    msg: AppMessage,
    tag: Tag,
    /// Mailboxes asked, and how many of them declined.
    asked: usize,
    declined: usize,
}

/// A fresh `ref`: random, so a mailbox learns nothing from the gaps
/// between ours.
fn fresh_ref() -> u64 {
    u64::from_le_bytes(threnody_core::crypto::random_bytes())
}

impl Placements {
    fn open(&mut self, dest: PublicIdentity, msg: AppMessage, tag: Tag, asked: usize) -> u64 {
        let r = fresh_ref();
        self.open.insert(
            r,
            Placement {
                dest,
                msg,
                tag,
                asked,
                declined: 0,
            },
        );
        self.dirty = true;
        r
    }

    /// Takes `r` out entirely.
    fn close(&mut self, r: u64) -> Option<Placement> {
        self.waiting.retain(|_, q| {
            q.retain(|(x, _)| *x != r);
            !q.is_empty()
        });
        let p = self.open.remove(&r);
        self.dirty |= p.is_some();
        p
    }

    /// A deposit with `ref` `r` went to `mailbox` for `to`. Unplaced ones
    /// unanswered for [`RECEIPT_WAIT`] are forgotten; placed ones are
    /// declined then ([`Node::await_receipt`]).
    fn wait(&mut self, mailbox: PublicIdentity, to: [u8; 32], r: u64, now: Instant) {
        let open = &self.open;
        let q = self.waiting.entry((mailbox, to)).or_default();
        q.retain(|(x, at)| open.contains_key(x) || now.duration_since(*at) < RECEIPT_WAIT);
        q.push_back((r, now));
    }

    /// `mailbox` answered for `to`: which deposit that was.
    fn receipt(&mut self, mailbox: PublicIdentity, to: [u8; 32], r: Option<u64>) -> Option<u64> {
        let q = self.waiting.get_mut(&(mailbox, to))?;
        let r = match r {
            Some(r) => q.remove(q.iter().position(|(x, _)| *x == r)?),
            None => q.pop_front(),
        };
        if q.is_empty() {
            self.waiting.remove(&(mailbox, to));
        }
        r.map(|(r, _)| r)
    }

    /// One more mailbox answered `status` for `r`. Returns the message to
    /// keep for a session once none of them took it.
    fn answer(&mut self, r: u64, status: DepositStatus) -> Option<Placement> {
        let p = self.open.get_mut(&r)?;
        if status != DepositStatus::Declined {
            self.close(r);
            return None;
        }
        p.declined += 1;
        if p.declined < p.asked {
            return None;
        }
        self.close(r)
    }

    /// One mailbox was never asked after all.
    fn unask(&mut self, r: u64) -> Option<Placement> {
        let p = self.open.get_mut(&r)?;
        p.asked -= 1;
        match p.asked {
            // Nobody asked: the caller keeps the message itself.
            0 => {
                self.close(r);
                None
            }
            n if p.declined >= n => self.close(r),
            _ => None,
        }
    }

    /// `{ 0: version, 1: [* [ref, dest (32), tag, AppMessage]] }`, tag as
    /// [`Tag::encode`] writes it.
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let msgs = self
            .open
            .iter()
            .map(|(r, p)| Ok((*r, p, p.msg.encode()?)))
            .collect::<threnody_core::Result<Vec<_>>>()?;
        let size = msgs.iter().map(|(_, _, m)| m.len() + 128).sum::<usize>() + 16;
        cbor::to_vec(size, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(STATE_VERSION)?;
            e.u8(1)?.array_len(msgs.len())?;
            for (r, p, m) in &msgs {
                let mut tag = Vec::with_capacity(Tag::LEN);
                p.tag.encode(&mut tag);
                e.array_len(4)?
                    .u64(*r)?
                    .bytes(p.dest.as_bytes())?
                    .bytes(&tag)?
                    .bytes(m)?;
            }
            Ok(())
        })
    }

    /// What [`Placements::encode`] saved: (ref, dest, message, tag).
    fn decode(b: &[u8]) -> threnody_core::Result<Vec<(u64, PublicIdentity, AppMessage, Tag)>> {
        let mut dec = Decoder::new(b);
        let (mut ver, mut out) = (None, Vec::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 4 {
                            return Err(threnody_core::Error::Malformed("placement"));
                        }
                        let r = d.u64()?;
                        let dest = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?)?;
                        let tag = d.bytes()?;
                        if tag.len() != Tag::LEN {
                            return Err(threnody_core::Error::Malformed("placement tag"));
                        }
                        let tag = Tag::decode(tag);
                        out.push((r, dest, AppMessage::decode(d.bytes()?)?, tag));
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(STATE_VERSION) {
            return Err(threnody_core::Error::Malformed("placements version"));
        }
        Ok(out)
    }
}

/// The `ref` of `m`, if it is a deposit for `to`.
fn deposit_ref(m: &AppMessage, to: &[u8; 32]) -> Option<u64> {
    let AppMessage::Mailbox(b) = m else {
        return None;
    };
    match MailboxMsg::decode(b) {
        Ok(MailboxMsg::Deposit {
            to: t, reference, ..
        }) if t == *to => reference,
        _ => None,
    }
}

impl Node {
    fn mailbox_send(&self, to: &PublicIdentity, m: &MailboxMsg) -> bool {
        m.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Mailbox(b)).is_ok())
    }

    /// True when we hold a usable prekey bundle for `peer`, and it is
    /// still a device of its account.
    pub fn can_send_offline(&self, peer: &PublicIdentity) -> bool {
        lock(&self.shared.bundles).has(peer, now_ms()) && !self.is_revoked(peer)
    }

    /// Tests only: how many of `peer`'s one-time prekeys we have left.
    #[doc(hidden)]
    pub fn opks_for(&self, peer: &PublicIdentity) -> usize {
        lock(&self.shared.bundles).opks_left(peer)
    }

    /// The neighbours that could hold a sealed message for `dest`: live
    /// direct sessions with mutually approved contacts other than `dest`.
    pub(crate) fn mailbox_neighbours(&self, dest: &PublicIdentity) -> Vec<PublicIdentity> {
        self.sessions()
            .into_iter()
            .filter(|s| s.via.is_none() && s.peer != *dest && self.shared.mutual(&s.peer))
            .map(|s| s.peer)
            .collect()
    }

    /// Seals `msg` for `dest` and offers it to every live, mutually
    /// approved neighbour. Returns how many were asked; each answers with
    /// [`Event::DepositReceipt`].
    pub fn send_offline(&self, dest: &PublicIdentity, msg: &AppMessage) -> Result<usize> {
        self.deposit(dest, msg, None)
    }

    /// [`Node::send_offline`] for a message that must not be lost: if
    /// every mailbox asked declines it, or doesn't answer within
    /// [`RECEIPT_WAIT`], it waits for a session with `dest` as
    /// [`Node::send_tagged`] would have sent it, under `tag`. An error
    /// means no mailbox was asked, and the message is the caller's to keep.
    pub fn send_offline_or_hold(
        &self,
        dest: &PublicIdentity,
        msg: &AppMessage,
        tag: Tag,
    ) -> Result<usize> {
        self.deposit(dest, msg, Some(tag))
    }

    fn deposit(&self, dest: &PublicIdentity, msg: &AppMessage, keep: Option<Tag>) -> Result<usize> {
        if self.is_revoked(dest) {
            return Err(NetError::NotAllowed(
                "that device was removed from its account".into(),
            ));
        }
        let sealed = {
            let mut book = lock(&self.shared.bundles);
            let sealed = book.seal(self.identity_ref(), dest, &msg.encode()?, now_ms())?;
            self.shared.persist_bundles(&book);
            sealed
        };
        let mailboxes = self.mailbox_neighbours(dest);
        // Hide who deposits: through an onion circuit, wherever one reaches
        // the mailbox. Only with no circuit anywhere do we deposit directly.
        let handle = tokio::runtime::Handle::try_current().ok();
        let hidden: Vec<PublicIdentity> = match &handle {
            Some(_) if self.prefer_onion() => mailboxes
                .iter()
                .filter(|m| {
                    !self
                        .onion_paths(m.fingerprint(), &[dest.fingerprint()], 2)
                        .is_empty()
                })
                .copied()
                .collect(),
            _ => Vec::new(),
        };
        let asked = if hidden.is_empty() {
            mailboxes.len()
        } else {
            hidden.len()
        };
        let placement = match keep {
            Some(tag) if asked > 0 => {
                let r = lock(&self.shared.placements).open(*dest, msg.clone(), tag, asked);
                self.save_placements();
                Some(r)
            }
            _ => None,
        };
        let sealed = std::sync::Arc::new(sealed);
        if let (Some(handle), false) = (handle, hidden.is_empty()) {
            for m in &hidden {
                let (node, m, dest, sealed) = (self.clone(), *m, *dest, sealed.clone());
                handle.spawn(async move {
                    match node.onion_deposit(&m, &dest, &sealed).await {
                        Ok(status) => {
                            node.emit(Event::DepositReceipt {
                                mailbox: m,
                                to: dest,
                                status,
                                anonymous: true,
                            });
                            if let Some(r) = placement {
                                node.placement_answer(r, status);
                            }
                        }
                        // Every circuit failed: deliver it rather than lose
                        // it, and say so.
                        Err(_) => {
                            if node.deposit_direct(&m, &dest, &sealed, placement) {
                                node.fell_back(dest, FallbackPath::Deposit { mailbox: m });
                            } else if let Some(r) = placement {
                                node.placement_answer(r, DepositStatus::Declined);
                            }
                        }
                    }
                });
            }
            return Ok(hidden.len());
        }
        // No circuit reaches any mailbox (with onion routing on, the app
        // hears so: each mailbox learns who writes to `dest`).
        let mut n = 0;
        for m in &mailboxes {
            if self.deposit_direct(m, dest, &sealed, placement) {
                self.fell_back(*dest, FallbackPath::Deposit { mailbox: *m });
                n += 1;
            } else if let Some(r) = placement {
                let p = lock(&self.shared.placements).unask(r);
                self.keep_for_session(p);
                self.save_placements();
            }
        }
        if n == 0 {
            return Err(NetError::NoRoute(format!(
                "{} (no mailbox neighbour online)",
                dest.fingerprint()
            )));
        }
        Ok(n)
    }

    /// Deposits `sealed` for `dest` with `mailbox` directly. With a
    /// placement `r`, waits for the receipt (see [`Node::await_receipt`]).
    fn deposit_direct(
        &self,
        mailbox: &PublicIdentity,
        dest: &PublicIdentity,
        sealed: &[u8],
        placement: Option<u64>,
    ) -> bool {
        let key = (*mailbox, *dest.as_bytes());
        // Every direct deposit carries a `ref`, so that no receipt can be
        // taken for another deposit's.
        let r = placement.unwrap_or_else(fresh_ref);
        lock(&self.shared.placements).wait(key.0, key.1, r, Instant::now());
        let sent = self.mailbox_send(
            mailbox,
            &MailboxMsg::Deposit {
                to: key.1,
                sealed: sealed.to_vec(),
                reference: Some(r),
            },
        );
        if !sent {
            lock(&self.shared.placements).receipt(key.0, key.1, Some(r));
            return false;
        }
        if placement.is_some() {
            self.await_receipt(*mailbox, *dest, r);
        }
        true
    }

    /// Counts placement `r`'s deposit with `mailbox` declined if its
    /// receipt doesn't come within [`RECEIPT_WAIT`], and withdraws it, so
    /// that the mailbox doesn't get it later as well, when the message may
    /// already have gone to `dest` another way.
    fn await_receipt(&self, mailbox: PublicIdentity, dest: PublicIdentity, r: u64) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let wait = lock(&self.shared.placements)
            .receipt_wait
            .unwrap_or(RECEIPT_WAIT);
        let node = self.clone();
        handle.spawn(async move {
            // Shut down first: the placement stays saved for the next start.
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = node.closed() => return,
            }
            let to = *dest.as_bytes();
            let late = lock(&node.shared.placements).receipt(mailbox, to, Some(r));
            if late.is_none() || node.is_closed() {
                return;
            }
            node.shared.delivery(|d| {
                d.drop_where(|p, m| *p == mailbox && deposit_ref(m, &to) == Some(r));
            });
            node.placement_answer(r, DepositStatus::Declined);
        });
    }

    fn placement_answer(&self, r: u64, status: DepositStatus) {
        // After a shutdown, it stays saved for the next start.
        if self.is_closed() {
            return;
        }
        let p = lock(&self.shared.placements).answer(r, status);
        self.keep_for_session(p);
        self.save_placements();
    }

    /// No mailbox took it: send it now if a session came up meanwhile,
    /// else hold it for the next one.
    fn keep_for_session(&self, p: Option<Placement>) {
        let Some(p) = p else { return };
        if self.send_tagged(&p.dest, p.msg.clone(), p.tag).is_err() {
            self.hold(&p.dest, p.msg, p.tag);
        }
    }

    /// Saves the open placements if they changed. After the message is
    /// kept elsewhere, so a crash in between can't lose it.
    fn save_placements(&self) {
        let mut ps = lock(&self.shared.placements);
        if std::mem::take(&mut ps.dirty) {
            self.shared.save_state("placements", ps.encode());
        }
    }

    /// Placements a restart left open: nothing waits for their receipts
    /// any more, so each message waits for a session with its recipient,
    /// and its deposits still queued for mailboxes are withdrawn. (One a
    /// mailbox already took then reaches the recipient twice; it keeps
    /// the first.)
    pub(crate) fn resume_placements(&self) {
        let Ok(Some(b)) = self
            .shared
            .home
            .load_state(self.identity_ref(), "placements")
        else {
            return;
        };
        let placed = Placements::decode(&b).unwrap_or_default();
        if !placed.is_empty() {
            self.shared.delivery(|d| {
                d.drop_where(|_, m| {
                    placed
                        .iter()
                        .any(|(r, dest, ..)| deposit_ref(m, dest.as_bytes()) == Some(*r))
                });
            });
        }
        for (_, dest, msg, tag) in placed {
            self.hold(&dest, msg, tag);
        }
        let ps = lock(&self.shared.placements);
        self.shared.save_state("placements", ps.encode());
    }

    /// How long a direct deposit from [`Node::send_offline_or_hold`] waits
    /// for its receipt ([`RECEIPT_WAIT`] unless set).
    pub fn set_receipt_wait(&self, wait: Duration) {
        lock(&self.shared.placements).receipt_wait = Some(wait);
    }

    /// Disappearing texts still waiting for their recipient once their
    /// timer has run out here: the plain copy goes, replaced by one sealed
    /// to the recipient, which we can't read back. It goes out as a
    /// mailbox delivery from us. With no prekeys to seal to, it is dropped.
    pub(crate) fn seal_expired_unacked(&self, now: u64) {
        for (peer, id, msg) in self.shared.delivery(|d| d.expired(now)) {
            let sealed = msg.encode().ok().and_then(|body| {
                let mut book = lock(&self.shared.bundles);
                let sealed = book.seal(self.identity_ref(), &peer, &body, now).ok()?;
                self.shared.persist_bundles(&book);
                MailboxMsg::Deliver { sealed }.encode().ok()
            });
            self.shared
                .delivery(|d| d.replace(&peer, id, sealed.map(AppMessage::Mailbox)));
        }
    }

    /// Messages held for `peer`, to deliver now that it is connected.
    /// Only to a mutually approved peer, as we only hold for one.
    pub(crate) fn mailbox_for(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        if !self.shared.mutual(peer) {
            return Vec::new();
        }
        let held = {
            let mut mb = lock(&self.shared.mailbox);
            let (held, changed) = mb.take(peer, now_ms());
            if changed {
                self.shared.persist_mailbox(&mb);
            }
            held
        };
        held.into_iter()
            .filter_map(|sealed| MailboxMsg::Deliver { sealed }.encode().ok())
            .map(AppMessage::Mailbox)
            .collect()
    }

    /// Prekeys to hand a mutually approved `peer`: a fresh bundle of ours,
    /// plus the shared bundles our sibling devices gave us (Appendix J).
    pub(crate) fn prekeys_for(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let mut out = Vec::new();
        {
            let mut store = lock(&self.shared.prekeys);
            let bundle = store.bundle_for(self.identity_ref(), peer, now_ms());
            self.shared.persist_prekeys(&store);
            out.extend(bundle.encode().ok().map(AppMessage::Prekeys));
        }
        let siblings: Vec<_> = lock(&self.shared.siblings)
            .iter()
            .filter(|(d, b)| *d != peer.as_bytes() && b.expiry_ms > now_ms())
            .filter_map(|(_, b)| b.encode().ok())
            .collect();
        out.extend(siblings.into_iter().map(AppMessage::Prekeys));
        out
    }

    /// Our shared bundle, for one of our own devices to pass on.
    pub(crate) fn shared_bundle(&self) -> Option<Vec<u8>> {
        let mut store = lock(&self.shared.prekeys);
        let b = store.shared_bundle(self.identity_ref(), now_ms());
        self.shared.persist_prekeys(&store);
        b.encode().ok()
    }

    /// A sibling device's shared bundle, received from that device itself.
    pub(crate) fn on_sibling_bundle(&self, from: PublicIdentity, bytes: &[u8]) {
        let Ok(b) = threnody_core::prekey::PrekeyBundle::decode(bytes) else {
            return;
        };
        if b.owner != from || !self.is_own_device(&from) || b.verify(&from, now_ms()).is_err() {
            return;
        }
        // Pass it on to connected contacts right away.
        if let Ok(fwd) = b.encode() {
            for s in self.sessions() {
                if !self.is_own_device(&s.peer)
                    && s.peer != self.identity()
                    && self.shared.mutual(&s.peer)
                {
                    let _ = self.send(&s.peer, AppMessage::Prekeys(fwd.clone()));
                }
            }
        }
        let mut sib = lock(&self.shared.siblings);
        sib.insert(*from.as_bytes(), b);
        let encoded: Vec<Vec<u8>> = sib.values().filter_map(|b| b.encode().ok()).collect();
        drop(sib);
        self.shared
            .save_state("siblings", encode_bundle_list(&encoded));
    }

    pub(crate) fn on_prekeys(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(bundle) = threnody_core::prekey::PrekeyBundle::decode(payload) else {
            return;
        };
        let owner = bundle.owner;
        let mut book = lock(&self.shared.bundles);
        let stored = if owner == from {
            book.insert(&from, bundle, now_ms()).is_ok()
        } else {
            // Forwarded by a sibling: `from` must be mutually approved and in
            // the same account as the owner, and the owner not revoked.
            let same_account = self
                .account_of(&from)
                .is_some_and(|a| a.state().has(&from) && a.state().has(&owner));
            same_account
                && self.shared.mutual(&from)
                && !self.is_revoked(&owner)
                && book
                    .offer_forwarded(&owner, bundle, now_ms())
                    .unwrap_or(false)
        };
        if stored {
            self.shared.persist_bundles(&book);
        }
    }

    pub(crate) fn on_mailbox(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = MailboxMsg::decode(payload) else {
            return;
        };
        match msg {
            MailboxMsg::Deposit {
                to,
                sealed,
                reference,
            } if to == *self.identity().as_bytes() => {
                self.open_sealed(from, &sealed);
                self.mailbox_send(
                    &from,
                    &MailboxMsg::Receipt {
                        to,
                        status: DepositStatus::Delivered,
                        reference,
                    },
                );
            }
            MailboxMsg::Deposit {
                to,
                sealed,
                reference,
            } => {
                let status = self.accept_deposit(&from, to, sealed);
                self.mailbox_send(
                    &from,
                    &MailboxMsg::Receipt {
                        to,
                        status,
                        reference,
                    },
                );
            }
            MailboxMsg::Deliver { sealed } => self.open_sealed(from, &sealed),
            MailboxMsg::Receipt {
                to,
                status,
                reference,
            } => {
                let placed = lock(&self.shared.placements).receipt(from, to, reference);
                if let Some(r) = placed {
                    self.placement_answer(r, status);
                }
                if let Ok(to) = PublicIdentity::from_bytes(&to) {
                    self.emit(Event::DepositReceipt {
                        mailbox: from,
                        to,
                        status,
                        anonymous: false,
                    });
                }
            }
        }
    }

    /// Holds or forwards a deposit, only between our own mutually approved
    /// contacts.
    fn accept_deposit(
        &self,
        from: &PublicIdentity,
        to: [u8; 32],
        sealed: Vec<u8>,
    ) -> DepositStatus {
        if !self.shared.mutual(from) {
            return DepositStatus::Declined;
        }
        self.store_deposit(to, sealed)
    }

    /// Holds or forwards a deposit for our mutually approved contact `to`.
    /// The depositor is either a mutual neighbour or anonymous, arriving
    /// over an onion circuit a mutual neighbour carried to us.
    pub(crate) fn store_deposit(&self, to: [u8; 32], sealed: Vec<u8>) -> DepositStatus {
        let Ok(dest) = PublicIdentity::from_bytes(&to) else {
            return DepositStatus::Declined;
        };
        if !self.shared.mutual(&dest) {
            return DepositStatus::Declined;
        }
        if self.sessions().iter().any(|s| s.peer == dest) {
            // A large one is kept until acknowledged, as below, in case
            // we restart before then.
            let kept = kept_until_acked(&sealed) && {
                let mut mb = lock(&self.shared.mailbox);
                let kept = mb.hold(to, sealed.clone(), now_ms(), true);
                if kept {
                    self.shared.persist_mailbox(&mb);
                }
                kept
            };
            let deliver = MailboxMsg::Deliver {
                sealed: sealed.clone(),
            };
            if self.mailbox_send(&dest, &deliver) {
                return DepositStatus::Delivered;
            }
            if kept {
                let mut mb = lock(&self.shared.mailbox);
                mb.delivered(&dest, &sealed);
                self.shared.persist_mailbox(&mb);
            }
        }
        let held = {
            let mut mb = lock(&self.shared.mailbox);
            let held = mb.hold(to, sealed, now_ms(), false);
            if held {
                self.shared.persist_mailbox(&mb);
            }
            held
        };
        if held {
            // Wake it, if it sleeps and gave us where (Appendix R).
            self.push_wake(&dest);
            DepositStatus::Held
        } else {
            DepositStatus::Declined
        }
    }

    fn open_sealed(&self, via: PublicIdentity, sealed: &[u8]) {
        let me = self.identity();
        // Only contacts we'd take a session from: not a blocked one, nor
        // a device its account removed (it may still hold our prekeys).
        let senders: Vec<PublicIdentity> = self
            .contacts()
            .iter()
            .filter(|c| !c.blocked && !self.is_revoked(&c.key))
            .map(|c| c.key)
            .collect();
        let opened = {
            let mut store = lock(&self.shared.prekeys);
            let r = store.open(&me, sealed, now_ms(), |k| {
                senders.iter().find(|c| c.as_bytes() == k).copied()
            });
            if r.is_ok() {
                self.shared.persist_prekeys(&store);
            }
            r
        };
        // Failures (duplicates from several mailboxes, strangers, blocked
        // or removed senders, forgeries) are dropped silently, as for any
        // unauthenticated input.
        if let Ok((from, body)) = opened
            && let Ok(msg) = AppMessage::decode(&body)
        {
            // Changes to a conversation are sealed too (see `send_change`),
            // and also held for our next session: a copy that came that way
            // first is dropped, so it can't undo a newer change.
            let contact = self.is_accepted(&from);
            if self
                .shared
                .delivery(|d| d.seen_change(&from, &msg, contact))
            {
                return;
            }
            match &msg {
                AppMessage::Delete { conversation, ids } => {
                    return self.on_delete(&from, conversation, ids);
                }
                AppMessage::Edit {
                    conversation,
                    id,
                    body,
                } => return self.on_edit(&from, conversation, *id, body),
                AppMessage::React {
                    conversation,
                    id,
                    emoji,
                    add,
                } => return self.on_react(&from, conversation, *id, emoji, *add),
                AppMessage::Pin {
                    conversation,
                    id,
                    pin,
                } => return self.on_pin(&from, conversation, *id, *pin),
                AppMessage::List(bytes) => return self.on_list_message(from, bytes),
                AppMessage::Receipt(payload) => return self.on_receipt(&from, payload),
                _ => {}
            }
            // A text already here (from its sender too) doesn't show
            // again. One that couldn't be stored still shows: a sealed
            // copy can't be asked for again.
            if self.record_incoming(&from, &msg, true) == Recorded::Duplicate {
                return;
            }
            // It came through a mailbox, so its sender (very likely
            // offline) learns it arrived from a receipt that goes the
            // same way, or over the next session.
            if let AppMessage::Text { id, .. } = &msg
                && *id != 0
                && self.is_accepted(&from)
            {
                self.queue_receipt(&from, &[*id], &[]);
            }
            if matches!(msg, AppMessage::Group(_)) || self.is_accepted(&from) {
                self.emit(Event::OfflineMessage { from, via, msg });
            } else {
                self.emit(Event::MessageRequest { peer: from, msg });
            }
        }
    }

    /// Number of sealed messages we are holding for others.
    pub fn held_messages(&self) -> usize {
        lock(&self.shared.mailbox).held_for()
    }

    /// Drops held messages past [`MAX_HOLD_MS`], whoever they wait for.
    pub(crate) fn sweep_mailbox(&self, now: u64) {
        let mut mb = lock(&self.shared.mailbox);
        if mb.prune(now) {
            self.shared.persist_mailbox(&mb);
        }
    }

    /// Drops what we hold for `peer`, a contact that is gone.
    pub(crate) fn forget_held(&self, peer: &PublicIdentity) {
        let mut mb = lock(&self.shared.mailbox);
        if mb.forget(peer) {
            self.shared.persist_mailbox(&mb);
        }
    }
}

impl Shared {
    /// `peer` has (or, not acknowledging, was sent) our delivery
    /// `payload`: if we kept a copy until then, it goes.
    pub(crate) fn mail_delivered(&self, peer: &PublicIdentity, payload: &[u8]) {
        let Ok(MailboxMsg::Deliver { sealed }) = MailboxMsg::decode(payload) else {
            return;
        };
        let mut mb = lock(&self.mailbox);
        if mb.delivered(peer, &sealed) {
            self.persist_mailbox(&mb);
        }
    }
}

fn encode_bundle_list(parts: &[Vec<u8>]) -> threnody_core::Result<Vec<u8>> {
    cbor::to_vec(parts.iter().map(Vec::len).sum::<usize>() + 16, |e| {
        e.array_len(parts.len())?;
        for p in parts {
            e.bytes(p)?;
        }
        Ok(())
    })
}

/// Decodes the persisted sibling bundles.
pub(crate) fn decode_bundle_list(
    b: &[u8],
) -> threnody_core::Result<HashMap<[u8; 32], threnody_core::prekey::PrekeyBundle>> {
    let mut dec = Decoder::new(b);
    let mut out = HashMap::new();
    for _ in 0..dec.array_len()? {
        let bundle = threnody_core::prekey::PrekeyBundle::decode(dec.bytes()?)?;
        out.insert(*bundle.owner.as_bytes(), bundle);
    }
    finish(&dec)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_and_limits_hold() {
        for m in [
            MailboxMsg::Deposit {
                to: [1; 32],
                sealed: vec![1, 2],
                reference: None,
            },
            MailboxMsg::Deposit {
                to: [1; 32],
                sealed: vec![1, 2],
                reference: Some(7),
            },
            MailboxMsg::Deliver { sealed: vec![3] },
            MailboxMsg::Receipt {
                to: [2; 32],
                status: DepositStatus::Held,
                reference: None,
            },
            MailboxMsg::Receipt {
                to: [2; 32],
                status: DepositStatus::Declined,
                reference: Some(u64::MAX),
            },
        ] {
            assert_eq!(MailboxMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        let id = threnody_core::Identity::from_seed(&[1; 32]).public();
        let to = *id.as_bytes();
        let mut mb = MailboxStore::default();
        assert!(mb.hold(to, vec![0], 0, false));
        assert!(!mb.hold(to, vec![0], 0, false), "duplicate");
        for i in 1..MAX_HELD_MESSAGES {
            assert!(mb.hold(to, vec![i as u8, 1], 0, false));
        }
        assert!(!mb.hold(to, vec![9, 9, 9], 0, false), "count limit");
        let mut again = MailboxStore::decode(&mb.encode().unwrap()).unwrap();
        assert_eq!(again.held_for(), MAX_HELD_MESSAGES);
        assert_eq!(again.take(&id, 1).0.len(), MAX_HELD_MESSAGES);
        assert!(mb.take(&id, MAX_HOLD_MS + 1).0.is_empty(), "expired");
    }

    #[test]
    fn expired_and_forgotten_messages_go_at_rest() {
        let ids: Vec<PublicIdentity> = (1..=3)
            .map(|i| threnody_core::Identity::from_seed(&[i; 32]).public())
            .collect();
        let mut mb = MailboxStore::default();
        mb.hold(*ids[0].as_bytes(), vec![1], 0, false);
        mb.hold(*ids[1].as_bytes(), vec![2], 0, false);
        mb.hold(*ids[1].as_bytes(), vec![3], MAX_HOLD_MS, false);
        mb.hold(*ids[2].as_bytes(), vec![4], MAX_HOLD_MS, false);
        // Nobody comes back for the old ones: they still go.
        assert!(mb.prune(MAX_HOLD_MS + 1));
        assert!(!mb.prune(MAX_HOLD_MS + 1));
        assert_eq!(mb.held_for(), 2);
        assert!(!mb.held.contains_key(ids[0].as_bytes()));
        assert!(mb.forget(&ids[2]) && !mb.forget(&ids[2]));
        let again = MailboxStore::decode(&mb.encode().unwrap()).unwrap();
        assert_eq!(again.held_for(), 1);
    }

    #[test]
    fn a_placement_is_kept_only_when_no_mailbox_took_it() {
        let id = |i| threnody_core::Identity::from_seed(&[i; 32]).public();
        let (m1, m2, dest) = (id(1), id(2), id(3));
        let to = *dest.as_bytes();
        let now = Instant::now();
        let mut p = Placements::default();
        let open = |p: &mut Placements, asked| {
            let r = p.open(dest, AppMessage::Cover, Tag::NONE, asked);
            for m in [m1, m2] {
                p.wait(m, to, r, now);
            }
            r
        };
        // Both decline; one mailbox doesn't repeat the reference.
        let r = open(&mut p, 2);
        assert_eq!(p.receipt(m1, to, None), Some(r));
        assert!(p.answer(r, DepositStatus::Declined).is_none());
        assert_eq!(p.receipt(m2, to, Some(r)), Some(r));
        assert!(p.answer(r, DepositStatus::Declined).is_some());
        // One holds it: done, whatever the other says.
        let r = open(&mut p, 2);
        assert!(p.answer(r, DepositStatus::Held).is_none());
        assert!(p.waiting.is_empty() && p.open.is_empty());
        assert!(p.answer(r, DepositStatus::Declined).is_none());
        // One couldn't be asked, the other declines.
        let r = open(&mut p, 2);
        assert!(p.unask(r).is_none());
        assert!(p.answer(r, DepositStatus::Declined).is_some());
        // None could be asked: the caller keeps it.
        let r = open(&mut p, 1);
        assert!(p.unask(r).is_none());
        assert!(p.open.is_empty() && p.waiting.is_empty());
    }

    #[test]
    fn a_receipt_answers_only_its_own_deposit() {
        let id = |i| threnody_core::Identity::from_seed(&[i; 32]).public();
        let (m, dest) = (id(1), id(3));
        let to = *dest.as_bytes();
        let now = Instant::now();
        let mut p = Placements::default();
        // An edit (not placed) goes first, then a text (placed).
        let (edit, text) = (fresh_ref(), p.open(dest, AppMessage::Cover, Tag::NONE, 1));
        p.wait(m, to, edit, now);
        p.wait(m, to, text, now);
        // The edit's receipt says nothing about the text.
        assert_eq!(p.receipt(m, to, Some(edit)), Some(edit));
        assert!(p.answer(edit, DepositStatus::Declined).is_none());
        assert!(p.open.contains_key(&text));
        assert_eq!(p.receipt(m, to, Some(text)), Some(text));
        assert!(p.answer(text, DepositStatus::Declined).is_some());

        // A mailbox that doesn't repeat refs answers in order.
        let text = p.open(dest, AppMessage::Cover, Tag::NONE, 1);
        p.wait(m, to, edit, now);
        p.wait(m, to, text, now);
        assert_eq!(p.receipt(m, to, None), Some(edit));
        assert_eq!(p.receipt(m, to, None), Some(text));

        // Unplaced deposits never answered are forgotten in time; placed
        // ones wait for their own timer.
        let text = p.open(dest, AppMessage::Cover, Tag::NONE, 1);
        p.wait(m, to, edit, now);
        p.wait(m, to, text, now);
        p.wait(m, to, fresh_ref(), now + RECEIPT_WAIT);
        let left: Vec<u64> = p.waiting[&(m, to)].iter().map(|(r, _)| *r).collect();
        assert_eq!(left[0], text);
        assert_eq!(left.len(), 2);
    }

    #[test]
    fn refs_say_nothing_about_other_placements() {
        let dest = threnody_core::Identity::from_seed(&[3; 32]).public();
        let mut p = Placements::default();
        let refs: Vec<u64> = (0..4)
            .map(|_| p.open(dest, AppMessage::Cover, Tag::NONE, 1))
            .collect();
        assert!(refs.windows(2).all(|w| w[1] != w[0].wrapping_add(1)));
        assert!(refs.iter().all(|r| *r > 4));
    }

    #[test]
    fn placements_are_saved() {
        let dest = threnody_core::Identity::from_seed(&[3; 32]).public();
        let mut p = Placements::default();
        let tag = Tag {
            local_id: 9,
            group: Some([1; 16]),
            relay_for: None,
        };
        let msg = AppMessage::Text {
            sent_ms: 5,
            body: "kept".into(),
            expires_in_s: None,
            id: 7,
            quote: None,
            forwarded: false,
        };
        let r = p.open(dest, msg.clone(), tag, 2);
        assert!(p.dirty);
        let saved = Placements::decode(&p.encode().unwrap()).unwrap();
        assert_eq!(saved, vec![(r, dest, msg, tag)]);
        p.close(r);
        assert!(Placements::decode(&p.encode().unwrap()).unwrap().is_empty());
        assert!(Placements::decode(&[0xa0]).is_err());
    }

    #[tokio::test]
    async fn an_expired_disappearing_text_waits_sealed_not_plain() {
        use crate::{AcceptPolicy, NodeConfig};
        use threnody_core::store::Home;
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str, create: bool| {
            let home = Home::new(dir.path().join(name));
            let identity = if create {
                home.create_identity(None).unwrap()
            } else {
                home.load_identity(None).unwrap()
            };
            Node::new(NodeConfig {
                home,
                identity,
                policy: AcceptPolicy::Anyone,
                constant_rate: None,
                tunnel_port: None,
            })
            .unwrap()
        };
        let wait = |cond: &dyn Fn() -> bool| {
            let ok = (0..500).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                cond()
            });
            assert!(ok, "condition not reached");
        };
        let (a, _arx) = open("a", true);
        let (b, _brx) = open("b", true);
        let addr = a.listen("127.0.0.1:0").await.unwrap().to_string();
        let aid = b.connect(&addr, None).await.unwrap();
        let bid = b.identity();
        b.set_approval(&aid, true).unwrap();
        a.set_approval(&bid, true).unwrap();
        tokio::task::spawn_blocking({
            let a = a.clone();
            move || wait(&|| a.can_send_offline(&bid))
        })
        .await
        .unwrap();
        b.shutdown();
        let a2 = a.clone();
        tokio::task::spawn_blocking(move || wait(&|| a2.sessions().is_empty()))
            .await
            .unwrap();

        let secret = AppMessage::Text {
            sent_ms: now_ms(),
            body: "gone in 30 s".into(),
            expires_in_s: Some(30),
            id: 1,
            quote: None,
            forwarded: false,
        };
        a.hold(&bid, secret.clone(), Tag::NONE);
        a.seal_expired_unacked(now_ms() + 31_000);
        let waiting = a.shared.delivery(|d| d.resend(&bid));
        assert!(
            matches!(&waiting[..], [AppMessage::Tracked { inner, .. }] if matches!(**inner, AppMessage::Mailbox(_))),
            "{waiting:?}"
        );
        let saved = a
            .shared
            .home
            .load_state(a.identity_ref(), "unacked")
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&saved.unwrap()).contains("gone in 30 s"),
            "plain text left in the saved state"
        );

        // B still gets it, from A, as a sealed message.
        let (b2, mut b2rx) = open("b", false);
        b2.connect(&addr, None).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(Event::OfflineMessage { from, msg, .. }) = b2rx.recv().await {
                    return (from, msg);
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(got, (aid, secret));
    }

    #[tokio::test]
    async fn a_receipt_timer_does_nothing_after_shutdown() {
        use crate::{AcceptPolicy, NodeConfig};
        use threnody_core::store::Home;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("a"));
        let identity = home.create_identity(None).unwrap();
        let (a, _rx) = Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap();
        let id = |i| threnody_core::Identity::from_seed(&[i; 32]).public();
        let (m, dest) = (id(1), id(3));
        a.set_receipt_wait(Duration::from_millis(50));
        let place = || {
            let mut p = lock(&a.shared.placements);
            let r = p.open(dest, AppMessage::Cover, Tag::NONE, 1);
            p.wait(m, *dest.as_bytes(), r, Instant::now());
            r
        };
        // Running, an unanswered deposit counts as declined in time.
        let r = place();
        a.await_receipt(m, dest, r);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!lock(&a.shared.placements).open.contains_key(&r));
        assert_eq!(a.shared.delivery(|d| d.unacked(&dest)), 1);

        // Shut down first: the old node leaves it for the next start.
        let r = place();
        a.await_receipt(m, dest, r);
        a.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(lock(&a.shared.placements).open.contains_key(&r));
        assert_eq!(a.shared.delivery(|d| d.unacked(&dest)), 1);
    }

    #[test]
    fn large_messages_stay_until_acknowledged() {
        let id = threnody_core::Identity::from_seed(&[1; 32]).public();
        let big = vec![7; crate::delivery::MAX_PERSISTED];
        let mut mb = MailboxStore::default();
        mb.hold(*id.as_bytes(), vec![1], 0, false);
        mb.hold(*id.as_bytes(), big.clone(), 0, false);
        let (out, changed) = mb.take(&id, 1);
        assert_eq!((out, changed), (vec![vec![1], big.clone()], true));
        // Still held, but not sent twice in one run.
        assert_eq!(mb.held_for(), 1);
        assert_eq!(mb.take(&id, 1), (vec![], false));
        // A restart before the acknowledgement: it goes again.
        let mut again = MailboxStore::decode(&mb.encode().unwrap()).unwrap();
        assert_eq!(again.take(&id, 1).0, vec![big.clone()]);
        assert!(again.delivered(&id, &big) && !again.delivered(&id, &big));
        assert_eq!(again.held_for(), 0);
    }
}
