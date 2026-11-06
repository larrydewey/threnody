//! Sending text to accounts, and recording history (spec §6.1, §6.4).

use std::collections::HashSet;
use std::time::Duration;

use threnody_core::history::{ConversationId, Entry, FileNote, History, Hit, Query};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use threnody_core::message::{
    Clip, FEATURE_READ, FEATURE_RECEIPTS, FEATURE_TIMER_OFF, FEATURE_TYPING, Mention, Quote,
};

use crate::delivery::{Tag, needs};
use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};
use crate::receipts::read_messages;

/// A week: the usual choice when the user turns disappearing messages on.
pub const WEEK_S: u32 = 7 * 24 * 3600;
/// What an incoming entry's `read_ms` holds once we reported it read.
const REPORTED: u64 = 1;

/// A random non-zero id for an outgoing history entry.
pub fn local_id() -> u64 {
    u64::from_le_bytes(threnody_core::crypto::random_bytes()).max(1)
}

/// How often expired messages are swept from disk.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// What happened to one outgoing message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendReport {
    pub live: usize,
    pub sealed: usize,
    /// Held until a session with the device comes up.
    pub queued: usize,
}

/// A file to send, with what goes with it.
#[derive(Clone, Debug, Default)]
pub struct OutgoingFile {
    pub name: String,
    pub data: Vec<u8>,
    /// Where it is on this device, recorded in history.
    pub location: Option<String>,
    /// Show it covered until opened.
    pub sensitive: bool,
    /// Text sent with it (empty if none).
    pub caption: String,
    /// Shared by files sent together (0 = alone); see [`album_id`].
    pub album: u64,
    /// A voice or video message recorded in the app.
    pub clip: Option<Clip>,
    /// Forwarded from another chat.
    pub forwarded: bool,
}

/// What a recorded message carries besides its text.
#[derive(Clone, Debug, Default)]
pub struct Extras {
    pub quote: Option<Quote>,
    pub mentions: Vec<Mention>,
    pub forwarded: bool,
}

/// A fresh album id for files sent together.
pub fn album_id() -> u64 {
    local_id()
}

/// What [`Node::record_incoming`] did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recorded {
    /// Recorded (or nothing to record): it shows.
    New,
    /// A copy of a text already recorded: it doesn't show again.
    Duplicate,
    /// It couldn't be stored (a full disk, no file descriptors left).
    Failed,
}

impl Node {
    /// Whether we want `peer`'s messages: we accepted it, or another device
    /// of its account, or it's ours. Blocked peers never are.
    pub fn is_accepted(&self, peer: &PublicIdentity) -> bool {
        if self.is_own_device(peer) {
            return true;
        }
        let contacts = lock(&self.shared.contacts);
        let Some(c) = contacts.get(peer) else {
            return false;
        };
        if c.blocked {
            return false;
        }
        c.accepted
            || c.account.is_some_and(|a| {
                contacts
                    .iter()
                    .any(|o| o.account == Some(a) && o.accepted && !o.blocked)
            })
    }

    /// Deletes messages (by [`Entry::message_id`]) from `peer`'s
    /// conversation, here and on our other devices. With `everyone`, our
    /// own messages among them are also deleted on the devices of `peer`'s
    /// account that support it. Returns how many entries went here.
    pub fn delete_messages(&self, peer: &PublicIdentity, ids: &[u64], everyone: bool) -> usize {
        let conv = self.conversation_for(peer);
        let me = *self.identity().as_bytes();
        let ours: Vec<u64> = self
            .history(conv)
            .map(|h| {
                h.entries()
                    .iter()
                    .filter(|e| e.outgoing && e.device == me && ids.contains(&e.local_id))
                    .map(|e| e.local_id)
                    .collect()
            })
            .unwrap_or_default();
        let gone = self.delete_local(conv, ids);
        let msg = |ids: Vec<u64>| AppMessage::Delete {
            conversation: conv.to_bytes(),
            ids,
        };
        // Our other devices delete the same, whatever they are, and those
        // away now when they come back.
        for d in self.own_devices() {
            self.send_change(&d, &msg(ids.to_vec()));
        }
        if everyone && !ours.is_empty() {
            let devices: Vec<PublicIdentity> = match self.account_of(peer) {
                Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
                None => vec![*peer],
            };
            for d in devices {
                if d != self.identity() {
                    self.send_change(&d, &msg(ours.clone()));
                }
            }
        }
        gone
    }

    fn delete_local(&self, conv: ConversationId, ids: &[u64]) -> usize {
        self.shared
            .home
            .delete_entries(self.identity_ref(), conv, now_ms(), |e| {
                let id = e.message_id();
                id != 0 && ids.contains(&id)
            })
            .unwrap_or(0)
    }

    /// Deletes entries by time and device (for entries without an id, such
    /// as older ones or group messages): only here and on our devices'
    /// own copies, never for others.
    pub fn delete_entry(&self, conv: ConversationId, at_ms: u64, device: [u8; 32]) -> usize {
        self.shared
            .home
            .delete_entries(self.identity_ref(), conv, now_ms(), |e| {
                e.at_ms == at_ms && e.device == device
            })
            .unwrap_or(0)
    }

