//! Message history across our own devices (Appendix J, "History").
//!
//! Contacts send texts to every device of our account, so each device
//! receives incoming texts itself (files reach one device: see *Received
//! files* below). What one device *sends* the others never see unless
//! told. So:
//!
//! - an outgoing 1:1 entry is pushed at once to our other devices that are
//!   connected (`AccountMsg::Transcript`);
//! - when a session with one of our devices starts, we send it our outgoing
//!   entries newer than what it has had from us. The first time, we send
//!   all of our history, 1:1 and group, so a newly linked device starts
//!   complete.
//!
//! Receivers merge entries, skipping ones already present, and only from
//! devices of our own account. Files travel as records (name, size); their
//! contents come on request (see `filesync`). A device reads a group's
//! messages (MLS) only from when it joined, so for groups we also send
//! every entry, ours or not, newer than what it has had from us: that
//! covers the time between being linked and being added.
//!
//! Ticks (Appendix J, *Ticks*): only the sending device hears a contact's
//! acknowledgements, so what each device learns about an outgoing entry's
//! ticks goes to our other devices as `AccountMsg::Ticks`, to those whose
//! `Hello` has `FEATURE_DEVICE_SYNC`:
//!
//! - live, batched for a second, to connected ones (plain sends);
//! - at session start, every tick this device learned since it last caught
//!   that device up (`Entry::ticked_ms` against a per-device watermark,
//!   tracked sends). A first full transcript already carries every tick.
//!
//! Merges only move forward and are never forwarded on; a tick for an
//! entry not here yet waits in memory until a transcript brings it.
//!
//! Received files (Appendix J, *Received files*): a contact's file reaches
//! only the device it was sent to (files go live to one device, unlike
//! texts), so that device tells our other devices of it, by transcript,
//! to those whose `Hello` has `FEATURE_DEVICE_SYNC` (an older one could
//! show it but never fetch it):
//!
//! - live, as it is recorded, to connected ones;
//! - at session start, every record of a file a contact sent us that this
//!   device has since it last caught that device up (state
//!   `history-sync-files`): all of them, once, the first time. A first
//!   full transcript already carries them, so it sets that position.
//!
//! Each carries the record only; the contents come as for any synced file
//! (`filesync`), so sensitive and disappearing ones wait for a tap, and
//! the entry's expiry goes with it.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use threnody_core::devsync::{TICKS_PER_MESSAGE, Tick, Ticks};
use threnody_core::history::{ConversationId, Entry, History, tick_of};
use threnody_core::message::FEATURE_DEVICE_SYNC;
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::delivery::Tag;
use crate::node::{Event, Node, lock};

/// Where the ticks watermark per device is kept.
pub(crate) const TICKS_STATE: &str = "history-sync-ticks";
/// Where the received-files watermark per device is kept.
pub(crate) const FILES_STATE: &str = "history-sync-files";
/// How long live ticks wait to go together.
pub(crate) const TICK_BATCH: Duration = Duration::from_secs(1);
/// Most ticks kept for entries not here yet.
pub(crate) const MAX_ORPHAN_TICKS: usize = 4096;

/// Entries per transcript message.
pub const CHUNK: usize = 256;

/// Per own device: the time up to which it has our outgoing entries (or,
/// as `Shared::ticks_sync`, our ticks).
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
    e.ticked_ms = 0;
    e
}

/// The ticks among `entries` a device whose ticks watermark is `since`
/// lacks: when it has none (linked before ticks were synced), every
/// outgoing entry with any tick, once; else those that changed after it.
/// None for notes to self, which nobody delivers.
pub(crate) fn ticks_since(notes: bool, entries: &[Entry], since: Option<u64>) -> Vec<Tick> {
    if notes {
        return Vec::new();
    }
    entries
        .iter()
        .filter(|e| match since {
            None => e.delivered || e.read_ms != 0 || !e.delivered_to.is_empty(),
            Some(t) => e.ticked_ms > t,
        })
        .filter_map(tick_of)
        .collect()
}

