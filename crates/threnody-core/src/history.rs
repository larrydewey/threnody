//! Local-first message history (spec §6.4) with disappearing messages
//! (spec §6.1).
//!
//! One encrypted state file per conversation (`Home::save_state`, so as
//! protected as the identity). Expired entries are pruned whenever a
//! conversation is loaded or saved; the per-conversation timer travels with
//! each message (`AppMessage::Text::expires_in_s`) so both sides apply it.

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::store::Home;

/// Entries kept per conversation; the oldest are dropped first.
pub const MAX_ENTRIES: usize = 10_000;
const VERSION: u64 = 1;

/// Which conversation a message belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConversationId {
    /// A peer, by account id (or device key for peers without a chain).
    Peer([u8; 32]),
    Group([u8; 16]),
}

impl ConversationId {
    /// `0 ‖ id (32)` for a peer, `1 ‖ id (16)` for a group.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Peer(k) => [&[0u8][..], k].concat(),
            Self::Group(g) => [&[1u8][..], g].concat(),
        }
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        match b.split_first()? {
            (0, k) => k.try_into().ok().map(Self::Peer),
            (1, g) => g.try_into().ok().map(Self::Group),
            _ => None,
        }
    }

    fn state_name(&self) -> String {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        match self {
            Self::Peer(k) => format!("hist-p-{}", hex(k)),
            Self::Group(g) => format!("hist-g-{}", hex(g)),
        }
    }

    fn parse_state_name(name: &str) -> Option<Self> {
        let unhex = |s: &str| -> Option<Vec<u8>> {
            (0..s.len())
                .step_by(2)
                .map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect()
        };
        if let Some(h) = name.strip_prefix("hist-p-") {
            return unhex(h).and_then(|v| v.try_into().ok()).map(Self::Peer);
        }
        if let Some(h) = name.strip_prefix("hist-g-") {
            return unhex(h).and_then(|v| v.try_into().ok()).map(Self::Group);
        }
        None
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub at_ms: u64,
    pub outgoing: bool,
    /// The device that sent it (for incoming) or that we sent from.
    pub device: [u8; 32],
    pub text: String,
    pub offline: bool,
    pub expires_at_ms: Option<u64>,
    /// Set when the entry records a file transfer (then `text` is empty).
    pub file: Option<FileNote>,
    /// For outgoing entries: a local id the delivery acknowledgements
    /// refer to (0 = none).
    pub local_id: u64,
    /// An outgoing message the recipient acknowledged (Appendix C); for a
    /// group, every recipient.
    pub delivered: bool,
    /// When the recipient displayed this message (0 = not yet). Only
    /// meaningful on outgoing entries.
    pub read_ms: u64,
    /// For outgoing group messages: how many members it went to, and the
    /// devices that acknowledged it so far.
    pub recipients: u32,
    pub delivered_to: Vec<[u8; 32]>,
    /// For incoming entries: the sender's id for the message (0 = none),
    /// which a later "delete for everyone" names.
    pub remote_id: u64,
    /// When the text was last edited (0 = never).
    pub edited_ms: u64,
    /// Reactions: who (an account, or a device whose account isn't known)
    /// and which emoji. Anyone may add several different ones.
    pub reactions: Vec<([u8; 32], String)>,
}

/// Reaction limits: per person on one message, per message, and per emoji.
pub const MAX_REACTIONS_EACH: usize = 16;
pub const MAX_REACTIONS: usize = 256;
pub const MAX_EMOJI_BYTES: usize = 32;

/// Whether `emoji` is acceptable as a reaction: short, with no control or
/// whitespace-only content.
pub fn valid_emoji(emoji: &str) -> bool {
    !emoji.trim().is_empty()
        && emoji.len() <= MAX_EMOJI_BYTES
        && !emoji.chars().any(char::is_control)
}

