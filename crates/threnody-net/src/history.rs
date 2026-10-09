//! Sending text to accounts, and recording history (spec §6.1, §6.4).

use std::time::Duration;

use threnody_core::history::{ConversationId, Entry, FileNote, History, Hit, Query};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use threnody_core::message::{
    Clip, FEATURE_DELETE, FEATURE_EDIT, FEATURE_REACT, FEATURE_READ, FEATURE_TYPING,
};

use crate::delivery::Tag;
use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// A week: the usual choice when the user turns disappearing messages on.
pub const WEEK_S: u32 = 7 * 24 * 3600;

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
}

/// A fresh album id for files sent together.
pub fn album_id() -> u64 {
    local_id()
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
        // Our other devices delete the same, whatever they are.
        for s in self.sessions() {
            if self.is_own_device(&s.peer) && self.supports(&s.peer, FEATURE_DELETE) {
                let _ = self.send_tracked(&s.peer, msg(ids.to_vec()));
            }
        }
        if everyone && !ours.is_empty() {
            let devices: Vec<PublicIdentity> = match self.account_of(peer) {
                Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
                None => vec![*peer],
            };
            for d in devices {
                if !self.supports(&d, FEATURE_DELETE) {
                    continue;
                }
                if self.send_tracked(&d, msg(ours.clone())).is_err() && self.can_send_offline(&d) {
                    let _ = self.send_offline(&d, &msg(ours.clone()));
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
        let mut targets: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .map(|s| s.peer)
            .filter(|p| self.is_own_device(p))
            .collect();
        match self.account_of(peer) {
            Some(a) => targets.extend(a.state().devices.iter().map(|(d, _)| *d)),
            None => targets.push(*peer),
        }
        targets.sort_unstable_by_key(|p| *p.as_bytes());
        targets.dedup();
        for d in targets {
            if d == self.identity() || !self.supports(&d, FEATURE_EDIT) {
                continue;
            }
            if self.send_tracked(&d, msg.clone()).is_err() && self.can_send_offline(&d) {
                let _ = self.send_offline(&d, &msg);
            }
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
        let mut targets: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .map(|s| s.peer)
            .filter(|p| self.is_own_device(p))
            .collect();
        match self.account_of(peer) {
            Some(a) => targets.extend(a.state().devices.iter().map(|(d, _)| *d)),
            None => targets.push(*peer),
        }
        targets.sort_unstable_by_key(|p| *p.as_bytes());
        targets.dedup();
        for d in targets {
            if d == self.identity() || !self.supports(&d, FEATURE_REACT) {
                continue;
            }
            if self.send_tracked(&d, msg.clone()).is_err() && self.can_send_offline(&d) {
                let _ = self.send_offline(&d, &msg);
            }
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

    /// Tells `peer`'s account devices we've displayed the messages it
    /// sent us with the sender-ids `ids` (their `remote_id`s in our
    /// conversation). Live-only: never sealed or tracked.
    pub fn report_read(&self, peer: &PublicIdentity, ids: &[u64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let conv = self.conversation_for(peer);
        let msg = AppMessage::Read {
            conversation: conv.to_bytes(),
            ids: ids.to_vec(),
        };
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) if !self.is_own_device(peer) => {
                a.state().devices.iter().map(|(d, _)| *d).collect()
            }
            _ => vec![*peer],
        };
        let mut reached = false;
        for d in &devices {
            if !self.supports(d, FEATURE_READ) {
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
                "{} (no session with a read-receipt-capable peer)",
                peer.fingerprint()
            )))
        }
    }

    /// Handles a `Read` from `from`: it displayed our outgoing messages
    /// `ids` in our conversation with it; their "seen" ticks update.
    pub(crate) fn on_read(&self, from: &PublicIdentity, conversation: &[u8], ids: &[u64]) {
        if self.is_own_device(from) {
            return;
        }
        let expected = self.conversation_for(from);
        let ok = match ConversationId::from_bytes(conversation) {
            Some(c) => c == expected,
            None => false,
        };
        if !ok {
            return;
        }
        let now = now_ms();
        let n = self
            .shared
            .home
            .mark_read_entries(self.identity_ref(), expected, now, |e| {
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
        self.mark_contacts(peer, |c| c.accepted = true);
    }

    /// Blocks `peer` and every device of its account: their sessions end
    /// and are refused, and our conversation with them is deleted.
    pub fn block_contact(&self, peer: &PublicIdentity) {
        let conv = self.conversation_for(peer);
        let devices = self.mark_contacts(peer, |c| {
            c.blocked = true;
            c.accepted = false;
            c.local_approved = false;
        });
        for d in devices {
            self.disconnect(&d);
        }
        let _ = self.shared.home.delete_history(conv);
    }

    /// Deletes a message request: the contact and the conversation go, and
    /// they can write again (as a new request).
    pub fn delete_request(&self, peer: &PublicIdentity) {
        let conv = self.conversation_for(peer);
        let devices = self.mark_contacts(peer, |_| {});
        self.update_contacts(|c| {
            for d in &devices {
                if c.get(d).is_some_and(|x| !x.accepted) {
                    c.remove(d);
                }
            }
        });
        for d in devices {
            self.disconnect(&d);
        }
        let _ = self.shared.home.delete_history(conv);
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
            self.disconnect(d);
        }
        let _ = self.shared.home.delete_history(conv);
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
        // Writing to someone accepts them.
        self.accept_contact(peer);
        let conv = self.conversation_for(peer);
        let timer = self.effective_timer(conv);
        // The message carries its history id, so we can delete it later.
        let local_id = local_id();
        let msg = AppMessage::Text {
            sent_ms: now_ms(),
            body: body.to_owned(),
            expires_in_s: timer,
            id: local_id,
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
            if self.can_send_offline(d) && self.send_offline(d, &msg).is_ok() {
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
            },
        );
        Ok(r)
    }

    /// Makes file contents ready to send: images lose their metadata
    /// unless that is switched off, and the size limit is checked.
    pub fn prepare_file(&self, data: Vec<u8>) -> Result<Vec<u8>> {
        let data = if self.strip_metadata() {
            match threnody_core::media::strip(data)? {
                threnody_core::media::Stripped::Image(d)
                | threnody_core::media::Stripped::Unsupported(d) => d,
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
        let mut h = self.history(conv)?;
        // 0 records "off on purpose", unlike no setting (the default).
        h.timer_s = Some(secs.unwrap_or(0));
        self.shared
            .home
            .save_history(self.identity_ref(), conv, &h)
            .map_err(NetError::from)
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
        self.record_with_id(conv, device, outgoing, text, offline, timer, 0);
    }

    /// [`Node::record`] for an incoming message with the sender's id.
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
    ) {
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
        };
        let _ = self
            .shared
            .home
            .append_history(self.identity_ref(), conv, entry, now);
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
            },
        );
    }

    /// Records an incoming text, adopting the sender's timer setting.
    pub(crate) fn record_incoming(&self, from: &PublicIdentity, msg: &AppMessage, offline: bool) {
        let AppMessage::Text {
            body,
            expires_in_s,
            id,
            ..
        } = msg
        else {
            return;
        };
        let conv = self.conversation_for(from);
        if self.effective_timer(conv) != *expires_in_s
            && let Ok(mut h) = self.history(conv)
        {
            h.timer_s = Some(expires_in_s.unwrap_or(0));
            let _ = self.shared.home.save_history(self.identity_ref(), conv, &h);
            self.emit(Event::TimerChanged {
                peer: *from,
                secs: *expires_in_s,
            });
        }
        self.record_with_id(
            conv,
            *from.as_bytes(),
            false,
            body,
            offline,
            *expires_in_s,
            *id,
        );
    }

    /// Starts the background sweep of expired history.
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
                    }
                    () = &mut closed => break,
                }
            }
        });
    }
}