/// The records among `entries` (a chat with a contact) of files the
/// contact sent us that a device whose received-files watermark is `since`
/// lacks: all of them when it has none, else those recorded after it.
pub(crate) fn received_files_since(entries: &[Entry], since: Option<u64>) -> Vec<Entry> {
    entries
        .iter()
        .filter(|e| !e.outgoing && e.file.is_some() && since.is_none_or(|t| e.at_ms > t))
        .cloned()
        .map(portable)
        .collect()
}

/// Ticks for entries not here yet, oldest dropped first.
#[derive(Default)]
pub(crate) struct OrphanTicks(std::collections::VecDeque<(ConversationId, Tick)>);

impl OrphanTicks {
    pub(crate) fn add(&mut self, conv: ConversationId, ticks: impl IntoIterator<Item = Tick>) {
        for t in ticks {
            // One per entry, holding all that is known of it.
            if let Some((_, x)) = self
                .0
                .iter_mut()
                .find(|(c, x)| *c == conv && x.id == t.id && x.device == t.device)
            {
                x.delivered |= t.delivered;
                if x.read_ms == 0 {
                    x.read_ms = t.read_ms;
                }
                x.recipients = x.recipients.max(t.recipients);
                for d in t.delivered_to {
                    if x.delivered_to.len() < threnody_core::devsync::MAX_TICK_DEVICES
                        && !x.delivered_to.contains(&d)
                    {
                        x.delivered_to.push(d);
                    }
                }
                continue;
            }
            if self.0.len() >= MAX_ORPHAN_TICKS {
                self.0.pop_front();
            }
            self.0.push_back((conv, t));
        }
    }