impl Entry {
    /// Adds or removes `who`'s `emoji`; returns true if anything changed.
    /// Adding beyond the limits is ignored.
    pub fn react(&mut self, who: [u8; 32], emoji: &str, add: bool) -> bool {
        let has = self.reactions.iter().any(|(w, e)| *w == who && e == emoji);
        if add {
            let mine = self.reactions.iter().filter(|(w, _)| *w == who).count();
            if has
                || !valid_emoji(emoji)
                || mine >= MAX_REACTIONS_EACH
                || self.reactions.len() >= MAX_REACTIONS
            {
                return false;
            }
            self.reactions.push((who, emoji.to_owned()));
            true
        } else {
            self.reactions.retain(|(w, e)| !(*w == who && e == emoji));
            has
        }
    }

    /// The id other devices know this message by: ours if we sent it, the
    /// sender's otherwise (0 = none).
    pub fn message_id(&self) -> u64 {
        if self.outgoing {
            self.local_id
        } else {
            self.remote_id
        }
    }
}

/// A file sent or received. The contents aren't kept here, only where the
/// app stored them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileNote {
    pub name: String,
    pub size: u64,
    /// A local path or platform URI for the saved file, if any.
    pub location: Option<String>,
    /// Marked sensitive by its sender: shown covered until opened.
    pub sensitive: bool,
    /// Files sent together share an album id (0 = alone). An album's
    /// caption is the text of its first entry.
    pub album: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Disappearing-message timer for new messages, in seconds.
    pub timer_s: Option<u32>,
    entries: Vec<Entry>,
}

/// Whether two entries record the same message: outgoing ones by their
/// local id (or time and content), incoming ones by sender and content
/// within ten minutes (each device stamps its own receipt time).
fn same(a: &Entry, b: &Entry) -> bool {
    if a.outgoing != b.outgoing || a.device != b.device {
        return false;
    }
    let content =
        a.text == b.text && a.file.as_ref().map(|f| &f.name) == b.file.as_ref().map(|f| &f.name);
    if a.outgoing && a.local_id != 0 && b.local_id != 0 {
        return a.local_id == b.local_id;
    }
    content && a.at_ms.abs_diff(b.at_ms) <= 10 * 60 * 1000
}

impl History {
    /// Adds `incoming` entries that aren't already here, keeping time
    /// order. Returns how many were added.
    pub fn merge(&mut self, incoming: Vec<Entry>, now_ms: u64) -> usize {
        let mut added = 0;
        for e in incoming {
            if !self.entries.iter().any(|x| same(x, &e)) {
                self.entries.push(e);
                added += 1;
            }
        }
        if added > 0 {
            self.entries.sort_by_key(|e| e.at_ms);
            self.prune(now_ms);
        }
        added
    }

