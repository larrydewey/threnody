//! End-to-end acknowledgements for user content (Appendix C, `Tracked`
//! and `Ack`).
//!
//! A message handed to a session is only queued: if the session dies
//! before the bytes reach the peer, they're gone. So text, files, group
//! and mailbox messages go out as `Tracked { id, inner }`, stay here until
//! the peer acknowledges the id, and are sent again first thing in the
//! next session with that peer. The receiver remembers recent ids and
//! delivers each message once. Both sides opt in through `Hello` feature
//! bits; with a peer that doesn't, messages go out plain as before.
//!
//! Both the unacknowledged messages (except files, which can be large)
//! and the recently delivered ids are kept in encrypted state, so a
//! restart in between loses or repeats nothing.

use std::collections::{HashMap, HashSet, VecDeque};

use threnody_core::crypto::random_bytes;
use threnody_core::message::{
    FEATURE_DELETE, FEATURE_EDIT, FEATURE_LISTS, FEATURE_PIN, FEATURE_REACT, FEATURE_RECEIPTS,
};
use threnody_core::{AppMessage, PublicIdentity};

/// Unacknowledged messages kept per peer, and as many changes to
/// conversations besides; the oldest of each are dropped first.
pub const MAX_UNACKED: usize = 256;
/// And at most this many bytes of them per peer.
pub const MAX_UNACKED_BYTES: usize = 32 * 1024 * 1024;
/// Delivered ids remembered per peer to drop resent copies.
pub const RECENT_IDS: usize = 4096;
/// And at most this many across all peers (strangers each add theirs):
/// the peers heard from longest ago are forgotten first, strangers before
/// accepted contacts.
pub const MAX_DELIVERED_IDS: usize = 64 * 1024;
/// Messages larger than this (files) are resent only within one run.
pub const MAX_PERSISTED: usize = 64 * 1024;
const VERSION: u8 = 4;

/// Whether a message is user content worth acknowledging and resending.
pub fn trackable(m: &AppMessage) -> bool {
    matches!(
        m,
        AppMessage::Text { .. }
            | AppMessage::File { .. }
            | AppMessage::Group(_)
            | AppMessage::Mailbox(_)
    )
}

/// The `Hello` feature bit a peer needs to read `m`, or 0. A change held
/// for a peer we haven't heard from since starting is dropped, not sent,
/// once its `Hello` shows it lacks this.
pub fn needs(m: &AppMessage) -> u64 {
    match m {
        AppMessage::Delete { .. } => FEATURE_DELETE,
        AppMessage::Edit { .. } => FEATURE_EDIT,
        AppMessage::React { .. } => FEATURE_REACT,
        AppMessage::Pin { .. } => FEATURE_PIN,
        AppMessage::List(_) => FEATURE_LISTS,
        // Not a change, but the same rules suit it: never to a peer that
        // can't read it (an older build would end the session), and a run
        // of them can't push out the texts held for the same device.
        AppMessage::Receipt(_) => FEATURE_RECEIPTS,
        _ => 0,
    }
}

/// What an acknowledgement means: our outgoing history entry `local_id`
/// (in the 1:1 conversation with the acknowledging peer, or in `group`),
/// or, with `relay_for`, a group message we forwarded for that member,
/// who sent it with reference `local_id` and wants a receipt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tag {
    pub local_id: u64,
    pub group: Option<[u8; 16]>,
    pub relay_for: Option<[u8; 32]>,
}

impl Tag {
    pub const NONE: Self = Self {
        local_id: 0,
        group: None,
        relay_for: None,
    };
    /// Encoded length (see [`Tag::encode`]).
    pub const LEN: usize = 57;

    /// `local id (u64 BE) | flags (1 group, 2 relay_for) | group (16) | relay_for (32)`
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.local_id.to_be_bytes());
        out.push(u8::from(self.group.is_some()) | (u8::from(self.relay_for.is_some()) << 1));
        out.extend_from_slice(&self.group.unwrap_or_default());
        out.extend_from_slice(&self.relay_for.unwrap_or_default());
    }

    /// Reads [`Tag::LEN`] bytes written by [`Tag::encode`].
    pub fn decode(b: &[u8]) -> Self {
        let local_id = u64::from_be_bytes(b[..8].try_into().unwrap_or_default());
        let group = (b[8] & 1 != 0).then(|| b[9..25].try_into().unwrap_or_default());
        let relay_for = (b[8] & 2 != 0).then(|| b[25..57].try_into().unwrap_or_default());
        Self {
            local_id,
            group,
            relay_for,
        }
    }
}

