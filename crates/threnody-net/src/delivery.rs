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
/// Messages larger than this (files) are resent only within one run.
pub const MAX_PERSISTED: usize = 64 * 1024;
const VERSION: u8 = 1;

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

#[derive(Default)]
struct Recent {
    order: VecDeque<u64>,
    set: HashSet<u64>,
}

#[derive(Default)]
pub(crate) struct Delivery {
    unacked: HashMap<PublicIdentity, VecDeque<(u64, AppMessage, usize)>>,
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
            while let Some((head, r)) = rest.split_at_checked(44) {
                let len = u32::from_le_bytes([head[40], head[41], head[42], head[43]]) as usize;
                let Some((msg, r)) = r.split_at_checked(len) else {
                    break;
                };
                rest = r;
                let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
                let id = u64::from_be_bytes(head[32..40].try_into().unwrap_or_default());
                if let (Ok(p), Ok(m)) = (PublicIdentity::from_bytes(&peer), AppMessage::decode(msg))
                {
                    d.unacked.entry(p).or_default().push_back((id, m, len));
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

    /// `VERSION || * ( peer (32) | id (u64 BE) | len (u32 LE) | AppMessage )`
    pub fn encode_unacked(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        for (p, q) in &self.unacked {
            for (id, m, len) in q {
                if *len > MAX_PERSISTED {
                    continue;
                }
                let Ok(b) = m.encode() else { continue };
                out.extend_from_slice(p.as_bytes());
                out.extend_from_slice(&id.to_be_bytes());
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

    /// Wraps `m` for `peer` and keeps it until acknowledged.
    pub fn track(&mut self, peer: &PublicIdentity, m: AppMessage) -> AppMessage {
        let id = u64::from_le_bytes(random_bytes());
        let len = m.encoded_len_hint();
        self.unacked_dirty |= len <= MAX_PERSISTED;
        let q = self.unacked.entry(*peer).or_default();
        q.push_back((id, m.clone(), len));
        let mut bytes: usize = q.iter().map(|(.., l)| l).sum();
        while q.len() > MAX_UNACKED || (bytes > MAX_UNACKED_BYTES && q.len() > 1) {
            if let Some((.., l)) = q.pop_front() {
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
                .map(|(id, m, _)| AppMessage::Tracked {
                    id: *id,
                    inner: Box::new(m.clone()),
                })
                .collect()
        })
    }

    pub fn acked(&mut self, peer: &PublicIdentity, ids: &[u64]) {
        if let Some(q) = self.unacked.get_mut(peer) {
            let before = q.len();
            q.retain(|(id, ..)| !ids.contains(id));
            self.unacked_dirty |= q.len() != before;
            if q.is_empty() {
                self.unacked.remove(peer);
            }
        }
    }

    /// The peer doesn't acknowledge: what was sent plain can't be tracked.
    pub fn forget(&mut self, peer: &PublicIdentity, id: u64) {
        self.acked(peer, &[id]);
    }

    pub fn unacked(&self, peer: &PublicIdentity) -> usize {
        self.unacked.get(peer).map_or(0, VecDeque::len)
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
        }
    }

    #[test]
    fn tracks_until_acked_and_delivers_once() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        let AppMessage::Tracked { id, .. } = d.track(&p, text("a")) else {
            panic!()
        };
        d.track(&p, text("b"));
        assert_eq!(d.unacked(&p), 2);
        assert_eq!(d.resend(&p).len(), 2);
        d.acked(&p, &[id]);
        assert_eq!(d.unacked(&p), 1);
        assert!(d.first_delivery(&p, 9) && !d.first_delivery(&p, 9));
        for i in 0..RECENT_IDS as u64 {
            d.first_delivery(&p, 100 + i);
        }
        assert!(d.first_delivery(&p, 9), "old ids are forgotten");
    }

    #[test]
    fn state_survives_a_restart_except_files() {
        let p = Identity::generate().public();
        let mut d = Delivery::default();
        d.track(&p, text("keep me"));
        d.track(&p, AppMessage::Group(vec![1; 100]));
        d.track(
            &p,
            AppMessage::File {
                sent_ms: 0,
                name: "big".into(),
                data: vec![0; MAX_PERSISTED],
            },
        );
        d.first_delivery(&p, 42);
        assert!(d.unacked_dirty && d.delivered_dirty);
        let (u, del) = (d.encode_unacked(), d.encode_delivered());
        let mut again = Delivery::load(&u, &del);
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
            d.track(&p, text(&i.to_string()));
        }
        assert_eq!(d.unacked(&p), MAX_UNACKED);
        let big = AppMessage::File {
            sent_ms: 0,
            name: "f".into(),
            data: vec![0; 8 * 1024 * 1024],
        };
        for _ in 0..6 {
            d.track(&p, big.clone());
        }
        assert!(d.unacked(&p) <= 4, "byte cap");
        assert!(trackable(&big) && !trackable(&AppMessage::Cover));
    }
}
