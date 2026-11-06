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
    FEATURE_DELETE, FEATURE_EDIT, FEATURE_LISTS, FEATURE_PIN, FEATURE_REACT,
};
use threnody_core::{AppMessage, PublicIdentity};

/// Unacknowledged messages kept per peer, and as many changes to
/// conversations besides; the oldest of each are dropped first.
pub const MAX_UNACKED: usize = 256;
/// And at most this many bytes of them per peer.
pub const MAX_UNACKED_BYTES: usize = 32 * 1024 * 1024;
/// Delivered ids remembered per peer to drop resent copies.
pub const RECENT_IDS: usize = 4096;
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
}

#[derive(Default)]
pub(crate) struct Delivery {
    /// Per peer: (id, message, size, tag). The tag is the history entry's
    /// local id to mark delivered on acknowledgement (0 = none).
    unacked: HashMap<PublicIdentity, VecDeque<(u64, AppMessage, usize, Tag)>>,
    delivered: HashMap<PublicIdentity, Recent>,
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
                    let recent = d.delivered.entry(p).or_default();
                    for id in ids
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_be_bytes(*c))
                    {
                        if recent.set.insert(id) {
                            recent.order.push_back(id);
                        }
                    }
                }
            }
        }
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

    /// `VERSION || * ( peer (32) | count (u32 LE) | ids (u64 BE each) )`
    pub fn encode_delivered(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        for (p, r) in &self.delivered {
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

    /// Notes that change `m` (a deletion, edit, reaction or pin) came from
    /// `peer`, and returns true if this exact change came before. A change
    /// for a device that is away is both held for its next session and
    /// sealed for its mailboxes, and carries no version, so a mailbox copy
    /// opened after the held one (and after newer changes) must be dropped
    /// rather than undo them. The change's digest is kept with the
    /// delivered ids. Anything else is never "seen".
    pub fn seen_change(&mut self, peer: &PublicIdentity, m: &AppMessage) -> bool {
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
        !self.first_delivery(peer, digest)
    }

    /// True the first time `id` arrives from `peer`.
    pub fn first_delivery(&mut self, peer: &PublicIdentity, id: u64) -> bool {
        let r = self.delivered.entry(*peer).or_default();
        if !r.set.insert(id) {
            return false;
        }
        r.order.push_back(id);
        self.delivered_dirty = true;
        if r.order.len() > RECENT_IDS
            && let Some(old) = r.order.pop_front()
        {
            r.set.remove(&old);
        }
        true
    }

    /// Takes back [`Delivery::first_delivery`] for `id`: it wasn't kept
    /// after all, so the copy the peer sends again counts as the first.
    pub fn undeliver(&mut self, peer: &PublicIdentity, id: u64) {
        if let Some(r) = self.delivered.get_mut(peer)
            && r.set.remove(&id)
        {
            r.order.retain(|x| *x != id);
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
        for i in 0..RECENT_IDS as u64 {
            d.first_delivery(&p, 100 + i);
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
        assert!(!d.seen_change(&p, &pin(true)));
        assert!(d.seen_change(&p, &pin(true)), "the same change again");
        assert!(!d.seen_change(&p, &pin(false)), "a different change");
        assert!(!d.seen_change(&q, &pin(true)), "from another peer");
        assert!(!d.seen_change(&p, &text("a")) && !d.seen_change(&p, &text("a")));
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