#[derive(Default)]
struct Recent {
    order: VecDeque<u64>,
    set: HashSet<u64>,
    /// When we last delivered from this peer, in [`Delivery::clock`] ticks.
    last: u64,
    /// An accepted contact's: kept over strangers' when trimming. Not
    /// saved; [`Delivery::mark_contacts`] sets it again after a restart.
    contact: bool,
}

#[derive(Default)]
pub(crate) struct Delivery {
    /// Per peer: (id, message, size, tag). The tag is the history entry's
    /// local id to mark delivered on acknowledgement (0 = none).
    unacked: HashMap<PublicIdentity, VecDeque<(u64, AppMessage, usize, Tag)>>,
    delivered: HashMap<PublicIdentity, Recent>,
    /// Ids whose message is being handled: neither delivered again nor
    /// saved as delivered until it has been (see [`Delivery::delivered`]).
    opening: HashSet<(PublicIdentity, u64)>,
    /// Ids in `delivered`, all peers together.
    delivered_ids: usize,
    /// Orders deliveries between peers.
    clock: u64,
    /// Changed since last saved.
    pub unacked_dirty: bool,
    pub delivered_dirty: bool,
}

impl Delivery {
    /// Restores saved state; anything unreadable is skipped.
    pub fn load(unacked: &[u8], delivered: &[u8]) -> Self {
        let mut d = Self::default();
        if let Some((&VERSION, mut rest)) = unacked.split_first() {
            while let Some((head, r)) = rest.split_at_checked(44 + Tag::LEN) {
                let n = 40 + Tag::LEN;
                let len =
                    u32::from_le_bytes([head[n], head[n + 1], head[n + 2], head[n + 3]]) as usize;
                let Some((msg, r)) = r.split_at_checked(len) else {
                    break;
                };
                rest = r;
                let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
                let id = u64::from_be_bytes(head[32..40].try_into().unwrap_or_default());
                let tag = Tag::decode(&head[40..40 + Tag::LEN]);
                if let (Ok(p), Ok(m)) = (PublicIdentity::from_bytes(&peer), AppMessage::decode(msg))
                {
                    d.unacked.entry(p).or_default().push_back((id, m, len, tag));
                }
            }
        }
        if let Some((&VERSION, mut rest)) = delivered.split_first() {
            while let Some((head, r)) = rest.split_at_checked(36) {
                let n = u32::from_le_bytes([head[32], head[33], head[34], head[35]]) as usize;
                let Some((ids, r)) = r.split_at_checked(n.saturating_mul(8)) else {
                    break;
                };
                rest = r;
                let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
                if let Ok(p) = PublicIdentity::from_bytes(&peer) {
                    // Saved oldest peer first.
                    d.clock += 1;
                    let recent = d.delivered.entry(p).or_default();
                    recent.last = d.clock;
                    for id in ids
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_be_bytes(*c))
                    {
                        if recent.set.insert(id) {
                            recent.order.push_back(id);
                            d.delivered_ids += 1;
                        }
                    }
                }
            }
        }
        d.trim_delivered(None);
        d
    }

    /// `VERSION || * ( peer (32) | id (u64 BE) | tag | len (u32 LE) | AppMessage )`,
    /// tag = `local id (u64 BE) | flags (1 group, 2 relay_for) | group (16) | relay_for (32)`
    pub fn encode_unacked(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        for (p, q) in &self.unacked {
            for (id, m, len, tag) in q {
                if *len > MAX_PERSISTED {
                    continue;
                }
                let Ok(b) = m.encode() else { continue };
                out.extend_from_slice(p.as_bytes());
                out.extend_from_slice(&id.to_be_bytes());
                tag.encode(&mut out);
                out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(u32::MAX).to_le_bytes());
                out.extend_from_slice(&b);
            }
        }
        out
    }

    /// `VERSION || * ( peer (32) | count (u32 LE) | ids (u64 BE each) )`,
    /// the peer delivered from longest ago first.
    pub fn encode_delivered(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + self.delivered.len() * 36 + self.delivered_ids * 8);
        out.push(VERSION);
        let mut peers: Vec<_> = self.delivered.iter().collect();
        peers.sort_by_key(|(_, r)| r.last);
        for (p, r) in peers {
            out.extend_from_slice(p.as_bytes());
            out.extend_from_slice(
                &u32::try_from(r.order.len())
                    .unwrap_or(u32::MAX)
                    .to_le_bytes(),
            );
            for id in &r.order {
                out.extend_from_slice(&id.to_be_bytes());
            }
        }
        out
    }

    /// Wraps `m` for `peer` and keeps it until acknowledged; `tag` is
    /// returned by [`Delivery::acked`] then.
    pub fn track(&mut self, peer: &PublicIdentity, m: AppMessage, tag: Tag) -> AppMessage {
        let id = u64::from_le_bytes(random_bytes());
        let len = m.encoded_len_hint();
        self.unacked_dirty |= len <= MAX_PERSISTED;
        let q = self.unacked.entry(*peer).or_default();
        // Changes (deletions, edits, reactions, pins, lists) and other
        // messages each have MAX_UNACKED places, so a run of changes for a
        // device that is away can't push out the texts held for it.
        let change = needs(&m) != 0;
        q.push_back((id, m.clone(), len, tag));
        let mut bytes: usize = q.iter().map(|(_, _, l, _)| l).sum();
        while q
            .iter()
            .filter(|(_, m, ..)| (needs(m) != 0) == change)
            .count()
            > MAX_UNACKED
        {
            let Some(i) = q.iter().position(|(_, m, ..)| (needs(m) != 0) == change) else {
                break;
            };
            if let Some((_, _, l, _)) = q.remove(i) {
                bytes -= l;
            }
        }
        while bytes > MAX_UNACKED_BYTES && q.len() > 1 {
            if let Some((_, _, l, _)) = q.pop_front() {
                bytes -= l;
            }
        }
        AppMessage::Tracked {
            id,
            inner: Box::new(m),
        }
    }

    /// Everything still unacknowledged for `peer`, to send again.
    pub fn resend(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        self.unacked.get(peer).map_or_else(Vec::new, |q| {
            q.iter()
                .map(|(id, m, ..)| AppMessage::Tracked {
                    id: *id,
                    inner: Box::new(m.clone()),
                })
                .collect()
        })
    }

    /// Drops what `peer` acknowledged; returns the tags that name entries,
    /// and the mailbox messages among it (a mailbox's copy can go now).
    pub fn acked(&mut self, peer: &PublicIdentity, ids: &[u64]) -> (Vec<Tag>, Vec<Vec<u8>>) {
        let (mut tags, mut mail) = (Vec::new(), Vec::new());
        if let Some(q) = self.unacked.get_mut(peer) {
            let before = q.len();
            q.retain_mut(|(id, m, _, tag)| {
                let hit = ids.contains(id);
                if hit && (tag.local_id != 0) {
                    tags.push(*tag);
                }
                if hit && let AppMessage::Mailbox(b) = m {
                    mail.push(std::mem::take(b));
                }
                !hit
            });
            self.unacked_dirty |= q.len() != before;
            if q.is_empty() {
                self.unacked.remove(peer);
            }
        }
        (tags, mail)
    }

    /// Disappearing texts still waiting for their peer whose timer has run
    /// out at our end (`now`): (peer, id, message).
    pub fn expired(&self, now: u64) -> Vec<(PublicIdentity, u64, AppMessage)> {
        let mut out = Vec::new();
        for (p, q) in &self.unacked {
            for (id, m, ..) in q {
                if let AppMessage::Text {
                    sent_ms,
                    expires_in_s: Some(s),
                    ..
                } = m
                    && sent_ms.saturating_add(u64::from(*s) * 1000) <= now
                {
                    out.push((*p, *id, m.clone()));
                }
            }
        }
        out
    }

    /// Swaps the message `id` waiting for `peer` for `m` (same id and tag),
    /// or drops it with `None`.
    pub fn replace(&mut self, peer: &PublicIdentity, id: u64, m: Option<AppMessage>) {
        let Some(q) = self.unacked.get_mut(peer) else {
            return;
        };
        let Some(i) = q.iter().position(|(x, ..)| *x == id) else {
            return;
        };
        let old = q[i].2;
        match m {
            Some(m) => {
                let len = m.encoded_len_hint();
                q[i].1 = m;
                q[i].2 = len;
                self.unacked_dirty |= len <= MAX_PERSISTED || old <= MAX_PERSISTED;
            }
            None => {
                q.remove(i);
                self.unacked_dirty |= old <= MAX_PERSISTED;
                if q.is_empty() {
                    self.unacked.remove(peer);
                }
            }
        }
    }

    /// The peer doesn't acknowledge: what was sent plain can't be tracked.
    pub fn forget(&mut self, peer: &PublicIdentity, id: u64) {
        self.acked(peer, &[id]);
    }

    /// Stops sending what `gone` picks out, to whichever peer.
    pub fn drop_where(&mut self, mut gone: impl FnMut(&PublicIdentity, &AppMessage) -> bool) {
        self.unacked.retain(|p, q| {
            q.retain(|(_, m, len, _)| {
                let drop = gone(p, m);
                self.unacked_dirty |= drop && *len <= MAX_PERSISTED;
                !drop
            });
            !q.is_empty()
        });
    }

    pub fn unacked(&self, peer: &PublicIdentity) -> usize {
        self.unacked.get(peer).map_or(0, VecDeque::len)
    }

    /// Notes that change `m` (a deletion, edit, reaction or pin) came from
    /// `peer`, and returns true if this exact change came before. A change
    /// for a device that is away is both held for its next session and
    /// sealed for its mailboxes, and carries no version, so a mailbox copy
    /// opened after the held one (and after newer changes) must be dropped
    /// rather than undo them. The change's digest is kept with the
    /// delivered ids (`contact`: `peer` is one we accepted). Anything else
    /// is never "seen".
    pub fn seen_change(&mut self, peer: &PublicIdentity, m: &AppMessage, contact: bool) -> bool {
        if !matches!(
            m,
            AppMessage::Delete { .. }
                | AppMessage::Edit { .. }
                | AppMessage::React { .. }
                | AppMessage::Pin { .. }
        ) {
            return false;
        }
        let Ok(b) = m.encode() else {
            return false;
        };
        let h = blake3::hash(&b);
        let digest = u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or_default());
        if !self.first_delivery(peer, digest) {
            return true;
        }
        self.delivered(peer, digest, contact);
        false
    }

    /// True the first time `id` arrives from `peer`. It only counts as
    /// delivered across a restart once [`Delivery::delivered`] says the
    /// message was handled (saved to history, say): a crash in between
    /// must take the sender's resend, not drop it as a duplicate.
    pub fn first_delivery(&mut self, peer: &PublicIdentity, id: u64) -> bool {
        let seen = self
            .delivered
            .get(peer)
            .is_some_and(|r| r.set.contains(&id));
        !seen && self.opening.insert((*peer, id))
    }

    /// The message `id` from `peer`, which [`Delivery::first_delivery`]
    /// let through, has been handled. `contact`: `peer` is one we accepted.
    pub fn delivered(&mut self, peer: &PublicIdentity, id: u64, contact: bool) {
        self.opening.remove(&(*peer, id));
        self.clock += 1;
        let r = self.delivered.entry(*peer).or_default();
        r.last = self.clock;
        r.contact = contact;
        if !r.set.insert(id) {
            return;
        }
        r.order.push_back(id);
        self.delivered_ids += 1;
        self.delivered_dirty = true;
        if r.order.len() > RECENT_IDS
            && let Some(old) = r.order.pop_front()
        {
            r.set.remove(&old);
            self.delivered_ids -= 1;
        }
        self.trim_delivered(Some(peer));
    }

    /// Which of the peers we remember ids from are accepted contacts.
    pub fn mark_contacts(&mut self, contacts: &HashSet<PublicIdentity>) {
        for (p, r) in &mut self.delivered {
            r.contact = contacts.contains(p);
        }
    }

    /// Peers we remember delivered ids from.
    pub fn delivered_from(&self) -> Vec<PublicIdentity> {
        self.delivered.keys().copied().collect()
    }

    /// Forgets the peers delivered from longest ago (never `keep`), strangers
    /// first, while there are more than [`MAX_DELIVERED_IDS`] ids: a flood
    /// from throwaway identities can't wipe what contacts' resends need.
    fn trim_delivered(&mut self, keep: Option<&PublicIdentity>) {
        while self.delivered_ids > MAX_DELIVERED_IDS {
            let Some(oldest) = self
                .delivered
                .iter()
                .filter(|(p, _)| Some(*p) != keep)
                .min_by_key(|(_, r)| (r.contact, r.last))
                .map(|(p, _)| *p)
            else {
                return;
            };
            if let Some(r) = self.delivered.remove(&oldest) {
                self.delivered_ids -= r.order.len();
                self.delivered_dirty = true;
            }
        }
    }

    /// `peer` is gone (forgotten, blocked, removed from its account):
    /// drops what we kept for it, sent or received.
    pub fn forget_peer(&mut self, peer: &PublicIdentity) {
        if let Some(q) = self.unacked.remove(peer) {
            self.unacked_dirty |= q.iter().any(|(_, _, len, _)| *len <= MAX_PERSISTED);
        }
        if let Some(r) = self.delivered.remove(peer) {
            self.delivered_ids -= r.order.len();
            self.delivered_dirty = true;
        }
        self.opening.retain(|(p, _)| p != peer);
    }

    /// Takes back [`Delivery::first_delivery`] for `id`: it wasn't kept
    /// after all, so the copy the peer sends again counts as the first.
    pub fn undeliver(&mut self, peer: &PublicIdentity, id: u64) {
        self.opening.remove(&(*peer, id));
        if let Some(r) = self.delivered.get_mut(peer)
            && r.set.remove(&id)
        {
            r.order.retain(|x| *x != id);
            self.delivered_ids -= 1;
            self.delivered_dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use threnody_core::Identity;

    fn text(s: &str) -> AppMessage {
        AppMessage::Text {
            sent_ms: 0,
            body: s.into(),
            expires_in_s: None,
            id: 0,
            quote: None,
            forwarded: false,
        }
    }

    #[test]
    fn tracks_until_acked_and_delivers_once() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        let AppMessage::Tracked { id, .. } = d.track(
            &p,
            text("a"),
            Tag {
                local_id: 5,
                group: Some([7; 16]),
                relay_for: Some([3; 32]),
            },
        ) else {
            panic!()
        };
        d.track(&p, text("b"), Tag::NONE);
        assert_eq!(d.unacked(&p), 2);
        assert_eq!(d.resend(&p).len(), 2);
        assert_eq!(
            d.acked(&p, &[id]).0,
            [Tag {
                local_id: 5,
                group: Some([7; 16]),
                relay_for: Some([3; 32]),
            }]
        );
        assert!(d.acked(&p, &[id]).0.is_empty(), "only once");
        let AppMessage::Tracked { id, .. } = d.track(&p, AppMessage::Mailbox(vec![4]), Tag::NONE)
        else {
            panic!()
        };
        assert_eq!(d.acked(&p, &[id]), (vec![], vec![vec![4]]));
        assert_eq!(d.unacked(&p), 1);
        assert!(d.first_delivery(&p, 9) && !d.first_delivery(&p, 9));
        d.delivered(&p, 9, true);
        assert!(!d.first_delivery(&p, 9));
        for i in 0..RECENT_IDS as u64 {
            d.first_delivery(&p, 100 + i);
            d.delivered(&p, 100 + i, true);
        }
        assert!(d.first_delivery(&p, 9), "old ids are forgotten");
    }

    #[test]
    fn changes_are_seen_once_per_peer() {
        let (p, q) = (Identity::generate().public(), Identity::generate().public());
        let mut d = Delivery::default();
        let pin = |pin| AppMessage::Pin {
            conversation: vec![1; 16],
            id: 7,
            pin,
        };
        assert!(!d.seen_change(&p, &pin(true), true));
        assert!(d.seen_change(&p, &pin(true), true), "the same change again");
        assert!(!d.seen_change(&p, &pin(false), true), "a different change");
        assert!(!d.seen_change(&q, &pin(true), true), "from another peer");
        assert!(!d.seen_change(&p, &text("a"), true) && !d.seen_change(&p, &text("a"), true));
    }

    #[test]
    fn held_changes_do_not_push_out_held_texts() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        for i in 0..10 {
            d.track(&p, text(&format!("t{i}")), Tag::NONE);
        }
        for i in 0..MAX_UNACKED as u64 + 50 {
            let react = AppMessage::React {
                conversation: vec![1; 16],
                id: i,
                emoji: "👍".into(),
                add: true,
            };
            d.track(&p, react, Tag::NONE);
        }
        let held = d.resend(&p);
        let texts = held
            .iter()
            .filter(|m| matches!(m, AppMessage::Tracked { inner, .. } if matches!(**inner, AppMessage::Text { .. })))
            .count();
        assert_eq!(texts, 10, "changes pushed out texts");
        assert_eq!(held.len(), 10 + MAX_UNACKED, "changes keep their own room");
    }

    #[test]
    fn state_survives_a_restart_except_files() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        d.track(&p, text("keep me"), Tag::NONE);
        d.track(&p, AppMessage::Group(vec![1; 100]), Tag::NONE);
        d.track(
            &p,
            AppMessage::File {
                sent_ms: 0,
                name: "big".into(),
                data: vec![0; MAX_PERSISTED],
                id: 0,
                sensitive: false,
                caption: String::new(),
                album: 0,
                clip: None,
                forwarded: false,
            },
            Tag::NONE,
        );
        d.first_delivery(&p, 42);
        d.delivered(&p, 42, true);
        assert!(d.unacked_dirty && d.delivered_dirty);
        let (u, del) = (d.encode_unacked(), d.encode_delivered());
        let mut again = Delivery::load(&u, &del);
        let t = Tag {
            local_id: 1,
            group: Some([4; 16]),
            relay_for: None,
        };
        let mut b = Vec::new();
        t.encode(&mut b);
        assert_eq!((b.len(), Tag::decode(&b)), (Tag::LEN, t));
        assert_eq!(again.resend(&p), d.resend(&p)[..2]);
        assert!(
            !again.first_delivery(&p, 42),
            "already delivered before the restart"
        );
        // Damaged state is skipped, not fatal.
        let cut = Delivery::load(&u[..u.len() - 1], &del[..del.len() - 1]);
        assert_eq!(cut.unacked(&p), 1);
        assert_eq!(Delivery::load(&[9], &[]).unacked(&p), 0);
    }

    #[test]
    fn an_id_is_saved_as_delivered_only_once_handled() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        assert!(d.first_delivery(&p, 7));
        assert!(!d.first_delivery(&p, 7), "a second copy while handling");
        // A crash now: the saved state hasn't got the id, so the
        // sender's resend is taken after the restart.
        assert!(!d.delivered_dirty);
        assert!(Delivery::load(&[], &d.encode_delivered()).first_delivery(&p, 7));
        d.delivered(&p, 7, true);
        assert!(!Delivery::load(&[], &d.encode_delivered()).first_delivery(&p, 7));
    }

    #[test]
    fn delivered_ids_are_bounded_across_peers_and_forgotten_with_them() {
        let mut d = Delivery::default();
        let peers: Vec<PublicIdentity> = (0..=MAX_DELIVERED_IDS / RECENT_IDS)
            .map(|_| Identity::generate().public())
            .collect();
        for (n, p) in peers.iter().enumerate() {
            for i in 0..RECENT_IDS as u64 {
                let id = (n as u64) << 32 | i;
                assert!(d.first_delivery(p, id));
                d.delivered(p, id, false);
            }
        }
        // The first peer, heard from longest ago, made room for the last.
        assert_eq!(d.delivered.len(), peers.len() - 1);
        assert!(d.first_delivery(&peers[0], 0));
        let last = peers.last().unwrap();
        assert!(!d.first_delivery(last, (peers.len() as u64 - 1) << 32));
        // And the order survives a restart.
        let again = Delivery::load(&[], &d.encode_delivered());
        assert_eq!(again.delivered_ids, MAX_DELIVERED_IDS);
        assert_eq!(
            again
                .delivered
                .iter()
                .min_by_key(|(_, r)| r.last)
                .map(|(p, _)| p),
            Some(&peers[1])
        );

        d.track(last, text("unsent"), Tag::NONE);
        d.unacked_dirty = false;
        d.delivered_dirty = false;
        d.forget_peer(last);
        assert!(d.unacked_dirty && d.delivered_dirty);
        assert_eq!(d.unacked(last), 0);
        assert!(d.first_delivery(last, (peers.len() as u64 - 1) << 32));
    }

    #[test]
    fn strangers_ids_go_before_a_contacts() {
        let mut d = Delivery::default();
        let contact = Identity::generate().public();
        for i in 0..RECENT_IDS as u64 {
            d.first_delivery(&contact, i);
            d.delivered(&contact, i, true);
        }
        // Throwaway identities send more than the cap between them, all
        // after the contact last did.
        let strangers: Vec<PublicIdentity> = (0..MAX_DELIVERED_IDS / RECENT_IDS)
            .map(|_| Identity::generate().public())
            .collect();
        for (n, p) in strangers.iter().enumerate() {
            for i in 0..RECENT_IDS as u64 {
                let id = (n as u64 + 1) << 32 | i;
                d.first_delivery(p, id);
                d.delivered(p, id, false);
            }
        }
        assert!(!d.first_delivery(&contact, 0), "the contact's ids stay");
        assert!(d.first_delivery(&strangers[0], 1 << 32));
        assert_eq!(d.delivered_ids, MAX_DELIVERED_IDS);

        // After a restart the node says again who the contacts are.
        let mut again = Delivery::load(&[], &d.encode_delivered());
        assert!(again.delivered_from().contains(&contact));
        again.mark_contacts(&HashSet::from([contact]));
        let p = Identity::generate().public();
        again.first_delivery(&p, 1);
        again.delivered(&p, 1, false);
        assert!(!again.first_delivery(&contact, 0));
    }

    #[test]
    fn chosen_unacked_messages_can_be_dropped() {
        let (p, q) = (Identity::generate().public(), Identity::generate().public());
        let mut d = Delivery::default();
        d.track(&p, text("keep"), Tag::NONE);
        d.track(&p, text("drop"), Tag::NONE);
        d.track(&q, text("drop"), Tag::NONE);
        d.unacked_dirty = false;
        d.drop_where(|peer, m| *peer == p && *m == text("drop"));
        assert!(d.unacked_dirty);
        assert_eq!((d.unacked(&p), d.unacked(&q)), (1, 1));
        assert_eq!(
            d.resend(&p),
            vec![AppMessage::Tracked {
                id: d.unacked[&p][0].0,
                inner: Box::new(text("keep"))
            }]
        );
    }

    #[test]
    fn expired_disappearing_texts_are_found_and_replaced() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        let gone = AppMessage::Text {
            sent_ms: 1_000,
            body: "secret".into(),
            expires_in_s: Some(30),
            id: 1,
            quote: None,
            forwarded: false,
        };
        let AppMessage::Tracked { id, .. } = d.track(&p, gone.clone(), Tag::NONE) else {
            panic!()
        };
        d.track(&p, text("kept"), Tag::NONE);
        assert!(d.expired(30_999).is_empty());
        assert_eq!(d.expired(31_000), [(p, id, gone)]);
        d.unacked_dirty = false;
        d.replace(&p, id, Some(AppMessage::Mailbox(vec![9])));
        assert!(d.unacked_dirty);
        assert!(d.expired(u64::MAX).is_empty());
        let saved = Delivery::load(&d.encode_unacked(), &[]);
        assert!(!String::from_utf8_lossy(&d.encode_unacked()).contains("secret"));
        assert_eq!(
            saved.resend(&p),
            [
                AppMessage::Tracked {
                    id,
                    inner: Box::new(AppMessage::Mailbox(vec![9]))
                },
                d.resend(&p)[1].clone()
            ]
        );
        d.replace(&p, id, None);
        assert_eq!(d.unacked(&p), 1);
    }

    #[test]
    fn queues_are_bounded() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        for i in 0..MAX_UNACKED + 10 {
            d.track(&p, text(&i.to_string()), Tag::NONE);
        }
        assert_eq!(d.unacked(&p), MAX_UNACKED);
        let big = AppMessage::File {
            sent_ms: 0,
            name: "f".into(),
            data: vec![0; 8 * 1024 * 1024],
            id: 0,
            sensitive: false,
            caption: String::new(),
            album: 0,
            clip: None,
            forwarded: false,
        };
        for _ in 0..6 {
            d.track(&p, big.clone(), Tag::NONE);
        }
        assert!(d.unacked(&p) <= 4, "byte cap");
        assert!(trackable(&big) && !trackable(&AppMessage::Cover));
    }
}
