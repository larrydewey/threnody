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

/// How many bytes of stored lists changes from others may fill, of the
/// [`cbor::MAX_ENCODED`] they can take: the rest is kept for our own.
const MAX_FROM_OTHERS: usize = 12 << 20;
/// How many bytes of the lists shared with one chat its changes may fill,
/// and how many lists it may start here, so that one contact or group
/// can't take the room of every other.
const MAX_CHAT_BYTES: usize = 3 << 20;
const MAX_CHAT_LISTS: usize = 50;
/// List slots only lists we start may fill.
const OWN_LISTS: usize = 50;

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
    /// Whether `conv` may start another list here.
    fn room_for(&self, conv: ConversationId) -> bool {
        self.lists.len() < MAX_LISTS - OWN_LISTS
            && self
                .lists
                .iter()
                .filter(|l| l.shared.contains(&conv))
                .count()
                < MAX_CHAT_LISTS
    }

    /// Whether the lists, after a change from `conv`, are within what
    /// others' changes may fill: in all, and of those shared with `conv`.
    fn fits_from(&self, conv: ConversationId) -> bool {
        let (mut all, mut chat) = (0, 0);
        for l in &self.lists {
            let n = l.list.encode().map_or(usize::MAX, |b| b.len()) + l.shared.len() * 40 + 8;
            all += n;
            if l.shared.contains(&conv) {
                chat += n;
            }
        }
        all <= MAX_FROM_OTHERS && chat <= MAX_CHAT_BYTES
    }

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
    /// Saves the lists; false if they no longer fit, and then the caller
    /// undoes its change, so that what we show is what we keep.
    fn save_lists(&self, l: &Lists) -> bool {
        match l.encode() {
            Ok(b) => {
                self.shared.save_state(STATE, Ok(b));
                true
            }
            Err(e) => {
                eprintln!("threnody: lists too large to save: {e}");
                false
            }
        }
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

    /// Starts a list; returns its id, or None if there are too many (or
    /// they're too large) to keep another.
    pub fn create_list(&self, title: &str) -> Option<ListId> {
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
            if ls.lists.len() >= MAX_LISTS {
                return None;
            }
            ls.lists.push(sl.clone());
            if !self.save_lists(&ls) {
                ls.lists.pop();
                return None;
            }
        }
        self.push_list(&sl, false);
        Some(id)
    }

    /// Changes list `id` with `f` (given the stamp to use); if it changed
    /// anything, keeps it and sends it to our devices and the contacts it's
    /// shared with. Returns the list and the groups it's shared with, which
    /// the caller sends it to (inside MLS); None if nothing changed, or the
    /// change would make the lists too large to keep.
    pub fn edit_list(
        &self,
        id: &ListId,
        f: impl FnOnce(&mut List, Stamp) -> bool,
    ) -> Option<(Vec<u8>, Vec<[u8; 16]>)> {
        let stamp = self.list_stamp();
        let sl = {
            let mut ls = lock(&self.shared.lists);
            let k = ls.lists.iter().position(|l| l.list.id == *id)?;
            let before = ls.lists[k].list.clone();
            if !f(&mut ls.lists[k].list, stamp) {
                return None;
            }
            if !self.save_lists(&ls) {
                ls.lists[k].list = before;
                return None;
            }
            ls.lists[k].clone()
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
            let k = ls.lists.iter().position(|l| l.list.id == *id)?;
            if !ls.lists[k].shared.contains(&conv) {
                ls.lists[k].shared.push(conv);
                if !self.save_lists(&ls) {
                    ls.lists[k].shared.pop();
                    return None;
                }
            }
            ls.lists[k].clone()
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

    /// The devices of the contact a 1:1 conversation is with; none if
    /// we blocked them.
    fn conversation_devices(&self, conv: ConversationId) -> Vec<PublicIdentity> {
        let contacts = self.contacts();
        let blocked = |k: &PublicIdentity| contacts.get(k).is_some_and(|c| c.blocked);
        let Some(k) = contacts
            .iter()
            .map(|c| c.key)
            .find(|k| self.conversation_for(k) == conv)
        else {
            return Vec::new();
        };
        if blocked(&k) {
            return Vec::new();
        }
        match self.account_of(&k) {
            Some(a) => a
                .state()
                .devices
                .iter()
                .map(|(d, _)| *d)
                .filter(|d| !blocked(d))
                .collect(),
            None => vec![k],
        }
    }

    /// Stops sharing every list with `conv`, a contact we blocked, on all
    /// our devices.
    pub(crate) fn unshare_lists_with(&self, conv: ConversationId) {
        let changed: Vec<SharedList> = {
            let mut ls = lock(&self.shared.lists);
            let mut changed = Vec::new();
            for l in &mut ls.lists {
                if l.shared.contains(&conv) {
                    l.shared.retain(|c| *c != conv);
                    changed.push(l.clone());
                }
            }
            if !changed.is_empty() {
                self.save_lists(&ls);
            }
            changed
        };
        for sl in &changed {
            self.push_list(sl, false);
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
        let Some(mut list) = ok
            .ok()
            .and(required(l, "list").ok())
            .and_then(|b| List::decode(&b).ok())
        else {
            return;
        };
        list.drop_future(now_ms());
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
                    let before = ls.lists[k].clone();
                    let x = &mut ls.lists[k];
                    let mut c = x.list.merge(&list);
                    if x.shared != shared {
                        x.shared = shared;
                        c = true;
                    }
                    if c && !self.save_lists(&ls) {
                        ls.lists[k] = before;
                        return;
                    }
                    c
                }
                (None, false) if ls.lists.len() < MAX_LISTS => {
                    ls.lists.push(SharedList { list, shared });
                    if !self.save_lists(&ls) {
                        ls.lists.pop();
                        return;
                    }
                    true
                }
                (None, false) => false,
            };
            if changed && deleted {
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
        let mut list = List::decode(bytes).ok()?;
        list.drop_future(now_ms());
        let id = list.id;
        // What it changes is kept only within what `conv` may fill (see
        // `MAX_CHAT_BYTES`), and undone otherwise.
        let new = {
            let mut ls = lock(&self.shared.lists);
            let at = ls.lists.iter().position(|x| x.list.id == id);
            match at {
                // Only from those it's shared with.
                Some(k) if ls.lists[k].shared.contains(&conv) => {
                    let before = ls.lists[k].list.clone();
                    if !ls.lists[k].list.merge(&list) {
                        return None;
                    }
                    if !ls.fits_from(conv) || !self.save_lists(&ls) {
                        ls.lists[k].list = before;
                        return None;
                    }
                    false
                }
                Some(_) => return None,
                None if ls.room_for(conv) => {
                    ls.lists.push(SharedList {
                        list: list.clone(),
                        shared: vec![conv],
                    });
                    if !ls.fits_from(conv) || !self.save_lists(&ls) {
                        ls.lists.pop();
                        return None;
                    }
                    true
                }
                None => return None,
            }
        };
        if new {
            self.note_list(conv, *from.as_bytes(), false, &list);
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
    use threnody_core::Identity;
    use threnody_core::store::Home;

    #[tokio::test]
    async fn nothing_goes_to_a_blocked_contact() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("alice"));
        let identity = home.create_identity(None).unwrap();
        let (node, _rx) = Node::new(crate::NodeConfig {
            home,
            identity,
            policy: crate::AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap();
        let bob = Identity::generate().public();
        node.accept_contact(&bob);
        let conv = node.conversation_for(&bob);
        assert_eq!(node.conversation_devices(conv), [bob]);
        let id = node.create_list("Gift ideas").unwrap();
        node.share_list(&id, conv);

        node.block_contact(&bob);
        assert!(node.conversation_devices(conv).is_empty());
        assert!(node.list(&id).unwrap().shared.is_empty());
    }

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