    /// Entries for a transcript: a conversation's (part of) history for
    /// our own other devices (Appendix J).
    pub fn transcript(entries: Vec<Entry>) -> Self {
        Self {
            timer_s: None,
            entries,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The last `n` entries, oldest first.
    pub fn recent(&self, n: usize) -> &[Entry] {
        &self.entries[self.entries.len().saturating_sub(n)..]
    }

    pub fn push(&mut self, e: Entry, now_ms: u64) {
        self.entries.push(e);
        self.prune(now_ms);
    }

    /// Drops expired entries and enforces [`MAX_ENTRIES`]. Returns true if
    /// anything was removed.
    pub fn prune(&mut self, now_ms: u64) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|e| e.expires_at_ms.is_none_or(|t| t > now_ms));
        if self.entries.len() > MAX_ENTRIES {
            let extra = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..extra);
        }
        self.entries.len() != before
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let size: usize = self
            .entries
            .iter()
            .map(|e| {
                e.text.len()
                    + 80
                    + e.file.as_ref().map_or(0, |f| {
                        f.name.len() + f.location.as_ref().map_or(0, String::len) + 32
                    })
            })
            .sum::<usize>()
            + 32;
        cbor::to_vec(size, |enc| {
            enc.map_len(2 + usize::from(self.timer_s.is_some()))?;
            enc.u8(0)?.u64(VERSION)?;
            if let Some(t) = self.timer_s {
                enc.u8(1)?.u32(t)?;
            }
            enc.u8(2)?.array_len(self.entries.len())?;
            for e in &self.entries {
                enc.map_len(
                    5 + usize::from(e.expires_at_ms.is_some())
                        + usize::from(e.file.is_some())
                        + usize::from(e.local_id != 0)
                        + usize::from(e.delivered)
                        + usize::from(e.recipients != 0)
                        + usize::from(!e.delivered_to.is_empty())
                        + usize::from(e.remote_id != 0)
                        + usize::from(e.edited_ms != 0)
                        + usize::from(!e.reactions.is_empty())
                        + usize::from(e.read_ms != 0),
                )?;
                enc.u8(0)?.u64(e.at_ms)?;
                enc.u8(1)?.bool(e.outgoing)?;
                enc.u8(2)?.bytes(&e.device)?;
                enc.u8(3)?.str(&e.text)?;
                enc.u8(4)?.bool(e.offline)?;
                if let Some(x) = e.expires_at_ms {
                    enc.u8(5)?.u64(x)?;
                }
                if let Some(f) = &e.file {
                    enc.u8(6)?.map_len(
                        2 + usize::from(f.location.is_some())
                            + usize::from(f.sensitive)
                            + usize::from(f.album != 0),
                    )?;
                    enc.u8(0)?.str(&f.name)?;
                    enc.u8(1)?.u64(f.size)?;
                    if let Some(l) = &f.location {
                        enc.u8(2)?.str(l)?;
                    }
                    if f.sensitive {
                        enc.u8(3)?.bool(true)?;
                    }
                    if f.album != 0 {
                        enc.u8(4)?.u64(f.album)?;
                    }
                }
                if e.local_id != 0 {
                    enc.u8(7)?.u64(e.local_id)?;
                }
                if e.delivered {
                    enc.u8(8)?.bool(true)?;
                }
                if e.recipients != 0 {
                    enc.u8(9)?.u32(e.recipients)?;
                }
                if !e.delivered_to.is_empty() {
                    enc.u8(10)?.bytes(&e.delivered_to.concat())?;
                }
                if e.remote_id != 0 {
                    enc.u8(11)?.u64(e.remote_id)?;
                }
                if e.edited_ms != 0 {
                    enc.u8(12)?.u64(e.edited_ms)?;
                }
                if e.read_ms != 0 {
                    enc.u8(14)?.u64(e.read_ms)?;
                }
                if !e.reactions.is_empty() {
                    enc.u8(13)?.array_len(e.reactions.len())?;
                    for (who, emoji) in &e.reactions {
                        enc.array_len(2)?.bytes(who)?.str(emoji)?;
                    }
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut ver, mut timer, mut entries) = (None, None, Vec::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => timer = Some(d.u32()?),
                2 => {
                    for _ in 0..d.array_len()? {
                        let (mut at, mut out, mut dev, mut text, mut off, mut exp, mut file) =
                            (None, None, None, None, None, None, None);
                        let (mut local_id, mut delivered) = (0, false);
                        let (mut recipients, mut delivered_to) = (0, Vec::new());
                        let (mut remote_id, mut edited_ms) = (0, 0);
                        let (mut reactions, mut read_ms) = (Vec::new(), 0);
                        read_map(d, |k, d| {
                            match k {
                                0 => at = Some(d.u64()?),
                                1 => out = Some(d.bool()?),
                                2 => dev = Some(fixed_bytes::<32>(d)?),
                                3 => text = Some(d.str()?.to_owned()),
                                4 => off = Some(d.bool()?),
                                5 => exp = Some(d.u64()?),
                                6 => file = Some(decode_file(d)?),
                                7 => local_id = d.u64()?,
                                8 => delivered = d.bool()?,
                                9 => recipients = d.u32()?,
                                10 => {
                                    let b = d.bytes()?;
                                    if b.len() % 32 != 0 {
                                        return Err(Error::Malformed("delivered devices"));
                                    }
                                    delivered_to = b.as_chunks::<32>().0.to_vec();
                                }
                                11 => remote_id = d.u64()?,
                                12 => edited_ms = d.u64()?,
                                14 => read_ms = d.u64()?,
                                13 => {
                                    for _ in 0..d.array_len()? {
                                        if d.array_len()? != 2 {
                                            return Err(Error::Malformed("reaction"));
                                        }
                                        let who = fixed_bytes::<32>(d)?;
                                        let emoji = d.str()?.to_owned();
                                        if reactions.len() < MAX_REACTIONS && valid_emoji(&emoji) {
                                            reactions.push((who, emoji));
                                        }
                                    }
                                }
                                _ => return Ok(false),
                            }
                            Ok(true)
                        })?;
                        entries.push(Entry {
                            at_ms: required(at, "time")?,
                            outgoing: required(out, "direction")?,
                            device: required(dev, "device")?,
                            text: required(text, "text")?,
                            offline: off.unwrap_or(false),
                            expires_at_ms: exp,
                            file,
                            local_id,
                            delivered,
                            recipients,
                            delivered_to,
                            remote_id,
                            edited_ms,
                            read_ms,
                            reactions,
                        });
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(VERSION) {
            return Err(Error::Malformed("history version"));
        }
        Ok(Self {
            timer_s: timer,
            entries,
        })
    }
}

fn decode_file(d: &mut Decoder<'_>) -> Result<FileNote> {
    let (mut name, mut size, mut location, mut sensitive, mut album) = (None, None, None, false, 0);
    read_map(d, |k, d| {
        match k {
            0 => name = Some(d.str()?.to_owned()),
            1 => size = Some(d.u64()?),
            2 => location = Some(d.str()?.to_owned()),
            3 => sensitive = d.bool()?,
            4 => album = d.u64()?,
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(FileNote {
        name: required(name, "file name")?,
        size: required(size, "file size")?,
        location,
        sensitive,
        album,
    })
}

impl Home {
    /// Loads a conversation, pruning expired messages (and persisting the
    /// pruned form so they are gone from disk too).
    pub fn load_history(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
    ) -> Result<History> {
        let mut h = match self.load_state(identity, &c.state_name())? {
            Some(b) => History::decode(&b)?,
            None => History::default(),
        };
        if h.prune(now_ms) {
            self.save_history(identity, c, &h)?;
        }
        Ok(h)
    }

    pub fn save_history(&self, identity: &Identity, c: ConversationId, h: &History) -> Result<()> {
        self.save_state(identity, &c.state_name(), &h.encode()?)
    }

    /// Appends one entry to a conversation.
    pub fn append_history(
        &self,
        identity: &Identity,
        c: ConversationId,
        e: Entry,
        now_ms: u64,
    ) -> Result<History> {
        let mut h = self.load_history(identity, c, now_ms)?;
        h.push(e, now_ms);
        self.save_history(identity, c, &h)?;
        Ok(h)
    }

    /// Records that `device` acknowledged the outgoing entry `local_id`.
    /// A 1:1 entry is then delivered; a group entry once every recipient
    /// has acknowledged. Returns false if nothing changed.
    pub fn mark_delivered(
        &self,
        identity: &Identity,
        c: ConversationId,
        local_id: u64,
        device: [u8; 32],
        now_ms: u64,
    ) -> Result<bool> {
        if local_id == 0 {
            return Ok(false);
        }
        let mut h = self.load_history(identity, c, now_ms)?;
        let Some(e) = h
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.outgoing && e.local_id == local_id)
        else {
            return Ok(false);
        };
        match c {
            ConversationId::Peer(_) if !e.delivered => e.delivered = true,
            ConversationId::Group(_) if !e.delivered_to.contains(&device) => {
                e.delivered_to.push(device);
                e.delivered = e.delivered_to.len() >= e.recipients as usize;
            }
            _ => return Ok(false),
        }
        self.save_history(identity, c, &h)?;
        Ok(true)
    }

    /// Merges entries from another of our devices into a conversation;
    /// returns how many were new.
    pub fn merge_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        entries: Vec<Entry>,
        now_ms: u64,
    ) -> Result<usize> {
        let mut h = self.load_history(identity, c, now_ms)?;
        let added = h.merge(entries, now_ms);
        if added > 0 {
            self.save_history(identity, c, &h)?;
        }
        Ok(added)
    }

    /// Moves `from`'s history into `into` (keeping time order) and deletes
    /// `from`. Used when a device's account becomes known. Returns true if
    /// anything moved.
    pub fn merge_history(
        &self,
        identity: &Identity,
        from: ConversationId,
        into: ConversationId,
        now_ms: u64,
    ) -> Result<bool> {
        if from == into {
            return Ok(false);
        }
        let Some(b) = self.load_state(identity, &from.state_name())? else {
            return Ok(false);
        };
        let src = History::decode(&b)?;
        let mut dst = self.load_history(identity, into, now_ms)?;
        dst.entries.extend(src.entries);
        dst.entries.sort_by_key(|e| e.at_ms);
        if dst.timer_s.is_none() {
            dst.timer_s = src.timer_s;
        }
        dst.prune(now_ms);
        self.save_history(identity, into, &dst)?;
        self.remove_state(&from.state_name())?;
        Ok(true)
    }

    /// Deletes the entries `pick` chooses from a conversation; returns how
    /// many went.
    pub fn delete_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<usize> {
        let mut h = self.load_history(identity, c, now_ms)?;
        let before = h.entries.len();
        h.entries.retain(|e| !pick(e));
        let gone = before - h.entries.len();
        if gone > 0 {
            self.save_history(identity, c, &h)?;
        }
        Ok(gone)
    }

    /// Adds or removes `who`'s `emoji` on the entries `pick` chooses;
    /// returns how many changed.
    #[allow(clippy::too_many_arguments)]
    pub fn react_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        who: [u8; 32],
        emoji: &str,
        add: bool,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<usize> {
        let mut h = self.load_history(identity, c, now_ms)?;
        let n = h
            .entries
            .iter_mut()
            .filter(|e| pick(e))
            .map(|e| e.react(who, emoji, add))
            .filter(|changed| *changed)
            .count();
        if n > 0 {
            self.save_history(identity, c, &h)?;
        }
        Ok(n)
    }

    /// Replaces the text of the entries `pick` chooses; returns how many.
    pub fn edit_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        body: &str,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<usize> {
        let mut h = self.load_history(identity, c, now_ms)?;
        let mut n = 0;
        for e in h.entries.iter_mut().filter(|e| e.file.is_none() && pick(e)) {
            e.text = body.to_owned();
            e.edited_ms = now_ms;
            n += 1;
        }
        if n > 0 {
            self.save_history(identity, c, &h)?;
        }
        Ok(n)
    }

    /// Marks the entries `pick` chooses as read at `now_ms`; returns how
    /// many changed. Only our outgoing entries are meant to get this.
    pub fn mark_read_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<usize> {
        let mut h = self.load_history(identity, c, now_ms)?;
        let mut n = 0;
        for e in h.entries.iter_mut().filter(|e| pick(e) && e.read_ms == 0) {
            e.read_ms = now_ms;
            n += 1;
        }
        if n > 0 {
            self.save_history(identity, c, &h)?;
        }
        Ok(n)
    }

    /// Deletes a conversation's history.
    pub fn delete_history(&self, c: ConversationId) -> Result<()> {
        self.remove_state(&c.state_name())
    }

    /// Every conversation with stored history.
    pub fn conversations(&self) -> Vec<ConversationId> {
        let Ok(rd) = std::fs::read_dir(self.dir()) else {
            return vec![];
        };
        rd.filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                ConversationId::parse_state_name(name.strip_suffix(".state")?)
            })
            .collect()
    }

    /// Prunes expired messages in every conversation; returns how many
    /// conversations changed.
    pub fn sweep_history(&self, identity: &Identity, now_ms: u64) -> usize {
        self.conversations()
            .into_iter()
            .filter(|c| {
                self.load_state(identity, &c.state_name())
                    .ok()
                    .flatten()
                    .and_then(|b| History::decode(&b).ok())
                    .is_some_and(|mut h| {
                        h.prune(now_ms) && self.save_history(identity, *c, &h).is_ok()
                    })
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(at: u64, text: &str, exp: Option<u64>) -> Entry {
        Entry {
            at_ms: at,
            outgoing: at.is_multiple_of(2),
            device: [7; 32],
            text: text.into(),
            offline: false,
            expires_at_ms: exp,
            file: None,
            local_id: 0,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
            remote_id: 0,
            edited_ms: 0,
            read_ms: 0,
            reactions: Vec::new(),
        }
    }

    #[test]
    fn history_persists_encrypted_and_disappears_on_time() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let c = ConversationId::Peer([1; 32]);
        home.append_history(&id, c, entry(1, "keep me", None), 1)
            .unwrap();
        home.append_history(&id, c, entry(2, "secret plans", Some(100)), 2)
            .unwrap();
        let h = home.load_history(&id, c, 50).unwrap();
        assert_eq!(h.entries().len(), 2);
        let raw = std::fs::read(dir.path().join(format!("{}.state", c.state_name()))).unwrap();
        assert!(
            !raw.windows(6).any(|w| w == b"secret"),
            "history stored in clear"
        );

        // After expiry the message is gone, including from disk.
        let h = home.load_history(&id, c, 101).unwrap();
        assert_eq!(
            h.entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            ["keep me"]
        );
        let raw = home.load_state(&id, &c.state_name()).unwrap().unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"));

        assert_eq!(home.conversations(), vec![c]);
        let acct = ConversationId::Peer([9; 32]);
        home.append_history(&id, acct, entry(0, "earlier", None), 0)
            .unwrap();
        assert!(home.merge_history(&id, c, acct, 102).unwrap());
        let merged = home.load_history(&id, acct, 102).unwrap();
        assert_eq!(
            merged
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            ["earlier", "keep me"]
        );
        assert_eq!(home.conversations(), vec![acct]);
        home.merge_history(&id, acct, c, 102).unwrap();
        home.merge_history(&id, c, acct, 102).unwrap();
        home.append_history(
            &id,
            ConversationId::Group([2; 16]),
            entry(3, "g", Some(5)),
            3,
        )
        .unwrap();
        assert_eq!(home.sweep_history(&id, 10), 1);

        let sent = Entry {
            local_id: 77,
            ..entry(200, "sent", None)
        };
        assert!(sent.outgoing);
        home.append_history(&id, acct, sent, 200).unwrap();
        assert!(!home.mark_delivered(&id, acct, 78, [1; 32], 201).unwrap());
        assert!(home.mark_delivered(&id, acct, 77, [1; 32], 201).unwrap());
        assert!(
            !home.mark_delivered(&id, acct, 77, [2; 32], 201).unwrap(),
            "once"
        );
        let h = home.load_history(&id, acct, 201).unwrap();
        let e = h.entries().last().unwrap();
        assert!(e.delivered && e.local_id == 77);

        // A group entry is delivered once every recipient acknowledged.
        let g = ConversationId::Group([3; 16]);
        let to_two = Entry {
            local_id: 5,
            recipients: 2,
            ..entry(300, "to the group", None)
        };
        home.append_history(&id, g, to_two, 300).unwrap();
        let get = || {
            home.load_history(&id, g, 301)
                .unwrap()
                .entries()
                .last()
                .cloned()
                .unwrap()
        };
        assert!(home.mark_delivered(&id, g, 5, [1; 32], 301).unwrap());
        assert!(
            !home.mark_delivered(&id, g, 5, [1; 32], 301).unwrap(),
            "same device twice"
        );
        assert!(!get().delivered && get().delivered_to == [[1; 32]]);
        assert!(home.mark_delivered(&id, g, 5, [2; 32], 301).unwrap());
        assert!(get().delivered && get().delivered_to.len() == 2 && get().recipients == 2);
    }

    #[test]
    fn merging_skips_messages_already_here() {
        let mut h = History::default();
        let sent = Entry {
            local_id: 9,
            ..entry(1000, "hi", None)
        };
        let got = Entry {
            device: [5; 32],
            outgoing: false,
            ..entry(2001, "hello", None)
        };
        h.push(sent.clone(), 0);
        h.push(got.clone(), 0);
        // The same messages as another device recorded them: the incoming
        // one with that device's own receipt time.
        let theirs = vec![
            sent.clone(),
            Entry {
                at_ms: 2001 + 60_000,
                ..got.clone()
            },
            Entry {
                local_id: 10,
                ..entry(500, "earlier, from the other device", None)
            },
        ];
        assert_eq!(h.merge(theirs, 0), 1);
        assert_eq!(h.entries().len(), 3);
        assert_eq!(
            h.entries()[0].text,
            "earlier, from the other device",
            "time order"
        );
        // Same text much later is a different message.
        let again = Entry {
            at_ms: 2001 + 3_600_000,
            ..got
        };
        assert_eq!(h.merge(vec![again], 0), 1);
        for c in [
            ConversationId::Peer([3; 32]),
            ConversationId::Group([4; 16]),
        ] {
            assert_eq!(ConversationId::from_bytes(&c.to_bytes()), Some(c));
        }
        assert_eq!(ConversationId::from_bytes(&[2, 0]), None);
    }

    #[test]
    fn timer_and_caps() {
        let mut h = History {
            timer_s: Some(30),
            ..History::default()
        };
        for i in 0..(MAX_ENTRIES as u64 + 5) {
            h.push(entry(i, "x", None), 0);
        }
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(h.entries()[0].at_ms, 5, "oldest dropped first");
        assert_eq!(h.recent(2).len(), 2);
        h.push(
            Entry {
                file: Some(FileNote {
                    name: "photo.jpg".into(),
                    size: 1234,
                    location: Some("content://media/1".into()),
                    sensitive: true,
                    album: 77,
                }),
                ..entry(MAX_ENTRIES as u64 + 6, "", None)
            },
            0,
        );
        h.push(
            Entry {
                file: Some(FileNote {
                    name: "notes.txt".into(),
                    size: 0,
                    location: None,
                    sensitive: false,
                    album: 0,
                }),
                ..entry(MAX_ENTRIES as u64 + 7, "", None)
            },
            0,
        );
        let again = History::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(again, h);
    }

    #[test]
    fn reactions_are_several_per_person_within_limits() {
        let mut e = entry(1, "hi", None);
        let (a, b) = ([1u8; 32], [2u8; 32]);
        assert!(e.react(a, "👍", true) && e.react(a, "❤️", true) && e.react(b, "👍", true));
        assert!(!e.react(a, "👍", true), "no duplicates");
        assert!(
            !e.react(a, "", true) && !e.react(a, "\n", true) && !e.react(a, &"x".repeat(40), true)
        );
        assert_eq!(e.reactions.len(), 3);
        assert!(e.react(a, "👍", false) && !e.react(a, "👍", false));
        assert_eq!(
            e.reactions,
            vec![(a, "❤️".to_owned()), (b, "👍".to_owned())]
        );
        for i in 0..40 {
            e.react(a, &format!("e{i}"), true);
        }
        assert_eq!(
            e.reactions.iter().filter(|(w, _)| *w == a).count(),
            MAX_REACTIONS_EACH
        );
        // They survive storage.
        let mut h = History::default();
        h.push(e.clone(), 0);
        assert_eq!(
            History::decode(&h.encode().unwrap()).unwrap().entries()[0].reactions,
            e.reactions
        );
    }
}
