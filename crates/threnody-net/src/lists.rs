//! Lists on a running node (see `threnody_core::lists`): kept encrypted,
//! on all our own devices, and shared with conversations.
//!
//! - Our devices exchange whole lists (account message `List`), with who
//!   each is shared with, or that it was deleted.
//! - A list shared with a contact goes to their devices as
//!   `AppMessage::List`; one shared with a group goes inside MLS, which
//!   the caller does (see [`Node::edit_list`]).
//! - Whoever receives a list merges it; a list new to us, from a contact,
//!   arrives shared with our chat with them, and a history entry there
//!   says so.
//!
//! Changes never echo: a device only sends what it changed itself.

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::history::{ConversationId, Entry};
use threnody_core::lists::{List, ListId, MAX_LISTS, MAX_TITLE, Stamp};
use threnody_core::message::FEATURE_LISTS;
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::node::{Event, Node, lock};

/// Where lists are kept (encrypted).
pub(crate) const STATE: &str = "lists";

/// A list with the conversations it's shared with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SharedList {
    pub list: List,
    pub shared: Vec<ConversationId>,
}

#[derive(Default)]
pub(crate) struct Lists {
    pub lists: Vec<SharedList>,
    /// Changes from contacts that groups the list is shared with should
    /// hear of; the app sends them (see [`Node::take_list_forwards`]).
    pub to_groups: Vec<(Vec<u8>, [u8; 16])>,
}

