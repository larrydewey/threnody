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
use threnody_core::{AppMessage, PublicIdentity};

/// Unacknowledged messages kept per peer; the oldest are dropped first.
pub const MAX_UNACKED: usize = 256;
/// And at most this many bytes of them per peer.
pub const MAX_UNACKED_BYTES: usize = 32 * 1024 * 1024;
/// Delivered ids remembered per peer to drop resent copies.
pub const RECENT_IDS: usize = 4096;
/// And at most this many across all peers (strangers each add theirs):
/// the peers heard from longest ago are forgotten first.
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
        q.push_back((id, m.clone(), len, tag));
        let mut bytes: usize = q.iter().map(|(_, _, l, _)| l).sum();
        while q.len() > MAX_UNACKED || (bytes > MAX_UNACKED_BYTES && q.len() > 1) {
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

    /// Drops what `peer` acknowledged; returns the tags that name entries.
    pub fn acked(&mut self, peer: &PublicIdentity, ids: &[u64]) -> Vec<Tag> {
        let mut tags = Vec::new();
        if let Some(q) = self.unacked.get_mut(peer) {
            let before = q.len();
            q.retain(|(id, _, _, tag)| {
                let hit = ids.contains(id);
                if hit && (tag.local_id != 0) {
                    tags.push(*tag);
                }
                !hit
            });
            self.unacked_dirty |= q.len() != before;
            if q.is_empty() {
                self.unacked.remove(peer);
            }
        }
        tags
    }

    /// The peer doesn't acknowledge: what was sent plain can't be tracked.
    pub fn forget(&mut self, peer: &PublicIdentity, id: u64) {
        self.acked(peer, &[id]);
    }

    pub fn unacked(&self, peer: &PublicIdentity) -> usize {
        self.unacked.get(peer).map_or(0, VecDeque::len)
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
    /// let through, has been handled.
    pub fn delivered(&mut self, peer: &PublicIdentity, id: u64) {
        self.opening.remove(&(*peer, id));
        self.clock += 1;
        let r = self.delivered.entry(*peer).or_default();
        r.last = self.clock;
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

    /// Forgets the peers delivered from longest ago (never `keep`) while
    /// there are more than [`MAX_DELIVERED_IDS`] ids.
    fn trim_delivered(&mut self, keep: Option<&PublicIdentity>) {
        while self.delivered_ids > MAX_DELIVERED_IDS {
            let Some(oldest) = self
                .delivered
                .iter()
                .filter(|(p, _)| Some(*p) != keep)
                .min_by_key(|(_, r)| r.last)
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
            d.acked(&p, &[id]),
            [Tag {
                local_id: 5,
                group: Some([7; 16]),
                relay_for: Some([3; 32]),
            }]
        );
        assert!(d.acked(&p, &[id]).is_empty(), "only once");
        assert_eq!(d.unacked(&p), 1);
        assert!(d.first_delivery(&p, 9) && !d.first_delivery(&p, 9));
        d.delivered(&p, 9);
        assert!(!d.first_delivery(&p, 9));
        for i in 0..RECENT_IDS as u64 {
            d.first_delivery(&p, 100 + i);
            d.delivered(&p, 100 + i);
        }
        assert!(d.first_delivery(&p, 9), "old ids are forgotten");
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
        d.delivered(&p, 42);
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
        d.delivered(&p, 7);
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
                d.delivered(p, id);
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