    /// Handles a `Delete` from `from`: one of our devices deleting in a
    /// conversation, or a peer taking back its own messages.
    pub(crate) fn on_delete(&self, from: &PublicIdentity, conversation: &[u8], ids: &[u64]) {
        let gone = if self.is_own_device(from) {
            match ConversationId::from_bytes(conversation) {
                // Our sibling's chat with *us* is our chat with it.
                Some(ConversationId::Peer(k)) if k == *self.identity().as_bytes() => {
                    self.delete_local(self.conversation_for(from), ids)
                }
                Some(conv) => self.delete_local(conv, ids),
                None => 0,
            }
        } else {
            // Only messages that device's account sent us.
            let conv = self.conversation_for(from);
            let senders: Vec<[u8; 32]> = match self.account_of(from) {
                Some(a) => a
                    .state()
                    .devices
                    .iter()
                    .map(|(d, _)| *d.as_bytes())
                    .collect(),
                None => vec![*from.as_bytes()],
            };
            self.shared
                .home
                .delete_entries(self.identity_ref(), conv, now_ms(), |e| {
                    !e.outgoing
                        && senders.contains(&e.device)
                        && e.remote_id != 0
                        && ids.contains(&e.remote_id)
                })
                .unwrap_or(0)
        };
        if gone > 0 {
            self.emit(Event::MessagesDeleted {
                peer: *from,
                count: gone,
            });
        }
    }

    /// Edits one of our own messages to `peer` (by its id): here, on our
    /// other devices, and on the devices of `peer`'s account that support
    /// it. Returns false if there's no such message of ours.
    pub fn edit_message(&self, peer: &PublicIdentity, id: u64, body: &str) -> bool {
        let conv = self.conversation_for(peer);
        let me = *self.identity().as_bytes();
        let n = self
            .shared
            .home
            .edit_entries(self.identity_ref(), conv, now_ms(), body, |e| {
                e.outgoing && e.device == me && e.local_id == id && id != 0
            })
            .unwrap_or(0);
        if n == 0 {
            return false;
        }
        let msg = AppMessage::Edit {
            conversation: conv.to_bytes(),
            id,
            body: body.to_owned(),
        };
        for d in self.account_targets(peer) {
            self.send_change(&d, &msg);
        }
        true
    }

    /// Who reacts, as reactions record it: an account (so a person's
    /// devices are one reactor), else the device.
    pub fn reactor_of(&self, peer: &PublicIdentity) -> [u8; 32] {
        if *peer == self.identity() || self.is_own_device(peer) {
            return self.account().id().0;
        }
        self.account_of(peer)
            .map(|a| a.id().0)
            .or_else(|| {
                lock(&self.shared.contacts)
                    .get(peer)
                    .and_then(|c| c.account)
                    .map(|a| a.0)
            })
            .unwrap_or(*peer.as_bytes())
    }

    /// Pins (or with `pin` false, unpins) message `id` in `conv` on this
    /// device only (groups tell members themselves). Returns true if that
    /// changed anything.
    pub fn pin_in(&self, conv: ConversationId, id: u64, pin: bool) -> bool {
        self.shared
            .home
            .pin_entry(self.identity_ref(), conv, now_ms(), id, pin)
            .unwrap_or(false)
    }

    /// Sets when `conv`'s new messages notify, on this device.
    pub fn set_notify(
        &self,
        conv: ConversationId,
        n: threnody_core::history::Notify,
    ) -> Result<()> {
        self.shared
            .home
            .set_notify(self.identity_ref(), conv, now_ms(), n)
            .map_err(NetError::from)
    }

    /// Keeps what the user started writing in `conv` (empty: none), on
    /// this device.
    pub fn set_draft(&self, conv: ConversationId, draft: &str) -> Result<()> {
        self.shared
            .home
            .set_draft(self.identity_ref(), conv, now_ms(), draft)
            .map_err(NetError::from)
    }

    /// Notes to self: a chat with our own account, kept on all our
    /// devices (each sends what it writes there to the others).
    pub fn notes(&self) -> ConversationId {
        ConversationId::Peer(self.account().id().0)
    }