impl Lists {
    pub fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let parts: Vec<(Vec<u8>, &[ConversationId])> = self
            .lists
            .iter()
            .filter_map(|l| Some((l.list.encode().ok()?, l.shared.as_slice())))
            .collect();
        let hint = 64
            + parts
                .iter()
                .map(|(b, s)| b.len() + s.len() * 40 + 8)
                .sum::<usize>();
        cbor::to_vec(hint, |e| {
            e.map_len(2)?.u8(0)?.u8(1)?;
            e.u8(1)?.array_len(parts.len())?;
            for (b, shared) in &parts {
                e.array_len(2)?.bytes(b)?.array_len(shared.len())?;
                for c in *shared {
                    e.bytes(&c.to_bytes())?;
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Self {
        let mut out = Self::default();
        let mut dec = Decoder::new(b);
        let _ = read_map(&mut dec, |k, d| {
            match k {
                0 => {
                    d.u8()?;
                }
                1 => {
                    for _ in 0..d.array_len()? {
                        let (l, shared) = decode_shared(d)?;
                        if let Ok(list) = List::decode(&l)
                            && out.lists.len() < MAX_LISTS
                        {
                            out.lists.push(SharedList { list, shared });
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        });
        out
    }
}

fn decode_shared(d: &mut Decoder<'_>) -> threnody_core::Result<(Vec<u8>, Vec<ConversationId>)> {
    if d.array_len()? != 2 {
        return Err(threnody_core::Error::Malformed("shared list"));
    }
    let l = d.bytes()?.to_vec();
    let mut shared = Vec::new();
    for _ in 0..d.array_len()? {
        if let Some(c) = ConversationId::from_bytes(d.bytes()?)
            && shared.len() < 64
        {
            shared.push(c);
        }
    }
    Ok((l, shared))
}

/// Account message `List`: `{ 0: list, 1: [* conversation], ? 2: true
/// (deleted) }`.
fn sync_payload(l: &SharedList, deleted: bool) -> Option<Vec<u8>> {
    let b = l.list.encode().ok()?;
    cbor::to_vec(b.len() + l.shared.len() * 40 + 32, |e| {
        e.map_len(2 + usize::from(deleted))?;
        e.u8(0)?.bytes(&b)?;
        e.u8(1)?.array_len(l.shared.len())?;
        for c in &l.shared {
            e.bytes(&c.to_bytes())?;
        }
        if deleted {
            e.u8(2)?.bool(true)?;
        }
        Ok(())
    })
    .ok()
}

impl Node {
    fn save_lists(&self, l: &Lists) {
        self.shared.save_state(STATE, l.encode());
    }

    /// Who changed it: our account, so our devices count as one.
    fn list_stamp(&self) -> Stamp {
        Stamp {
            ms: now_ms(),
            by: self.reactor_of(&self.identity()),
        }
    }

    /// Our lists, with whom each is shared.
    pub fn lists(&self) -> Vec<SharedList> {
        lock(&self.shared.lists).lists.clone()
    }

    pub fn list(&self, id: &ListId) -> Option<SharedList> {
        lock(&self.shared.lists)
            .lists
            .iter()
            .find(|l| l.list.id == *id)
            .cloned()
    }

    /// Starts a list; returns its id.
    pub fn create_list(&self, title: &str) -> ListId {
        let id: ListId = threnody_core::crypto::random_bytes();
        let stamp = self.list_stamp();
        let sl = SharedList {
            list: List {
                id,
                title: title.trim().chars().take(MAX_TITLE).collect(),
                title_stamp: stamp,
                items: Vec::new(),
            },
            shared: Vec::new(),
        };
        {
            let mut ls = lock(&self.shared.lists);
            ls.lists.push(sl.clone());
            self.save_lists(&ls);
        }
        self.push_list(&sl, false);
        id
    }

    /// Changes list `id` with `f` (given the stamp to use); if it changed
    /// anything, keeps it and sends it to our devices and the contacts it's
    /// shared with. Returns the list and the groups it's shared with, which
    /// the caller sends it to (inside MLS); None if nothing changed.
    pub fn edit_list(
        &self,
        id: &ListId,
        f: impl FnOnce(&mut List, Stamp) -> bool,
    ) -> Option<(Vec<u8>, Vec<[u8; 16]>)> {
        let stamp = self.list_stamp();
        let sl = {
            let mut ls = lock(&self.shared.lists);
            let sl = ls.lists.iter_mut().find(|l| l.list.id == *id)?;
            if !f(&mut sl.list, stamp) {
                return None;
            }
            let sl = sl.clone();
            self.save_lists(&ls);
            sl
        };
        self.push_list(&sl, false);
        self.send_list_out(&sl, None)
    }

    /// Shares list `id` with `conv`, and notes it in that conversation's
    /// history. Returns what [`Node::edit_list`] does.
    pub fn share_list(
        &self,
        id: &ListId,
        conv: ConversationId,
    ) -> Option<(Vec<u8>, Vec<[u8; 16]>)> {
        let sl = {
            let mut ls = lock(&self.shared.lists);
            let sl = ls.lists.iter_mut().find(|l| l.list.id == *id)?;
            if !sl.shared.contains(&conv) {
                sl.shared.push(conv);
            }
            let sl = sl.clone();
            self.save_lists(&ls);
            sl
        };
        self.note_list(conv, *self.identity().as_bytes(), true, &sl.list);
        self.push_list(&sl, false);
        self.send_list_out(&sl, None)
    }

    /// Stops sharing list `id` with `conv` (they keep their copy).
    pub fn unshare_list(&self, id: &ListId, conv: ConversationId) {
        let sl = {
            let mut ls = lock(&self.shared.lists);
            let Some(sl) = ls.lists.iter_mut().find(|l| l.list.id == *id) else {
                return;
            };
            sl.shared.retain(|c| *c != conv);
            let sl = sl.clone();
            self.save_lists(&ls);
            sl
        };
        self.push_list(&sl, false);
    }

    /// Deletes list `id` on all our devices (others keep theirs).
    pub fn delete_list(&self, id: &ListId) {
        let gone = {
            let mut ls = lock(&self.shared.lists);
            let Some(k) = ls.lists.iter().position(|l| l.list.id == *id) else {
                return;
            };
            let gone = ls.lists.remove(k);
            self.save_lists(&ls);
            gone
        };
        self.push_list(&gone, true);
    }

    /// Records in `conv` that a list was shared there (by `device`).
    fn note_list(&self, conv: ConversationId, device: [u8; 32], outgoing: bool, l: &List) {
        let now = now_ms();
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing,
                device,
                text: format!("📋 {}", l.title),
                offline: false,
                expires_at_ms: None,
                file: None,
                local_id: crate::history::local_id(),
                delivered: outgoing,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id: 0,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
                quote: None,
                mentions: Vec::new(),
                poll: None,
                forwarded: false,
                list: Some(l.id),
            },
        );
    }

    /// Sends a list to our connected devices.
    fn push_list(&self, sl: &SharedList, deleted: bool) {
        let Some(m) = sync_payload(sl, deleted).and_then(crate::account::list_message) else {
            return;
        };
        for p in self.sessions().into_iter().map(|s| s.peer) {
            if self.is_own_device(&p) {
                let _ = self.send_tracked(&p, m.clone());
            }
        }
    }

    /// Our lists, for one of our devices whose session just started.
    pub(crate) fn lists_hello(&self) -> Vec<AppMessage> {
        lock(&self.shared.lists)
            .lists
            .iter()
            .filter_map(|l| sync_payload(l, false).and_then(crate::account::list_message))
            .collect()
    }

    /// Sends a list to the contacts it's shared with (but `except`);
    /// returns its bytes and the groups, for the caller.
    fn send_list_out(
        &self,
        sl: &SharedList,
        except: Option<ConversationId>,
    ) -> Option<(Vec<u8>, Vec<[u8; 16]>)> {
        let bytes = sl.list.encode().ok()?;
        let msg = AppMessage::List(bytes.clone());
        let mut groups = Vec::new();
        for c in sl.shared.iter().filter(|c| Some(**c) != except) {
            match c {
                ConversationId::Group(g) => groups.push(*g),
                ConversationId::Peer(_) if *c == self.notes() => {}
                ConversationId::Peer(_) => {
                    for d in self.conversation_devices(*c) {
                        if d == self.identity() || !self.supports(&d, FEATURE_LISTS) {
                            continue;
                        }
                        if self.send_tracked(&d, msg.clone()).is_err() && self.can_send_offline(&d)
                        {
                            let _ = self.send_offline(&d, &msg);
                        }
                    }
                }
            }
        }
        Some((bytes, groups))
    }

    /// The devices of the contact a 1:1 conversation is with.
    fn conversation_devices(&self, conv: ConversationId) -> Vec<PublicIdentity> {
        let keys: Vec<PublicIdentity> = self.contacts().iter().map(|c| c.key).collect();
        let Some(k) = keys.into_iter().find(|k| self.conversation_for(k) == conv) else {
            return Vec::new();
        };
        match self.account_of(&k) {
            Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
            None => vec![k],
        }
    }

    /// Lists to send to groups, for the app (after `Event::ListChanged`).
    pub fn take_list_forwards(&self) -> Vec<(Vec<u8>, [u8; 16])> {
        std::mem::take(&mut lock(&self.shared.lists).to_groups)
    }

    /// A list from our own device `from`: merged, or deleted.
    pub(crate) fn on_list_sync(&self, from: PublicIdentity, payload: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let mut dec = Decoder::new(payload);
        let (mut l, mut shared, mut deleted) = (None, Vec::new(), false);
        let ok = read_map(&mut dec, |k, d| {
            match k {
                0 => l = Some(d.bytes()?.to_vec()),
                1 => {
                    for _ in 0..d.array_len()? {
                        if let Some(c) = ConversationId::from_bytes(d.bytes()?) {
                            shared.push(c);
                        }
                    }
                }
                2 => deleted = d.bool()?,
                _ => return Ok(false),
            }
            Ok(true)
        })
        .and_then(|()| finish(&dec));
        let Some(list) = ok
            .ok()
            .and(required(l, "list").ok())
            .and_then(|b| List::decode(&b).ok())
        else {
            return;
        };
        let id = list.id;
        let changed = {
            let mut ls = lock(&self.shared.lists);
            let k = ls.lists.iter().position(|x| x.list.id == id);
            let changed = match (k, deleted) {
                (Some(k), true) => {
                    ls.lists.remove(k);
                    true
                }
                (None, true) => false,
                (Some(k), false) => {
                    let x = &mut ls.lists[k];
                    let mut c = x.list.merge(&list);
                    if x.shared != shared {
                        x.shared = shared;
                        c = true;
                    }
                    c
                }
                (None, false) if ls.lists.len() < MAX_LISTS => {
                    ls.lists.push(SharedList { list, shared });
                    true
                }
                (None, false) => false,
            };
            if changed {
                self.save_lists(&ls);
            }
            changed
        };
        if changed {
            self.emit(Event::ListChanged { id });
        }
    }

    /// A list shared in `conv` (by `from`, a contact or a group member):
    /// merged into ours, or, new to us, kept as shared there. What it
    /// changed goes on to the other chats it's shared with; returns the
    /// list and the groups among them, for the caller to send it to.
    pub fn on_shared_list(
        &self,
        conv: ConversationId,
        from: PublicIdentity,
        bytes: &[u8],
    ) -> Option<(Vec<u8>, Vec<[u8; 16]>)> {
        let list = List::decode(bytes).ok()?;
        let id = list.id;
        let (changed, new) = {
            let mut ls = lock(&self.shared.lists);
            let at = ls.lists.iter().position(|x| x.list.id == id);
            let room = ls.lists.len() < MAX_LISTS;
            let r = match at {
                // Only from those it's shared with.
                Some(k) if ls.lists[k].shared.contains(&conv) => {
                    (ls.lists[k].list.merge(&list), false)
                }
                Some(_) => (false, false),
                None if room => {
                    ls.lists.push(SharedList {
                        list: list.clone(),
                        shared: vec![conv],
                    });
                    (true, true)
                }
                None => (false, false),
            };
            if r.0 {
                self.save_lists(&ls);
            }
            r
        };
        if new {
            self.note_list(conv, *from.as_bytes(), false, &list);
        }
        if !changed {
            return None;
        }
        self.emit(Event::ListChanged { id });
        let sl = self.list(&id)?;
        self.send_list_out(&sl, Some(conv))
    }

    /// `AppMessage::List` from a contact: shared in our chat with them.
    pub(crate) fn on_list_message(&self, from: PublicIdentity, bytes: &[u8]) {
        if !self.is_accepted(&from) {
            return;
        }
        let conv = self.conversation_for(&from);
        // Groups it's also shared with hear of it from the app, which has
        // the group state.
        if let Some((b, groups)) = self.on_shared_list(conv, from, bytes) {
            let mut ls = lock(&self.shared.lists);
            for g in groups {
                ls.to_groups.push((b.clone(), g));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_lists_round_trip() {
        let l = Lists {
            lists: vec![SharedList {
                list: List {
                    id: [7; 16],
                    title: "Shopping".into(),
                    ..List::default()
                },
                shared: vec![
                    ConversationId::Group([1; 16]),
                    ConversationId::Peer([2; 32]),
                ],
            }],
            to_groups: Vec::new(),
        };
        assert_eq!(Lists::decode(&l.encode().unwrap()).lists, l.lists);
        assert!(Lists::decode(&[0xff]).lists.is_empty());
    }
}