    /// Takes the ticks waiting for `conv`.
    pub(crate) fn take(&mut self, conv: ConversationId) -> Vec<Tick> {
        let (mine, rest) = std::mem::take(&mut self.0)
            .into_iter()
            .partition(|(c, _)| *c == conv);
        self.0 = rest;
        mine.into_iter().map(|(_, t)| t).collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
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
        if since.is_none() {
            // The full transcript carries every entry's ticks as they are
            // now, and every file a contact sent us: nothing to catch up
            // on beyond it.
            let mut t = lock(&self.shared.ticks_sync);
            t.synced.insert(*peer.as_bytes(), now);
            self.shared.save_state(TICKS_STATE, Ok(t.encode()));
            drop(t);
            let mut f = lock(&self.shared.files_sync);
            f.synced.insert(*peer.as_bytes(), now);
            self.shared.save_state(FILES_STATE, Ok(f.encode()));
        }
        let mut out = Vec::new();
        for conv in self.shared.home.conversations(self.identity_ref()) {
            let group = matches!(conv, ConversationId::Group(_));
            let Ok(h) = self.history(conv) else { continue };
            let entries: Vec<Entry> = h
                .entries()
                .iter()
                .filter(|e| match since {
                    // First sync: everything we have.
                    None => true,
                    Some(t) => e.at_ms > t && (group || (e.outgoing && e.device == me)),
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

    /// Whether `conv` is a chat with a contact: not our notes, nor a chat
    /// between our own devices (which each device files under the other).
    fn with_contact(&self, conv: ConversationId) -> bool {
        let ConversationId::Peer(k) = conv else {
            return false;
        };
        // An account id need not be a device key at all.
        conv != self.notes()
            && PublicIdentity::from_bytes(&k)
                .map_or(true, |p| p != self.identity() && !self.is_own_device(&p))
    }

    /// Tells our connected other devices that take file fetches of a file
    /// a contact just sent us (`entry`, recorded in `conv`), so they show
    /// it and fetch it as they would one of ours.
    pub(crate) fn push_received_file(&self, conv: ConversationId, entry: &Entry) {
        if entry.outgoing || entry.file.is_none() || !self.with_contact(conv) {
            return;
        }
        // Devices that never synced get everything when they next connect.
        let peers: Vec<PublicIdentity> = self
            .device_sync_peers()
            .into_iter()
            .filter(|p| self.has_synced(p))
            .collect();
        if peers.is_empty() {
            return;
        }
        let Some(m) = self.transcript_message(conv, vec![portable(entry.clone())]) else {
            return;
        };
        // Past it for those, so a session start doesn't send it again (and
        // bring back a copy deleted there meanwhile).
        let mut s = lock(&self.shared.files_sync);
        for p in peers {
            if self.send_tracked(&p, m.clone()).is_ok() {
                let t = s.synced.entry(*p.as_bytes()).or_default();
                *t = (*t).max(entry.at_ms);
            }
        }
        self.shared.save_state(FILES_STATE, Ok(s.encode()));
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
        // Nothing from before the user cleared this chat.
        let cleared = self.cleared_at(conv);
        // A location is a path or URI on the device that recorded it,
        // never ours to open or serve, whatever a sibling sends.
        let entries: Vec<Entry> = h
            .entries()
            .iter()
            .filter(|e| e.at_ms > cleared)
            .cloned()
            .map(portable)
            .collect();
        let files: Vec<Entry> = entries
            .iter()
            .filter(|e| e.file.is_some())
            .cloned()
            .collect();
        // The transcript's ticks, for entries we had already (merging
        // entries skips those): a device that got them from a third one,
        // or whose sync state was lost, still catches up on what the
        // sender knows of them.
        let ticks: Vec<Tick> = if conv == self.notes() {
            Vec::new()
        } else {
            entries
                .iter()
                .filter_map(tick_of)
                .filter(|t| t.delivered || t.read_ms != 0 || !t.delivered_to.is_empty())
                .collect()
        };
        let Ok(added) =
            self.shared
                .home
                .merge_entries(self.identity_ref(), conv, entries, now_ms())
        else {
            return;
        };
        if !ticks.is_empty() {
            self.merge_ticks(from, conv, &ticks, false);
        }
        // Queued before the event, so whoever sees it sees the queue.
        self.fetch_auto(conv, &files);
        if added > 0 {
            // Ticks that came before their entries.
            let waiting = lock(&self.shared.orphan_ticks).take(conv);
            if !waiting.is_empty() {
                self.merge_ticks(from, conv, &waiting, true);
            }
            self.emit(Event::HistorySynced { from, added });
        }
    }

    /// Our connected other devices that take ticks and file fetches.
    pub(crate) fn device_sync_peers(&self) -> Vec<PublicIdentity> {
        self.sessions()
            .into_iter()
            .map(|s| s.peer)
            .filter(|p| self.is_own_device(p) && self.supports(p, FEATURE_DEVICE_SYNC))
            .collect()
    }

    /// At the start of a session with our own device `peer` (once its
    /// `Hello` says what it takes): the ticks and the records of files
    /// contacts sent us it lacks, and our file fetches it may serve.
    pub(crate) fn device_sync_hello(&self, peer: &PublicIdentity) {
        if !(self.is_own_device(peer) && self.supports(peer, FEATURE_DEVICE_SYNC)) {
            return;
        }
        let (node, peer) = (self.clone(), *peer);
        // Loads every conversation: off the session task, which reads
        // frames.
        let work = move || {
            for m in node.catch_up_for(&peer) {
                let _ = node.send_tracked(&peer, m);
            }
            node.resume_fetches();
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(work);
            }
            Err(_) => work(),
        }
    }

    /// Messages for our own device `peer` that takes device sync: the
    /// ticks it lacks, and the records of files contacts sent us it lacks
    /// (transcripts), since we last caught it up, every conversation's.
    pub(crate) fn catch_up_for(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let now = now_ms();
        let since = lock(&self.shared.ticks_sync)
            .synced
            .get(peer.as_bytes())
            .copied();
        let files_since = lock(&self.shared.files_sync)
            .synced
            .get(peer.as_bytes())
            .copied();
        let notes = self.notes();
        let mut out = Vec::new();
        for conv in self.shared.home.conversations(self.identity_ref()) {
            let Ok(h) = self.history(conv) else { continue };
            if self.with_contact(conv) {
                // As live: never a message request's.
                let files: Vec<Entry> = received_files_since(h.entries(), files_since)
                    .into_iter()
                    .filter(|e| {
                        PublicIdentity::from_bytes(&e.device).is_ok_and(|d| self.is_accepted(&d))
                    })
                    .collect();
                for chunk in files.chunks(CHUNK) {
                    out.extend(self.transcript_message(conv, chunk.to_vec()));
                }
            }
            let ticks = ticks_since(conv == notes, h.entries(), since);
            out.extend(self.ticks_messages(conv, ticks));
        }
        for (state, name) in [
            (&self.shared.ticks_sync, TICKS_STATE),
            (&self.shared.files_sync, FILES_STATE),
        ] {
            let mut s = lock(state);
            let t = s.synced.entry(*peer.as_bytes()).or_default();
            *t = (*t).max(now);
            self.shared.save_state(name, Ok(s.encode()));
        }
        out
    }

    fn ticks_messages(&self, conv: ConversationId, ticks: Vec<Tick>) -> Vec<AppMessage> {
        ticks
            .chunks(TICKS_PER_MESSAGE)
            .filter_map(|c| {
                let b = Ticks(c.to_vec()).encode().ok()?;
                crate::account::ticks_message(conv.to_bytes(), b)
            })
            .collect()
    }

    /// Tells our connected other devices, within [`TICK_BATCH`], what
    /// changed in the ticks of the outgoing entries `ids` of `conv`.
    pub(crate) fn push_ticks(&self, conv: ConversationId, ids: &[u64]) {
        if ids.is_empty() || conv == self.notes() || self.device_sync_peers().is_empty() {
            return;
        }
        let first = {
            let mut o = lock(&self.shared.tick_outbox);
            let first = o.is_empty();
            o.entry(conv).or_default().extend(ids.iter().copied());
            first
        };
        if !first {
            return;
        }
        let node = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    tokio::time::sleep(TICK_BATCH).await;
                    let _ = tokio::task::spawn_blocking(move || node.flush_ticks()).await;
                });
            }
            Err(_) => node.flush_ticks(),
        }
    }

    /// Sends what [`Node::push_ticks`] gathered.
    fn flush_ticks(&self) {
        let batch: HashMap<ConversationId, BTreeSet<u64>> =
            std::mem::take(&mut *lock(&self.shared.tick_outbox));
        let peers = self.device_sync_peers();
        if peers.is_empty() {
            return;
        }
        for (conv, ids) in batch {
            let Ok(h) = self.history(conv) else { continue };
            let ticks: Vec<Tick> = h
                .entries()
                .iter()
                .filter(|e| ids.contains(&e.message_id()))
                .filter_map(tick_of)
                .collect();
            for m in self.ticks_messages(conv, ticks) {
                for p in &peers {
                    let _ = self.send(p, m.clone());
                }
            }
        }
    }

    /// Handles ticks from our own device `from`.
    pub(crate) fn on_ticks(&self, from: PublicIdentity, conv: &[u8], body: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let (Some(conv), Ok(ticks)) = (ConversationId::from_bytes(conv), Ticks::decode(body))
        else {
            return;
        };
        self.merge_ticks(from, conv, &ticks.0, true);
    }

    /// Merges `ticks` from `from`; those for entries not here yet wait for
    /// them if `keep_unmatched`.
    fn merge_ticks(
        &self,
        from: PublicIdentity,
        conv: ConversationId,
        ticks: &[Tick],
        keep_unmatched: bool,
    ) {
        let Ok((n, unmatched)) =
            self.shared
                .home
                .merge_ticks(self.identity_ref(), conv, ticks, now_ms())
        else {
            return;
        };
        if keep_unmatched && !unmatched.is_empty() {
            lock(&self.shared.orphan_ticks).add(conv, unmatched);
        }
        if n > 0 {
            self.emit(Event::DeviceSync { from });
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

    fn entry(at_ms: u64, outgoing: bool, id: u64) -> Entry {
        let mut h = History::transcript(Vec::new());
        h.push(
            Entry {
                at_ms,
                outgoing,
                device: [1; 32],
                text: "x".into(),
                offline: false,
                expires_at_ms: None,
                file: None,
                local_id: if outgoing { id } else { 0 },
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id: if outgoing { 0 } else { id },
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
            0,
        );
        h.entries()[0].clone()
    }

    #[test]
    fn ticks_since_backfills_once_then_follows_the_watermark() {
        let plain = entry(1, true, 1);
        let delivered = Entry {
            delivered: true,
            ticked_ms: 10,
            ..entry(2, true, 2)
        };
        let read = Entry {
            delivered: true,
            read_ms: 5,
            ticked_ms: 30,
            ..entry(3, true, 3)
        };
        let group = Entry {
            recipients: 3,
            delivered_to: vec![[9; 32]],
            ticked_ms: 20,
            ..entry(4, true, 4)
        };
        let theirs = Entry {
            delivered: true,
            ticked_ms: 40,
            ..entry(5, false, 5)
        };
        let no_id = Entry {
            delivered: true,
            ticked_ms: 50,
            ..entry(6, true, 0)
        };
        let all = [plain, delivered, read, group, theirs, no_id];
        let ids = |t: Vec<Tick>| t.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(ticks_since(false, &all, None)), [2, 3, 4]);
        assert_eq!(ids(ticks_since(false, &all, Some(15))), [3, 4]);
        assert_eq!(ids(ticks_since(false, &all, Some(30))), Vec::<u64>::new());
        assert!(ticks_since(true, &all, None).is_empty());
        let t = &ticks_since(false, &all, Some(25))[0];
        assert_eq!((t.id, t.delivered, t.read_ms), (3, true, 5));
    }

    #[test]
    fn received_files_since_takes_contacts_files_after_the_watermark() {
        let note = || threnody_core::history::FileNote {
            name: "a.png".into(),
            size: 1,
            location: Some("/home/me/a.png".into()),
            sensitive: false,
            album: 0,
            clip: None,
            forwarded: false,
        };
        let theirs = |at, id| Entry {
            file: Some(note()),
            ..entry(at, false, id)
        };
        let ours = Entry {
            file: Some(note()),
            ..entry(5, true, 3)
        };
        let text = entry(6, false, 4);
        let all = [theirs(1, 1), theirs(10, 2), ours, text];
        let ids = |v: Vec<Entry>| v.into_iter().map(|e| e.remote_id).collect::<Vec<_>>();
        // Once, everything; then only what came after.
        assert_eq!(ids(received_files_since(&all, None)), [1, 2]);
        assert_eq!(ids(received_files_since(&all, Some(5))), [2]);
        assert!(received_files_since(&all, Some(10)).is_empty());
        // Records only: where it is here stays here.
        assert!(
            received_files_since(&all, None).iter().all(|e| e
                .file
                .as_ref()
                .unwrap()
                .location
                .is_none())
        );
    }

    #[test]
    fn transcripts_carry_no_location_or_tick_stamp() {
        let mut e = Entry {
            ticked_ms: 9,
            ..entry(1, true, 1)
        };
        e.file = Some(threnody_core::history::FileNote {
            name: "a.png".into(),
            size: 1,
            location: Some("/home/me/a.png".into()),
            sensitive: false,
            album: 0,
            clip: None,
            forwarded: false,
        });
        let p = portable(e);
        assert_eq!(p.ticked_ms, 0);
        assert_eq!(p.file.unwrap().location, None);
    }

    #[test]
    fn orphan_ticks_are_bounded_and_taken_per_conversation() {
        let (a, b) = (
            ConversationId::Peer([1; 32]),
            ConversationId::Group([2; 16]),
        );
        let tick = |id| Tick {
            id,
            device: [3; 32],
            ..Tick::default()
        };
        let mut o = OrphanTicks::default();
        o.add(a, [tick(1), tick(2)]);
        o.add(b, [tick(1)]);
        // A second word on the same entry joins the first.
        o.add(
            a,
            [Tick {
                delivered: true,
                read_ms: 7,
                ..tick(1)
            }],
        );
        assert_eq!(o.len(), 3);
        let got = o.take(a);
        assert_eq!(got.len(), 2);
        assert!(got[0].delivered && got[0].read_ms == 7);
        assert!(o.take(a).is_empty());
        assert_eq!(o.take(b).len(), 1);
        o.add(a, (0..MAX_ORPHAN_TICKS as u64 + 10).map(tick));
        assert_eq!(o.len(), MAX_ORPHAN_TICKS);
        // The oldest went first.
        assert_eq!(o.take(a)[0].id, 10);
    }
}