    /// Writes a note to self, optionally quoting an earlier one, or with a
    /// file (kept where it is on this device; the others get its record).
    pub fn write_note(&self, text: &str, quote: Option<Quote>, file: Option<FileNote>) {
        let now = now_ms();
        let conv = self.notes();
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing: true,
                device: *self.identity().as_bytes(),
                text: text.to_owned(),
                offline: false,
                expires_at_ms: self
                    .effective_timer(conv)
                    .map(|s| now + u64::from(s) * 1000),
                file,
                local_id: local_id(),
                // Nobody else to deliver to.
                delivered: true,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id: 0,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
                quote,
                mentions: Vec::new(),
                poll: None,
                forwarded: false,
                list: None,
                ticked_ms: 0,
            },
        );
    }

    /// Sets `who`'s vote in poll `id` in `conv`, on this device; returns
    /// true if that changed anything.
    pub fn vote_in(&self, conv: ConversationId, id: u64, who: [u8; 32], choices: &[u32]) -> bool {
        self.shared
            .home
            .vote_entry(self.identity_ref(), conv, now_ms(), id, who, choices)
            .unwrap_or(false)
    }

    /// Closes poll `id` in `conv`, on this device, if `author` accepts the
    /// device that sent it; returns true if that changed anything.
    pub fn close_poll_in(
        &self,
        conv: ConversationId,
        id: u64,
        author: impl Fn(&[u8; 32]) -> bool,
    ) -> bool {
        self.shared
            .home
            .close_poll(self.identity_ref(), conv, now_ms(), id, author)
            .unwrap_or(false)
    }

    /// Pinned messages in `conv`, oldest pin first.
    pub fn pinned(&self, conv: ConversationId) -> Vec<Entry> {
        let Ok(h) = self.history(conv) else {
            return Vec::new();
        };
        h.pinned()
            .iter()
            .filter_map(|id| h.entries().iter().find(|e| e.message_id() == *id))
            .cloned()
            .collect()
    }

    /// Pins (or unpins) message `id` in our conversation with `peer`, for
    /// both sides: tells their devices and ours. Returns false if there's
    /// no such message.
    pub fn pin(&self, peer: &PublicIdentity, id: u64, pin: bool) -> bool {
        let conv = self.conversation_for(peer);
        let known = self
            .history(conv)
            .is_ok_and(|h| id != 0 && h.entries().iter().any(|e| e.message_id() == id));
        if !known {
            return false;
        }
        self.pin_in(conv, id, pin);
        let msg = AppMessage::Pin {
            conversation: conv.to_bytes(),
            id,
            pin,
        };
        for d in self.account_targets(peer) {
            self.send_change(&d, &msg);
        }
        true
    }

    /// `peer` (or one of our devices) pinned or unpinned message `id`.
    pub(crate) fn on_pin(&self, from: &PublicIdentity, conversation: &[u8], id: u64, pin: bool) {
        let Some(conv) = self.conversation_from(from, conversation) else {
            return;
        };
        if self.pin_in(conv, id, pin) {
            self.emit(Event::Pinned {
                peer: *from,
                id,
                pin,
            });
        }
    }

    /// The conversation a message from `from` names: ours with `from`, or
    /// for one of our own devices, the one it names (its chat with us
    /// being ours with it).
    fn conversation_from(
        &self,
        from: &PublicIdentity,
        conversation: &[u8],
    ) -> Option<ConversationId> {
        if !self.is_own_device(from) {
            return Some(self.conversation_for(from));
        }
        match ConversationId::from_bytes(conversation)? {
            ConversationId::Peer(k) if k == *self.identity().as_bytes() => {
                Some(self.conversation_for(from))
            }
            c => Some(c),
        }
    }

    /// Every device of `peer`'s account and our other devices, connected
    /// or not, except this one: where a change to our chat with `peer` goes.
    fn account_targets(&self, peer: &PublicIdentity) -> Vec<PublicIdentity> {
        let mut targets = self.own_devices();
        match self.account_of(peer) {
            Some(a) => targets.extend(a.state().devices.iter().map(|(d, _)| *d)),
            None => targets.push(*peer),
        }
        targets.sort_unstable_by_key(|p| *p.as_bytes());
        targets.dedup();
        targets.retain(|d| *d != self.identity());
        targets
    }

    /// Our account's other devices, connected or not.
    pub(crate) fn own_devices(&self) -> Vec<PublicIdentity> {
        let me = self.identity();
        self.account()
            .state()
            .devices
            .iter()
            .map(|(d, _)| *d)
            .filter(|d| *d != me)
            .collect()
    }

    /// Sends `msg`, a change to a conversation (a deletion, an edit, a
    /// reaction, a pin or a list), to device `d`. It is tracked, so it is
    /// sent again if lost. For a device that isn't connected it waits for
    /// its next session, and is also sealed for its mailboxes when we can,
    /// so it arrives even if we don't meet first; taking a change twice
    /// does nothing more, and a mailbox copy of one already taken is
    /// dropped (see `Delivery::seen_change`). A device that told us it can't read such changes
    /// is skipped; one we haven't heard from since starting gets it held,
    /// dropped then if it can't (see `needs`).
    pub(crate) fn send_change(&self, d: &PublicIdentity, msg: &AppMessage) {
        if self.lacks(d, needs(msg)) || self.send_tracked(d, msg.clone()).is_ok() {
            return;
        }
        self.hold(d, msg.clone(), Tag::NONE);
        if self.can_send_offline(d) {
            let _ = self.send_offline(d, msg);
        }
    }

    /// Adds (or with `add` false, takes away) our `emoji` on message `id`
    /// in our conversation with `peer`, whoever sent it; tells their
    /// devices and ours. Returns false if there's no such message.
    pub fn react(&self, peer: &PublicIdentity, id: u64, emoji: &str, add: bool) -> bool {
        if id == 0 || !threnody_core::history::valid_emoji(emoji) {
            return false;
        }
        let conv = self.conversation_for(peer);
        let me = self.reactor_of(&self.identity());
        let found = self
            .shared
            .home
            .load_history(self.identity_ref(), conv, now_ms())
            .is_ok_and(|h| h.entries().iter().any(|e| e.message_id() == id));
        if !found {
            return false;
        }
        let _ = self.shared.home.react_entries(
            self.identity_ref(),
            conv,
            now_ms(),
            me,
            emoji,
            add,
            |e| e.message_id() == id,
        );
        let msg = AppMessage::React {
            conversation: conv.to_bytes(),
            id,
            emoji: emoji.to_owned(),
            add,
        };
        for d in self.account_targets(peer) {
            self.send_change(&d, &msg);
        }
        true
    }

    /// Tells the devices of `peer`'s account we are (or aren't) typing in
    /// their conversation with us. Transient: not tracked, not sealed for
    /// mailboxes, and ignored by peers without [`FEATURE_TYPING`].
    pub fn set_typing(&self, peer: &PublicIdentity, active: bool) -> Result<()> {
        let msg = AppMessage::Typing { active };
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) if !self.is_own_device(peer) => {
                a.state().devices.iter().map(|(d, _)| *d).collect()
            }
            _ => vec![*peer],
        };
        let mut reached = false;
        for d in &devices {
            if !self.supports(d, FEATURE_TYPING) {
                continue;
            }
            if self.send(d, msg.clone()).is_ok() {
                reached = true;
            }
        }
        if reached {
            Ok(())
        } else {
            Err(NetError::NoRoute(format!(
                "{} (no session with a typing-capable peer)",
                peer.fingerprint()
            )))
        }
    }

    /// Adds or takes away `who`'s `emoji` on message `id` in `conv` (used
    /// for groups, whose reactions travel inside MLS). Returns true if it
    /// changed anything.
    pub fn apply_reaction(
        &self,
        conv: ConversationId,
        who: [u8; 32],
        id: u64,
        emoji: &str,
        add: bool,
    ) -> bool {
        id != 0
            && self
                .shared
                .home
                .react_entries(self.identity_ref(), conv, now_ms(), who, emoji, add, |e| {
                    e.message_id() == id
                })
                .unwrap_or(0)
                > 0
    }

    /// Handles a `React` from `from`: a peer reacting in our conversation,
    /// or one of our devices reacting (in `conversation`) as us.
    pub(crate) fn on_react(
        &self,
        from: &PublicIdentity,
        conversation: &[u8],
        id: u64,
        emoji: &str,
        add: bool,
    ) {
        let conv = if self.is_own_device(from) {
            match ConversationId::from_bytes(conversation) {
                Some(ConversationId::Peer(k)) if k == *self.identity().as_bytes() => {
                    self.conversation_for(from)
                }
                Some(c) => c,
                None => return,
            }
        } else {
            self.conversation_for(from)
        };
        let who = self.reactor_of(from);
        let n = self
            .shared
            .home
            .react_entries(self.identity_ref(), conv, now_ms(), who, emoji, add, |e| {
                e.message_id() == id && id != 0
            })
            .unwrap_or(0);
        if n > 0 {
            self.emit(Event::Reacted { peer: *from, id });
        }
    }

    /// Tells `peer`'s account we displayed the messages it sent us with
    /// the sender-ids `ids` (their `remote_id`s in our conversation): live
    /// to its connected devices, and to the device that sent each one, if
    /// it isn't connected, by a receipt that waits for it or goes sealed
    /// through mailboxes (see `receipts`). Each message is reported once
    /// (its `read_ms` here is set to 1, a flag that keeps no time). Nothing is sent with read
    /// receipts off, or to someone we haven't accepted. Messages from
    /// before this version first ran, or older than a mailbox keeps
    /// anything, are reported live only. Fails with `NoRoute` when nothing
    /// could go or wait, for the caller to try again later.
    pub fn report_read(&self, peer: &PublicIdentity, ids: &[u64]) -> Result<()> {
        if ids.is_empty()
            || self.is_own_device(peer)
            || !self.is_accepted(peer)
            || !self.send_read_receipts()
        {
            return Ok(());
        }
        let conv = self.conversation_for(peer);
        let now = now_ms();
        let since = self.shared.receipts(|o| o.since_ms);
        let wanted: HashSet<u64> = ids.iter().copied().filter(|i| *i != 0).collect();
        // Which are already reported, and which may wait for their device.
        let mut reported = HashSet::new();
        let mut queueable: Vec<(PublicIdentity, u64)> = Vec::new();
        if let Ok(h) = self.history(conv) {
            for e in h
                .entries()
                .iter()
                .filter(|e| !e.outgoing && wanted.contains(&e.remote_id))
            {
                if e.read_ms != 0 {
                    reported.insert(e.remote_id);
                } else if e.at_ms >= since
                    && now.saturating_sub(e.at_ms) < crate::receipts::MAX_AGE_MS
                    && let Ok(d) = PublicIdentity::from_bytes(&e.device)
                {
                    queueable.push((d, e.remote_id));
                }
            }
        }
        let mut seen = HashSet::new();
        let fresh: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|i| wanted.contains(i) && !reported.contains(i) && seen.insert(*i))
            .collect();
        if fresh.is_empty() {
            return Ok(());
        }
        let conv_bytes = conv.to_bytes();
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
            None => vec![*peer],
        };
        let mut reached = HashSet::new();
        for d in &devices {
            // At most 512 ids a message: the receiver refuses more, and
            // that ends the session.
            let sent = if self.supports(d, FEATURE_RECEIPTS) {
                read_messages(FEATURE_RECEIPTS, &conv_bytes, &fresh)
                    .into_iter()
                    .all(|m| self.send_tracked(d, m).is_ok())
            } else if self.supports(d, FEATURE_READ) {
                read_messages(FEATURE_READ, &conv_bytes, &fresh)
                    .into_iter()
                    .all(|m| self.send(d, m).is_ok())
            } else {
                false
            };
            if sent {
                reached.insert(*d);
            }
        }
        let mut done: HashSet<u64> = if reached.is_empty() {
            HashSet::new()
        } else {
            fresh.iter().copied().collect()
        };
        // The device that sent each message, if it wasn't reached, gets
        // a receipt that waits for it.
        let mut by_device: Vec<(PublicIdentity, Vec<u64>)> = Vec::new();
        for (d, id) in queueable {
            if reached.contains(&d) || !fresh.contains(&id) {
                continue;
            }
            match by_device.iter_mut().find(|(x, _)| *x == d) {
                Some((_, v)) => v.push(id),
                None => by_device.push((d, vec![id])),
            }
        }
        for (d, these) in by_device {
            if self.queue_receipt(&d, &[], &these) {
                done.extend(these);
            }
        }
        if done.is_empty() {
            return Err(NetError::NoRoute(format!(
                "{} (no session with a read-receipt-capable peer)",
                peer.fingerprint()
            )));
        }
        // A flag, not a time: history at rest doesn't show when the user
        // read each message.
        let _ = self
            .shared
            .home
            .mark_read_entries(self.identity_ref(), conv, REPORTED, |e| {
                !e.outgoing && done.contains(&e.remote_id)
            });
        Ok(())
    }

    /// Handles a `Read` from `from`: it displayed our outgoing messages
    /// `ids` in our conversation with it; their "seen" ticks update.
    /// `conversation` is ours as the reader names it (after our account,
    /// not theirs), so like an edit's it says nothing here: the
    /// conversation is the one with `from`.
    pub(crate) fn on_read(&self, from: &PublicIdentity, _conversation: &[u8], ids: &[u64]) {
        if self.is_own_device(from) {
            return;
        }
        let conv = self.conversation_for(from);
        let now = now_ms();
        let ids: HashSet<u64> = ids.iter().copied().collect();
        let n = self
            .shared
            .home
            .mark_read_entries(self.identity_ref(), conv, now, |e| {
                e.outgoing && e.local_id != 0 && ids.contains(&e.local_id)
            })
            .unwrap_or(0);
        if n > 0 {
            self.emit(Event::Read { peer: *from });
        }
    }

    /// Handles an `Edit` from `from`: one of our devices editing our own
    /// message, or a peer editing a message its account sent us.
    pub(crate) fn on_edit(&self, from: &PublicIdentity, conversation: &[u8], id: u64, body: &str) {
        let n = if self.is_own_device(from) {
            let conv = match ConversationId::from_bytes(conversation) {
                Some(ConversationId::Peer(k)) if k == *self.identity().as_bytes() => {
                    self.conversation_for(from)
                }
                Some(c) => c,
                None => return,
            };
            // Our message, as any of our devices knows it.
            self.shared
                .home
                .edit_entries(self.identity_ref(), conv, now_ms(), body, |e| {
                    e.message_id() == id && id != 0
                })
                .unwrap_or(0)
        } else {
            let conv = self.conversation_for(from);
            let senders: Vec<[u8; 32]> = match self.account_of(from) {
                Some(a) => a
                    .state()
                    .devices
                    .iter()
                    .map(|(d, _)| *d.as_bytes())
                    .collect(),
                None => vec![*from.as_bytes()],
            };
            self.shared
                .home
                .edit_entries(self.identity_ref(), conv, now_ms(), body, |e| {
                    !e.outgoing && senders.contains(&e.device) && e.remote_id == id && id != 0
                })
                .unwrap_or(0)
        };
        if n > 0 {
            self.emit(Event::MessageEdited { peer: *from, id });
        }
    }

    /// Accepts `peer`'s message requests (every device of its account):
    /// its messages show as a conversation from now on.
    pub fn accept_contact(&self, peer: &PublicIdentity) {
        // Accepting in advance (before they've connected) works too.
        self.update_contacts(|c| c.observe(*peer, None, now_ms()));
        let devices = self.mark_contacts(peer, |c| c.accepted = true);
        // Now they may see our account (withheld from strangers).
        self.send_chain(&devices);
    }

    /// Blocks `peer` and every device of its account: their sessions end
    /// and are refused, their tunnels and discovery keys go, our
    /// conversation with them is deleted, and lists are no longer shared
    /// with them. Our other devices learn the block through contact sync.
    pub fn block_contact(&self, peer: &PublicIdentity) {
        let conv = self.conversation_for(peer);
        let now = now_ms();
        let devices = self.mark_contacts(peer, |c| {
            c.blocked = true;
            c.accepted = false;
            // A revocation like any other: dated, so an older approval
            // synced from our other devices can't bring it back (the
            // latest of a block and an approval wins across our devices).
            c.local_approved = false;
            c.approval_changed_ms = c.approval_changed_ms.max(now);
            c.clear_discovery_keys();
        });
        for d in devices {
            self.withdraw_approval(&d);
            self.disconnect(&d);
            self.forget_peer(&d);
        }
        let _ = self.shared.home.delete_history(self.identity_ref(), conv);
        self.unshare_lists_with(conv);
        self.push_contact_sync();
    }

    /// Deletes a message request: the contact and the conversation go, and
    /// they can write again (as a new request). Returns false, changing
    /// nothing, when `peer` isn't a request: an accepted contact (its
    /// conversation stays), a blocked one (it stays blocked) or a stranger.
    pub fn delete_request(&self, peer: &PublicIdentity) -> bool {
        let request = !self.is_accepted(peer)
            && lock(&self.shared.contacts)
                .get(peer)
                .is_some_and(|c| !c.blocked);
        if !request {
            return false;
        }
        let conv = self.conversation_for(peer);
        let devices = self.mark_contacts(peer, |_| {});
        let mut gone = Vec::new();
        self.update_contacts(|c| {
            for d in &devices {
                if c.get(d).is_some_and(|x| !x.accepted) {
                    c.remove(d);
                    gone.push(*d);
                }
            }
        });
        self.shared.prune_tunnels();
        for d in devices {
            self.disconnect(&d);
        }
        for d in &gone {
            self.forget_peer(d);
        }
        let _ = self.shared.home.delete_history(self.identity_ref(), conv);
        true
    }

    /// Deletes the conversation with `peer`: every device of its account
    /// leaves our contacts and the history goes, here and (through contact
    /// sync) on our other devices. If they write again, it's a new request.
    pub fn delete_conversation(&self, peer: &PublicIdentity) {
        let conv = self.conversation_for(peer);
        let devices = self.mark_contacts(peer, |_| {});
        let now = now_ms();
        self.update_contacts(|c| {
            for d in &devices {
                c.forget(d, now);
            }
        });
        for d in &devices {
            self.withdraw_approval(d);
            self.disconnect(d);
            self.forget_peer(d);
        }
        let _ = self.shared.home.delete_history(self.identity_ref(), conv);
        self.push_contact_sync();
    }

    /// Clears the conversation with `peer`: its messages go, here and on
    /// our other devices, but the contact stays (approved, verified and
    /// reachable as before), and so does the chat's timer.
    pub fn clear_conversation(&self, peer: &PublicIdentity) {
        let now = now_ms();
        self.mark_contacts(peer, |c| c.cleared_ms = c.cleared_ms.max(now));
        self.clear_before(self.conversation_for(peer), now);
        self.push_contact_sync();
    }

    /// Clears a group's messages on this device (members keep theirs).
    pub fn clear_group_conversation(&self, group: [u8; 16]) {
        self.clear_before(ConversationId::Group(group), now_ms());
    }

    pub(crate) fn clear_before(&self, conv: ConversationId, t: u64) {
        let _ = self
            .shared
            .home
            .delete_entries(self.identity_ref(), conv, now_ms(), |e| e.at_ms <= t);
    }

    /// When the user last cleared the conversation `conv` (0 if never).
    pub(crate) fn cleared_at(&self, conv: ConversationId) -> u64 {
        let ConversationId::Peer(id) = conv else {
            return 0;
        };
        lock(&self.shared.contacts)
            .iter()
            .filter(|c| *c.key.as_bytes() == id || c.account.is_some_and(|a| a.0 == id))
            .map(|c| c.cleared_ms)
            .max()
            .unwrap_or(0)
    }

    /// Applies `f` to `peer` and the other devices of its account; returns them.
    fn mark_contacts(
        &self,
        peer: &PublicIdentity,
        f: impl Fn(&mut threnody_core::store::Contact),
    ) -> Vec<PublicIdentity> {
        let account = lock(&self.shared.contacts)
            .get(peer)
            .and_then(|c| c.account);
        self.update_contacts(|cs| {
            let keys: Vec<PublicIdentity> = cs
                .iter()
                .filter(|c| c.key == *peer || (account.is_some() && c.account == account))
                .map(|c| c.key)
                .collect();
            for k in &keys {
                if let Some(c) = cs.get_mut(k) {
                    f(c);
                }
            }
            keys
        })
    }

    /// The conversation a peer device belongs to: its account if known,
    /// otherwise the device itself.
    pub fn conversation_for(&self, peer: &PublicIdentity) -> ConversationId {
        if self.is_own_device(peer) {
            return ConversationId::Peer(*peer.as_bytes());
        }
        // The account chain if we've seen it, else the account our contact
        // book records (synced from our other devices, say).
        let account = self.account_of(peer).map(|a| a.id().0).or_else(|| {
            lock(&self.shared.contacts)
                .get(peer)
                .and_then(|c| c.account)
                .map(|a| a.0)
        });
        ConversationId::Peer(account.unwrap_or(*peer.as_bytes()))
    }

    /// Sends `body` to every device of `peer`'s account (live where
    /// connected, sealed for mailboxes otherwise), applying and recording
    /// the conversation's disappearing-message timer.
    pub fn send_text(&self, peer: &PublicIdentity, body: &str) -> Result<SendReport> {
        self.send_reply(peer, body, None)
    }

    /// A quote of the message in `conv` that `pick` chooses (the newest
    /// such), to reply to it with; `None` if there's none.
    pub fn quote_of(&self, conv: ConversationId, pick: impl Fn(&Entry) -> bool) -> Option<Quote> {
        let h = self.history(conv).ok()?;
        h.entries().iter().rev().find(|e| pick(e)).map(Quote::of)
    }

    /// [`Node::send_text`], replying to (quoting) an earlier message.
    pub fn send_reply(
        &self,
        peer: &PublicIdentity,
        body: &str,
        quote: Option<Quote>,
    ) -> Result<SendReport> {
        self.send_message(peer, body, quote, false)
    }

    /// [`Node::send_text`], quoting `quote`, and marked `forwarded`.
    pub fn send_message(
        &self,
        peer: &PublicIdentity,
        body: &str,
        quote: Option<Quote>,
        forwarded: bool,
    ) -> Result<SendReport> {
        // Writing to someone accepts them.
        self.accept_contact(peer);
        let conv = self.conversation_for(peer);
        let timer = self.effective_timer(conv);
        // The message carries its history id, so we can delete it later.
        let local_id = local_id();
        let msg = AppMessage::Text {
            sent_ms: now_ms(),
            body: body.to_owned(),
            expires_in_s: self.announced_timer(conv),
            id: local_id,
            quote: quote.clone(),
            forwarded,
        };
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) if !self.is_own_device(peer) => {
                a.state().devices.iter().map(|(d, _)| *d).collect()
            }
            _ => vec![*peer],
        };
        let mut r = SendReport::default();
        for d in &devices {
            let tag = Tag {
                local_id,
                ..Tag::NONE
            };
            if self.send_tagged(d, msg.clone(), tag).is_ok() {
                r.live += 1;
                continue;
            }
            // Try to reach it directly while the message waits.
            self.seek(d);
            // Should every mailbox decline it, it waits here after all.
            if self.can_send_offline(d) && self.send_offline_or_hold(d, &msg, tag).is_ok() {
                r.sealed += 1;
            } else {
                self.hold(d, msg.clone(), tag);
                r.queued += 1;
            }
        }
        let now = now_ms();
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing: true,
                device: *self.identity().as_bytes(),
                text: body.to_owned(),
                offline: false,
                expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
                file: None,
                local_id,
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id: 0,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
                quote,
                mentions: Vec::new(),
                poll: None,
                forwarded,
                list: None,
                ticked_ms: 0,
            },
        );
        Ok(r)
    }

    /// Makes file contents ready to send: images lose their metadata
    /// unless that is switched off, and the size limit is checked. A photo
    /// whose metadata can't be removed (HEIC, AVIF, TIFF) is refused rather
    /// than sent with it; apps convert those to JPEG first.
    pub fn prepare_file(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        use threnody_core::media::{Stripped, needs_converting, strip};
        let data = if self.strip_metadata() {
            match strip(data)? {
                Stripped::Unsupported(d) if needs_converting(&d) => {
                    return Err(NetError::NotAllowed(
                        "this photo's location and camera details can't be removed: \
                         send it as a JPEG, or turn metadata removal off"
                            .into(),
                    ));
                }
                Stripped::Image(d) | Stripped::Unsupported(d) => d,
            }
        } else {
            data
        };
        if data.len() > threnody_core::message::MAX_FILE {
            return Err(NetError::Protocol(threnody_core::Error::Malformed(
                "file too large",
            )));
        }
        Ok(data)
    }

    /// Sends a file to `peer` (who must be reachable live: files aren't
    /// sealed for mailboxes) and records it in history, with
    /// `file.location`, where it lives on this device.
    pub fn send_file(&self, peer: &PublicIdentity, file: OutgoingFile) -> Result<()> {
        let OutgoingFile {
            name,
            data,
            location,
            sensitive,
            caption,
            album,
            clip,
            forwarded,
        } = file;
        let data = self.prepare_file(data)?;
        self.accept_contact(peer);
        let local_id = local_id();
        let size = data.len() as u64;
        let msg = AppMessage::File {
            sent_ms: now_ms(),
            name: name.clone(),
            data,
            id: local_id,
            sensitive,
            caption: caption.clone(),
            album,
            clip,
            forwarded,
        };
        let tag = Tag {
            local_id,
            ..Tag::NONE
        };
        if let Err(e) = self.send_tagged(peer, msg.clone(), tag) {
            if !matches!(e, NetError::Closed) {
                return Err(e);
            }
            // No session yet: it goes once one comes up.
            self.seek(peer);
            self.hold(peer, msg, tag);
        }
        self.append_file(
            peer,
            true,
            FileNote {
                name,
                size,
                location,
                sensitive,
                album,
                clip,
                forwarded,
            },
            &caption,
            local_id,
            0,
        );
        Ok(())
    }

    /// Records `peer`'s acknowledgement of the entry `tag` names, and
    /// tells the app if that changed anything. For a message forwarded on
    /// another member's behalf, only tells the app (to send a receipt).
    pub fn mark_delivered(&self, peer: &PublicIdentity, tag: Tag) {
        if let Some(origin) = tag.relay_for {
            if let Ok(origin) = PublicIdentity::from_bytes(&origin) {
                self.emit(Event::Delivered {
                    peer: *peer,
                    local_id: tag.local_id,
                    group: tag.group,
                    relay_for: Some(origin),
                });
            }
            return;
        }
        let conv = match tag.group {
            Some(g) => ConversationId::Group(g),
            None => self.conversation_for(peer),
        };
        if let Ok(true) = self.shared.home.mark_delivered(
            self.identity_ref(),
            conv,
            tag.local_id,
            *peer.as_bytes(),
            now_ms(),
        ) {
            self.emit(Event::Delivered {
                peer: *peer,
                local_id: tag.local_id,
                group: tag.group,
                relay_for: None,
            });
        }
    }

    /// Appends an entry to a conversation's history.
    /// Our own outgoing 1:1 entries also go to our other devices.
    pub fn append(&self, conv: ConversationId, entry: Entry) {
        if !entry.outgoing {
            self.unarchive(conv);
        }
        let now = entry.at_ms;
        let ours = entry.outgoing && entry.device == *self.identity().as_bytes();
        let pushed = ours.then(|| entry.clone());
        let _ = self
            .shared
            .home
            .append_history(self.identity_ref(), conv, entry, now);
        if let Some(e) = pushed {
            self.push_entry(conv, &e);
        }
    }

    /// Sets the disappearing-message timer for the conversation with
    /// `peer` (`None` turns it off). Applies to messages sent from now on.
    pub fn set_timer(&self, peer: &PublicIdentity, secs: Option<u32>) -> Result<()> {
        self.set_conversation_timer(self.conversation_for(peer), secs)
    }

    /// [`Node::set_timer`] for any conversation (groups too: their timer
    /// applies on this device).
    pub fn set_conversation_timer(&self, conv: ConversationId, secs: Option<u32>) -> Result<()> {
        let _lock = threnody_core::history::history_lock();
        let mut h = self.history(conv)?;
        // 0 records "off on purpose", unlike no setting (the default).
        h.timer_s = Some(secs.unwrap_or(0));
        self.shared
            .home
            .save_history(self.identity_ref(), conv, &h)
            .map_err(NetError::from)
    }

    /// The timer our messages in `conv` carry: the effective one, or
    /// `Some(0)` when the user turned it off on purpose (as opposed to
    /// never choosing one), so the peer can tell the two apart.
    fn announced_timer(&self, conv: ConversationId) -> Option<u32> {
        match self.history(conv).ok().and_then(|h| h.timer_s) {
            Some(0) => Some(0),
            _ => self.effective_timer(conv),
        }
    }

    /// The timer messages in `peer`'s conversation get now.
    pub fn timer(&self, peer: &PublicIdentity) -> Option<u32> {
        self.effective_timer(self.conversation_for(peer))
    }

    /// A conversation's own setting, else the default
    /// ([`Node::set_default_timer`]).
    pub fn effective_timer(&self, conv: ConversationId) -> Option<u32> {
        match self.history(conv).ok().and_then(|h| h.timer_s) {
            Some(0) => None,
            Some(t) => Some(t),
            None => self.default_timer(),
        }
    }

    /// The disappearing timer for conversations that haven't set one: off
    /// unless the user chooses one (the user decided messages stay by
    /// default; every timer remains available per chat and as a default).
    pub fn default_timer(&self) -> Option<u32> {
        match self
            .shared
            .default_timer
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            0 => None,
            t => Some(t),
        }
    }

    pub fn set_default_timer(&self, secs: Option<u32>) {
        self.shared
            .default_timer
            .store(secs.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
    }

    pub fn history(&self, conv: ConversationId) -> Result<History> {
        self.shared
            .home
            .load_history(self.identity_ref(), conv, now_ms())
            .map_err(NetError::from)
    }

    /// Searches one conversation, or all of them (`None`), for `query`;
    /// at most `limit` hits, newest first.
    pub fn search(
        &self,
        scope: Option<ConversationId>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        self.shared
            .home
            .search_history(
                self.identity_ref(),
                scope,
                &Query::new(query),
                now_ms(),
                limit,
            )
            .map_err(NetError::from)
    }

    /// Appends one entry; `timer` is the message's own expiry in seconds.
    pub fn record(
        &self,
        conv: ConversationId,
        device: [u8; 32],
        outgoing: bool,
        text: &str,
        offline: bool,
        timer: Option<u32>,
    ) {
        self.record_with_id(
            conv,
            device,
            outgoing,
            text,
            offline,
            timer,
            0,
            Extras::default(),
        );
    }

    /// [`Node::record`] for an incoming message with the sender's id.
    /// False if it couldn't be stored.
    #[allow(clippy::too_many_arguments)]
    pub fn record_with_id(
        &self,
        conv: ConversationId,
        device: [u8; 32],
        outgoing: bool,
        text: &str,
        offline: bool,
        timer: Option<u32>,
        remote_id: u64,
        extras: Extras,
    ) -> bool {
        if !outgoing {
            self.unarchive(conv);
        }
        let now = now_ms();
        let entry = Entry {
            at_ms: now,
            outgoing,
            device,
            text: text.to_owned(),
            offline,
            expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
            file: None,
            local_id: 0,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
            remote_id,
            edited_ms: 0,
            read_ms: 0,
            reactions: Vec::new(),
            quote: extras.quote,
            mentions: extras.mentions,
            poll: None,
            forwarded: extras.forwarded,
            list: None,
            ticked_ms: 0,
        };
        if let Err(e) = self
            .shared
            .home
            .append_history(self.identity_ref(), conv, entry, now)
        {
            eprintln!("threnody: failed to save history: {e}");
            return false;
        }
        true
    }

    /// Records a file received from (or sent to) `peer`'s conversation,
    /// with where the app saved it. It follows the conversation's
    /// disappearing-message timer. Sending through [`Node::send_file`]
    /// records it already.
    pub fn record_file(&self, peer: &PublicIdentity, outgoing: bool, file: FileNote) {
        self.append_file(peer, outgoing, file, "", 0, 0);
    }

    /// Records a received file, with its caption and the sender's id for
    /// it (from the `File` message, so a later delete-for-everyone can
    /// find it).
    pub fn record_received_file(
        &self,
        peer: &PublicIdentity,
        file: FileNote,
        caption: &str,
        remote_id: u64,
    ) {
        self.append_file(peer, false, file, caption, 0, remote_id);
    }

    fn append_file(
        &self,
        peer: &PublicIdentity,
        outgoing: bool,
        file: FileNote,
        caption: &str,
        local_id: u64,
        remote_id: u64,
    ) {
        let conv = self.conversation_for(peer);
        let timer = self.effective_timer(conv);
        let now = now_ms();
        let device = if outgoing {
            *self.identity().as_bytes()
        } else {
            *peer.as_bytes()
        };
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing,
                device,
                text: caption.to_owned(),
                offline: false,
                expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
                file: Some(file),
                local_id,
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
                quote: None,
                mentions: Vec::new(),
                poll: None,
                forwarded: false,
                list: None,
                ticked_ms: 0,
            },
        );
    }

    /// Records an incoming text, adopting the sender's timer setting.
    /// Says what became of it: a copy of a text already recorded (one may
    /// come both from its sender and from a mailbox, when the sender
    /// stopped waiting for the mailbox's receipt) shouldn't show again, and
    /// one that couldn't be stored shouldn't be acknowledged. Other
    /// messages need nothing here.
    pub(crate) fn record_incoming(
        &self,
        from: &PublicIdentity,
        msg: &AppMessage,
        offline: bool,
    ) -> Recorded {
        let AppMessage::Text {
            body,
            expires_in_s,
            id,
            quote,
            forwarded,
            ..
        } = msg
        else {
            return Recorded::New;
        };
        let conv = self.conversation_for(from);
        // Held until it is recorded, so two copies can't both pass.
        let lock = threnody_core::history::history_lock();
        if *id != 0
            && self.history(conv).is_ok_and(|h| {
                h.entries()
                    .iter()
                    .any(|e| !e.outgoing && e.device == *from.as_bytes() && e.remote_id == *id)
            })
        {
            return Recorded::Duplicate;
        }
        // `Some(0)`: turned off on purpose. No timer at all from a peer that
        // would have said so means it never chose one: ours stands, and
        // it adopts ours from our next message. From an older peer it may
        // mean either, and is taken as off, as such peers intend.
        let chosen = expires_in_s.is_some() || !self.known_to_support(from, FEATURE_TIMER_OFF);
        // A message with no timer of its own goes when ours says: in a
        // disappearing chat, it must not stay for good.
        let expiry = if chosen {
            expires_in_s.filter(|&s| s > 0)
        } else {
            self.effective_timer(conv)
        };
        if chosen
            && self.effective_timer(conv) != expiry
            && let Ok(mut h) = self.history(conv)
        {
            h.timer_s = Some(expiry.unwrap_or(0));
            let _ = self.shared.home.save_history(self.identity_ref(), conv, &h);
            self.emit(Event::TimerChanged {
                peer: *from,
                secs: expiry,
            });
        }
        let stored = self.record_with_id(
            conv,
            *from.as_bytes(),
            false,
            body,
            offline,
            expiry,
            *id,
            Extras {
                quote: quote.clone(),
                forwarded: *forwarded,
                ..Extras::default()
            },
        );
        drop(lock);
        if stored {
            Recorded::New
        } else {
            Recorded::Failed
        }
    }

    /// Starts the background sweep of expired history (and of what is
    /// left of it waiting for a peer), and of sealed messages we have held
    /// too long for others.
    pub(crate) fn start_history_sweep(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        node.shared.home.sweep_history(node.identity_ref(), now_ms());
                        node.seal_expired_unacked(now_ms());
                        node.sweep_mailbox(now_ms());
                        node.shared.expire_saved_tunnels(now_ms());
                    }
                    () = &mut closed => break,
                }
            }
        });
    }
}
