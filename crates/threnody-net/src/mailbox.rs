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
use crate::node::{Event, Node, lock};

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
    held: HashMap<[u8; 32], Vec<(u64, Vec<u8>)>>,
}

impl MailboxStore {
    fn hold(&mut self, to: [u8; 32], sealed: Vec<u8>, now: u64) -> bool {
        let q = self.held.entry(to).or_default();
        q.retain(|(t, _)| now.saturating_sub(*t) < MAX_HOLD_MS);
        let bytes: usize = q.iter().map(|(_, s)| s.len()).sum();
        if q.len() >= MAX_HELD_MESSAGES
            || bytes + sealed.len() > MAX_HELD_BYTES
            || q.iter().any(|(_, s)| *s == sealed)
        {
            return false;
        }
        q.push((now, sealed));
        true
    }

    pub(crate) fn take(&mut self, to: &PublicIdentity, now: u64) -> Vec<Vec<u8>> {
        self.held
            .remove(to.as_bytes())
            .unwrap_or_default()
            .into_iter()
            .filter(|(t, _)| now.saturating_sub(*t) < MAX_HOLD_MS)
            .map(|(_, s)| s)
            .collect()
    }

    pub(crate) fn held_for(&self) -> usize {
        self.held.values().map(Vec::len).sum()
    }

    pub(crate) fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let size: usize = self
            .held
            .values()
            .flatten()
            .map(|(_, s)| s.len() + 48)
            .sum::<usize>()
            + 64;
        cbor::to_vec(size, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(STATE_VERSION)?;
            e.u8(1)?.array_len(self.held_for())?;
            for (to, q) in &self.held {
                for (t, s) in q {
                    e.array_len(3)?.bytes(to)?.u64(*t)?.bytes(s)?;
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
                        me.held
                            .entry(to)
                            .or_default()
                            .push((t, d.bytes()?.to_vec()));
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
    pub(crate) fn mailbox_for(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let held = {
            let mut mb = lock(&self.shared.mailbox);
            let held = mb.take(peer, now_ms());
            if !held.is_empty() {
                self.shared.persist_mailbox(&mb);
            }
            held
        };
        held.into_iter()
            .filter_map(|sealed| MailboxMsg::Deliver { sealed }.encode().ok())
            .map(AppMessage::Mailbox)
            .collect()
    }

    /// A fresh prekey bundle for `peer`, encoded for `AppMessage::Prekeys`.
    pub(crate) fn prekeys_for(&self, peer: &PublicIdentity) -> Option<AppMessage> {
        let mut store = lock(&self.shared.prekeys);
        let bundle = store.bundle_for(self.identity_ref(), peer, now_ms());
        self.shared.persist_prekeys(&store);
        bundle.encode().ok().map(AppMessage::Prekeys)
    }

    pub(crate) fn on_prekeys(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(bundle) = threnody_core::prekey::PrekeyBundle::decode(payload) else {
            return;
        };
        let mut book = lock(&self.shared.bundles);
        if book.insert(&from, bundle, now_ms()).is_ok() {
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
        let Ok(dest) = PublicIdentity::from_bytes(&to) else {
            return DepositStatus::Declined;
        };
        if !self.shared.mutual(from) || !self.shared.mutual(&dest) {
            return DepositStatus::Declined;
        }
        if self.sessions().iter().any(|s| s.peer == dest) {
            let deliver = MailboxMsg::Deliver {
                sealed: sealed.clone(),
            };
            if self.mailbox_send(&dest, &deliver) {
                return DepositStatus::Delivered;
            }
        }
        let mut mb = lock(&self.shared.mailbox);
        if mb.hold(to, sealed, now_ms()) {
            self.shared.persist_mailbox(&mb);
            DepositStatus::Held
        } else {
            DepositStatus::Declined
        }
    }

    fn open_sealed(&self, via: PublicIdentity, sealed: &[u8]) {
        let me = self.identity();
        let contacts = self.contacts();
        let opened = {
            let mut store = lock(&self.shared.prekeys);
            let r = store.open(&me, sealed, now_ms(), |k| {
                contacts
                    .iter()
                    .find(|c| c.key.as_bytes() == k)
                    .map(|c| c.key)
            });
            if r.is_ok() {
                self.shared.persist_prekeys(&store);
            }
            r
        };
        // Failures (duplicates from several mailboxes, strangers, forgeries)
        // are dropped silently, as for any unauthenticated input.
        if let Ok((from, body)) = opened
            && let Ok(msg) = AppMessage::decode(&body)
        {
            self.emit(Event::OfflineMessage { from, via, msg });
        }
    }

    /// Number of sealed messages we are holding for others.
    pub fn held_messages(&self) -> usize {
        lock(&self.shared.mailbox).held_for()
    }
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
        assert!(mb.hold(to, vec![0], 0));
        assert!(!mb.hold(to, vec![0], 0), "duplicate");
        for i in 1..MAX_HELD_MESSAGES {
            assert!(mb.hold(to, vec![i as u8, 1], 0));
        }
        assert!(!mb.hold(to, vec![9, 9, 9], 0), "count limit");
        let mut again = MailboxStore::decode(&mb.encode().unwrap()).unwrap();
        assert_eq!(again.held_for(), MAX_HELD_MESSAGES);
        assert_eq!(again.take(&id, 1).len(), MAX_HELD_MESSAGES);
        assert!(mb.take(&id, MAX_HOLD_MS + 1).is_empty(), "expired");
    }
}
