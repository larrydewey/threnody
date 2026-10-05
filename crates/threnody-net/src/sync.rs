//! Message history across our own devices (Appendix J, "History").
//!
//! Contacts send to every device of our account, so each device receives
//! incoming messages itself. What one device *sends* the others never see
//! unless told. So:
//!
//! - an outgoing 1:1 entry is pushed at once to our other devices that are
//!   connected (`AccountMsg::Transcript`);
//! - when a session with one of our devices starts, we send it our outgoing
//!   entries newer than what it has had from us. The first time, we send
//!   all of our 1:1 history, so a newly linked device starts complete.
//!
//! Receivers merge entries, skipping ones already present, and only from
//! devices of our own account. Files travel as records (name, size), not
//! contents. Group history isn't sent: a device only reads a group from
//! when it joined (MLS), and sees what our other devices send there as a
//! member.

use std::collections::HashMap;

use threnody_core::history::{ConversationId, Entry, History};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::delivery::Tag;
use crate::node::{Event, Node, lock};

/// Entries per transcript message.
pub const CHUNK: usize = 256;

/// Per own device: the time up to which it has our outgoing entries.
#[derive(Default)]
pub(crate) struct SyncState {
    synced: HashMap<[u8; 32], u64>,
}

impl SyncState {
    /// `VERSION (1) || * ( device (32) | until (u64 BE) )`
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![1];
        for (d, t) in &self.synced {
            out.extend_from_slice(d);
            out.extend_from_slice(&t.to_be_bytes());
        }
        out
    }

    pub fn decode(b: &[u8]) -> Self {
        let mut s = Self::default();
        if let Some((1, rest)) = b.split_first() {
            for c in rest.as_chunks::<40>().0 {
                let d: [u8; 32] = c[..32].try_into().unwrap_or_default();
                let t = u64::from_be_bytes(c[32..].try_into().unwrap_or_default());
                s.synced.insert(d, t);
            }
        }
        s
    }
}

/// An entry as another device should store it: a file's location is a
/// path on this device, meaningless there.
fn portable(mut e: Entry) -> Entry {
    if let Some(f) = &mut e.file {
        f.location = None;
    }
    e
}

impl Node {
    fn save_sync(&self, s: &SyncState) {
        self.shared.save_state("history-sync", Ok(s.encode()));
    }

    /// Whether `peer` has had our history before.
    pub(crate) fn has_synced(&self, peer: &PublicIdentity) -> bool {
        lock(&self.shared.sync).synced.contains_key(peer.as_bytes())
    }

    /// Transcripts to send our own device `peer` as its session starts.
    pub(crate) fn transcripts_for(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let me = *self.identity().as_bytes();
        let since = lock(&self.shared.sync).synced.get(peer.as_bytes()).copied();
        let now = now_ms();
        let mut out = Vec::new();
        for conv in self.shared.home.conversations() {
            if !matches!(conv, ConversationId::Peer(_)) {
                continue;
            }
            let Ok(h) = self.history(conv) else { continue };
            let entries: Vec<Entry> = h
                .entries()
                .iter()
                .filter(|e| match since {
                    // First sync: everything we have.
                    None => true,
                    Some(t) => e.outgoing && e.device == me && e.at_ms > t,
                })
                .cloned()
                .map(portable)
                .collect();
            for chunk in entries.chunks(CHUNK) {
                // Tracked, so a transcript lost with a dying session is resent.
                if let Some(m) = self.transcript_message(conv, chunk.to_vec()) {
                    out.push(self.shared.delivery(|d| d.track(peer, m, Tag::NONE)));
                }
            }
        }
        let mut s = lock(&self.shared.sync);
        s.synced.insert(*peer.as_bytes(), now);
        self.save_sync(&s);
        out
    }

    /// Pushes a new outgoing 1:1 entry to our connected other devices.
    pub(crate) fn push_entry(&self, conv: ConversationId, entry: &Entry) {
        if !matches!(conv, ConversationId::Peer(_)) {
            return;
        }
        let own: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .map(|s| s.peer)
            .filter(|p| self.is_own_device(p))
            .collect();
        if own.is_empty() {
            return;
        }
        let Some(m) = self.transcript_message(conv, vec![portable(entry.clone())]) else {
            return;
        };
        let mut s = lock(&self.shared.sync);
        for p in own {
            // Devices that never synced get everything when they next connect.
            if s.synced.contains_key(p.as_bytes()) && self.send_tracked(&p, m.clone()).is_ok() {
                let t = s.synced.entry(*p.as_bytes()).or_default();
                *t = (*t).max(entry.at_ms);
            }
        }
        self.save_sync(&s);
    }

    fn transcript_message(&self, conv: ConversationId, entries: Vec<Entry>) -> Option<AppMessage> {
        let h = History::transcript(entries).encode().ok()?;
        crate::account::transcript(conv.to_bytes(), h)
    }

    /// Merges a transcript from our own device `from`.
    pub(crate) fn on_transcript(&self, from: PublicIdentity, conv: &[u8], history: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let (Some(conv), Ok(h)) = (ConversationId::from_bytes(conv), History::decode(history))
        else {
            return;
        };
        if !matches!(conv, ConversationId::Peer(_)) {
            return;
        }
        // Nothing from before the user cleared this chat.
        let cleared = self.cleared_at(conv);
        let entries: Vec<Entry> = h
            .entries()
            .iter()
            .filter(|e| e.at_ms > cleared)
            .cloned()
            .collect();
        if let Ok(added) =
            self.shared
                .home
                .merge_entries(self.identity_ref(), conv, entries, now_ms())
            && added > 0
        {
            self.emit(Event::HistorySynced { from, added });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_round_trips() {
        let mut s = SyncState::default();
        s.synced.insert([1; 32], 5);
        s.synced.insert([2; 32], u64::MAX);
        let b = s.encode();
        assert_eq!(SyncState::decode(&b).synced, s.synced);
        assert!(
            SyncState::decode(&b[..b.len() - 1]).synced.len() == 1,
            "partial record dropped"
        );
        assert!(SyncState::decode(&[9]).synced.is_empty());
    }
}
