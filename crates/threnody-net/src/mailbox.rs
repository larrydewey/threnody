//! Store-and-forward mailboxes for sealed messages
//! (`docs/appendix-h-offline.md`).
//!
//! ```text
//! MailboxMsg = { 0: op, ? 1: to (32), ? 2: sealed, ? 3: status }
//! op: 1 deposit, 2 deliver, 3 receipt (status 0 declined, 1 held, 2 delivered now)
//! ```

use std::collections::HashMap;

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::node::{Event, Node, Shared, lock};

/// Per-recipient limits for held messages.
pub const MAX_HELD_MESSAGES: usize = 100;
pub const MAX_HELD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HOLD_MS: u64 = 14 * 24 * 60 * 60 * 1000;
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
    Deposit { to: [u8; 32], sealed: Vec<u8> },
    Deliver { sealed: Vec<u8> },
    Receipt { to: [u8; 32], status: DepositStatus },
}

impl MailboxMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let size = match self {
            Self::Deposit { sealed, .. } | Self::Deliver { sealed } => sealed.len(),
            Self::Receipt { .. } => 0,
        } + 64;
        cbor::to_vec(size, |e| {
            match self {
                Self::Deposit { to, sealed } => {
                    e.map_len(3)?.u8(0)?.u8(1)?;
                    e.u8(1)?.bytes(to)?;
                    e.u8(2)?.bytes(sealed)?;
                }
                Self::Deliver { sealed } => {
                    e.map_len(2)?.u8(0)?.u8(2)?;
                    e.u8(2)?.bytes(sealed)?;
                }
                Self::Receipt { to, status } => {
                    e.map_len(3)?.u8(0)?.u8(3)?;
                    e.u8(1)?.bytes(to)?;
                    e.u8(3)?.u8(*status as u8)?;
                }
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut to, mut sealed, mut status) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => to = Some(fixed_bytes::<32>(d)?),
                2 => sealed = Some(d.bytes()?.to_vec()),
                3 => status = Some(d.u8()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(op, "mailbox op")? {
            1 => Self::Deposit {
                to: required(to, "recipient")?,
                sealed: required(sealed, "sealed message")?,
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

impl Node {
    fn mailbox_send(&self, to: &PublicIdentity, m: &MailboxMsg) -> bool {
        m.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Mailbox(b)).is_ok())
    }

    /// True when we hold a usable prekey bundle for `peer`.
    pub fn can_send_offline(&self, peer: &PublicIdentity) -> bool {
        lock(&self.shared.bundles).has(peer, now_ms())
    }

    /// Seals `msg` for `dest` and offers it to every live, mutually
    /// approved neighbour. Returns how many were asked; each answers with
    /// [`Event::DepositReceipt`].
    pub fn send_offline(&self, dest: &PublicIdentity, msg: &AppMessage) -> Result<usize> {
        let sealed = {
            let mut book = lock(&self.shared.bundles);
            let sealed = book.seal(self.identity_ref(), dest, &msg.encode()?, now_ms())?;
            self.shared.persist_bundles(&book);
            sealed
        };
        let deposit = MailboxMsg::Deposit {
            to: *dest.as_bytes(),
            sealed,
        };
        let mailboxes: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .filter(|s| s.via.is_none() && s.peer != *dest && self.shared.mutual(&s.peer))
            .map(|s| s.peer)
            .collect();
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
        if let (Some(handle), false) = (handle, hidden.is_empty()) {
            let MailboxMsg::Deposit { sealed, .. } = &deposit else {
                unreachable!()
            };
            let sealed = std::sync::Arc::new(sealed.clone());
            for m in &hidden {
                let (node, m, dest, sealed) = (self.clone(), *m, *dest, sealed.clone());
                let deposit = MailboxMsg::Deposit {
                    to: *dest.as_bytes(),
                    sealed: sealed.to_vec(),
                };
                handle.spawn(async move {
                    match node.onion_deposit(&m, &dest, &sealed).await {
                        Ok(status) => node.emit(Event::DepositReceipt {
                            mailbox: m,
                            to: dest,
                            status,
                            anonymous: true,
                        }),
                        // Every circuit failed: deliver it rather than lose it.
                        Err(_) => {
                            node.mailbox_send(&m, &deposit);
                        }
                    }
                });
            }
            return Ok(hidden.len());
        }
        let n = mailboxes
            .iter()
            .filter(|m| self.mailbox_send(m, &deposit))
            .count();
        if n == 0 {
            return Err(NetError::NoRoute(format!(
                "{} (no mailbox neighbour online)",
                dest.fingerprint()
            )));
        }
        Ok(n)
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
            MailboxMsg::Deposit { to, sealed } if to == *self.identity().as_bytes() => {
                self.open_sealed(from, &sealed);
                self.mailbox_send(
                    &from,
                    &MailboxMsg::Receipt {
                        to,
                        status: DepositStatus::Delivered,
                    },
                );
            }
            MailboxMsg::Deposit { to, sealed } => {
                let status = self.accept_deposit(&from, to, sealed);
                self.mailbox_send(&from, &MailboxMsg::Receipt { to, status });
            }
            MailboxMsg::Deliver { sealed } => self.open_sealed(from, &sealed),
            MailboxMsg::Receipt { to, status } => {
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
            if let AppMessage::Delete { conversation, ids } = &msg {
                self.on_delete(&from, conversation, ids);
                return;
            }
            if let AppMessage::Edit {
                conversation,
                id,
                body,
            } = &msg
            {
                self.on_edit(&from, conversation, *id, body);
                return;
            }
            self.record_incoming(&from, &msg, true);
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
            },
            MailboxMsg::Deliver { sealed: vec![3] },
            MailboxMsg::Receipt {
                to: [2; 32],
                status: DepositStatus::Held,
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
