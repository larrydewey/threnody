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
    /// For outgoing group messages: how many members it went to, and the
    /// devices that acknowledged it so far.
    pub recipients: u32,
    pub delivered_to: Vec<[u8; 32]>,
}

/// A file sent or received. The contents aren't kept here, only where the
/// app stored them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileNote {
    pub name: String,
    pub size: u64,
    /// A local path or platform URI for the saved file, if any.
    pub location: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Disappearing-message timer for new messages, in seconds.
    pub timer_s: Option<u32>,
    entries: Vec<Entry>,
}

impl History {
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
                        + usize::from(!e.delivered_to.is_empty()),
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
                    enc.u8(6)?.map_len(2 + usize::from(f.location.is_some()))?;
                    enc.u8(0)?.str(&f.name)?;
                    enc.u8(1)?.u64(f.size)?;
                    if let Some(l) = &f.location {
                        enc.u8(2)?.str(l)?;
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
    let (mut name, mut size, mut location) = (None, None, None);
    read_map(d, |k, d| {
        match k {
            0 => name = Some(d.str()?.to_owned()),
            1 => size = Some(d.u64()?),
            2 => location = Some(d.str()?.to_owned()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(FileNote {
        name: required(name, "file name")?,
        size: required(size, "file size")?,
        location,
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
                }),
                ..entry(MAX_ENTRIES as u64 + 7, "", None)
            },
            0,
        );
        let again = History::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(again, h);
    }
}
