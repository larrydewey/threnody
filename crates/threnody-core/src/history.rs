//! Local-first message history (spec §6.4) with disappearing messages
//! (spec §6.1).
//!
//! One encrypted state file per conversation (`Home::save_state`, so as
//! protected as the identity). Expired entries are pruned whenever a
//! conversation is loaded or saved; the per-conversation timer travels with
//! each message (`AppMessage::Text::expires_in_s`) so both sides apply it.

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::kdf;
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::message::{Clip, Mention, Quote};
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

    /// The name of its history file: keyed, so a listing of the data
    /// directory shows neither whom we talk to nor which groups we are in.
    ///
    /// ```text
    /// name = "hist-" || hex(KDF("history file name", KDF("history file name key", seed), 0 ‖ account | 1 ‖ group))
    /// ```
    fn state_name(&self, identity: &Identity) -> String {
        let key = Zeroizing::new(kdf::derive::<32>(
            kdf::label::HISTORY_NAME_KEY,
            &[&identity.seed()[..]],
        ));
        let name: [u8; 32] =
            kdf::derive(kdf::label::HISTORY_FILE_NAME, &[&key[..], &self.to_bytes()]);
        format!("{KEYED_PREFIX}{}", hex(&name))
    }

    /// The name its history file had before 0.18.0, which named the
    /// conversation: `hist-p-<account or device>`, `hist-g-<group>`. Only
    /// read (and moved, see [`Home::migrate_history_names`]), and written
    /// inside backups so that older versions restore them.
    fn legacy_state_name(&self) -> String {
        match self {
            Self::Peer(k) => format!("hist-p-{}", hex(k)),
            Self::Group(g) => format!("hist-g-{}", hex(g)),
        }
    }

    fn parse_legacy_state_name(name: &str) -> Option<Self> {
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

const KEYED_PREFIX: &str = "hist-";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Whether `name` is a history file's keyed name ([`ConversationId::state_name`]).
fn is_keyed_name(name: &str) -> bool {
    name.strip_prefix(KEYED_PREFIX).is_some_and(|h| {
        h.len() == 64
            && h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// Whether the state file `name` holds a conversation's history, under
/// either name.
pub(crate) fn is_history_name(name: &str) -> bool {
    is_keyed_name(name) || ConversationId::parse_legacy_state_name(name).is_some()
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
    /// For outgoing entries: when the recipient displayed it (0 = not
    /// yet). For incoming ones: nonzero once we reported it read, so it is
    /// reported once (the network layer stores 1, so no reading time is
    /// kept at rest).
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
    /// The message this one replies to.
    pub quote: Option<Quote>,
    /// Who it mentions (group messages).
    pub mentions: Vec<Mention>,
    /// A poll (its question is `text`).
    pub poll: Option<Poll>,
    /// Forwarded from another chat.
    pub forwarded: bool,
    /// A list shared here (see `crate::lists`): the entry opens it.
    pub list: Option<[u8; 16]>,
}

/// Most options a poll has, and longest question and option, in
/// characters.
pub const MAX_POLL_OPTIONS: usize = 12;
pub const MAX_POLL_QUESTION: usize = 300;
pub const MAX_POLL_OPTION: usize = 100;

/// A poll: its options, whether several may be chosen, the votes (by
/// voter: an account, or a device whose account isn't known), and
/// whether its author closed it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Poll {
    pub options: Vec<String>,
    pub multi: bool,
    pub votes: Vec<([u8; 32], Vec<u32>)>,
    pub closed: bool,
}

impl Poll {
    /// A poll of `options` (trimmed and cut short), if there are 2 to
    /// [`MAX_POLL_OPTIONS`] non-empty ones.
    pub fn new(options: Vec<String>, multi: bool) -> Option<Self> {
        let options: Vec<String> = options
            .iter()
            .map(|o| o.trim().chars().take(MAX_POLL_OPTION).collect::<String>())
            .filter(|o| !o.is_empty())
            .collect();
        (2..=MAX_POLL_OPTIONS)
            .contains(&options.len())
            .then_some(Self {
                options,
                multi,
                votes: Vec::new(),
                closed: false,
            })
    }

    /// Sets `who`'s choices (none: takes the vote back). Choices out of
    /// range are ignored, and a single-choice poll keeps the first. Returns
    /// true if that changed anything; a closed poll takes no votes.
    pub fn vote(&mut self, who: [u8; 32], choices: &[u32]) -> bool {
        if self.closed {
            return false;
        }
        let mut c: Vec<u32> = choices
            .iter()
            .copied()
            .filter(|i| (*i as usize) < self.options.len())
            .collect();
        c.sort_unstable();
        c.dedup();
        if !self.multi {
            c.truncate(1);
        }
        let before = self
            .votes
            .iter()
            .find(|(w, _)| *w == who)
            .map(|(_, v)| v.clone());
        if before.as_ref() == Some(&c) || (before.is_none() && c.is_empty()) {
            return false;
        }
        self.votes.retain(|(w, _)| *w != who);
        if !c.is_empty() {
            self.votes.push((who, c));
        }
        true
    }

    /// How many voters chose each option.
    pub fn counts(&self) -> Vec<u32> {
        let mut n = vec![0u32; self.options.len()];
        for (_, v) in &self.votes {
            for i in v {
                if let Some(x) = n.get_mut(*i as usize) {
                    *x += 1;
                }
            }
        }
        n
    }
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
        // A sibling's message is ours, under the sender's id.
        if self.outgoing && self.local_id != 0 {
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
    /// A voice or video message, played in the chat.
    pub clip: Option<Clip>,
    /// Forwarded from another chat.
    pub forwarded: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Disappearing-message timer for new messages, in seconds.
    pub timer_s: Option<u32>,
    entries: Vec<Entry>,
    /// Pinned messages, by [`Entry::message_id`], oldest pin first; at
    /// most [`MAX_PINS`].
    pinned: Vec<u64>,
    /// When new messages here should notify.
    pub notify: Notify,
    /// What the user started writing here and hasn't sent.
    pub draft: String,
    /// When the user last looked at the conversation: incoming messages
    /// after it are unread (on this device).
    pub seen_ms: u64,
}

/// When a conversation's new messages notify.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Notify {
    #[default]
    All,
    /// Only messages that mention us (in groups), or reply to us.
    Mentions,
    Off,
}

impl Notify {
    pub fn to_u8(self) -> u8 {
        match self {
            Self::All => 0,
            Self::Mentions => 1,
            Self::Off => 2,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Mentions,
            2 => Self::Off,
            _ => Self::All,
        }
    }
}

/// Most messages pinned in one conversation; pinning another unpins the
/// oldest pin.
pub const MAX_PINS: usize = 3;

/// Whether two entries record the same message: outgoing ones by their
/// local id (or time and content), incoming ones by sender and content
/// within ten minutes (each device stamps its own receipt time).
fn decode_poll(d: &mut Decoder<'_>) -> Result<Poll> {
    if d.array_len()? != 4 {
        return Err(Error::Malformed("poll"));
    }
    let mut options = Vec::new();
    for _ in 0..d.array_len()? {
        let o = d.str()?.to_owned();
        if options.len() < MAX_POLL_OPTIONS {
            options.push(o);
        }
    }
    let (multi, closed) = (d.bool()?, d.bool()?);
    let mut votes = Vec::new();
    for _ in 0..d.array_len()? {
        if d.array_len()? != 2 {
            return Err(Error::Malformed("poll vote"));
        }
        let who = fixed_bytes::<32>(d)?;
        let mut v = Vec::new();
        for _ in 0..d.array_len()? {
            let i = d.u32()?;
            if (i as usize) < MAX_POLL_OPTIONS {
                v.push(i);
            }
        }
        votes.push((who, v));
    }
    Ok(Poll {
        options,
        multi,
        votes,
        closed,
    })
}

/// Held while a conversation's history is read, changed and written
/// back, so concurrent changes (a message sent while an acknowledgement
/// arrives) don't lose each other. Re-entrant on one thread.
pub fn history_lock() -> HistoryLock {
    use std::cell::Cell;
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    thread_local!(static DEPTH: Cell<u32> = const { Cell::new(0) });
    let guard = if DEPTH.get() == 0 {
        Some(
            LOCK.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    } else {
        None
    };
    DEPTH.set(DEPTH.get() + 1);
    HistoryLock {
        _guard: guard,
        depth: &DEPTH,
    }
}

/// See [`history_lock`].
pub struct HistoryLock {
    _guard: Option<std::sync::MutexGuard<'static, ()>>,
    depth: &'static std::thread::LocalKey<std::cell::Cell<u32>>,
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        self.depth.with(|d| d.set(d.get() - 1));
    }
}

/// What a quote of `e` shows: its text, or a file's name.
pub fn snippet(e: &Entry) -> &str {
    match &e.file {
        Some(f) if e.text.is_empty() => &f.name,
        _ => &e.text,
    }
}

impl Quote {
    /// A quote of `e`, to reply to it with.
    pub fn of(e: &Entry) -> Self {
        Self::new(e.message_id(), e.device, snippet(e))
    }
}

/// Whether `q` quotes `e`: by the sender's id for the message, or, for a
/// message too old to have one, by sender and text.
pub fn quotes(q: &Quote, e: &Entry) -> bool {
    if q.id != 0 {
        return e.message_id() == q.id;
    }
    e.device == q.author && !q.text.is_empty() && snippet(e).starts_with(q.text.as_str())
}

fn same(a: &Entry, b: &Entry) -> bool {
    if a.outgoing != b.outgoing || a.device != b.device {
        return false;
    }
    let content =
        a.text == b.text && a.file.as_ref().map(|f| &f.name) == b.file.as_ref().map(|f| &f.name);
    if a.outgoing && a.local_id != 0 && b.local_id != 0 {
        return a.local_id == b.local_id;
    }
    // The sender's id for the message, where both have it: an outgoing
    // one's local id is that id, and a sibling records it as remote.
    let id = |e: &Entry| {
        if e.local_id != 0 {
            e.local_id
        } else {
            e.remote_id
        }
    };
    if id(a) != 0 && id(b) != 0 {
        return id(a) == id(b);
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

    /// Takes in what `newer`, the copy of this conversation an older
    /// version kept under its old name after the upgrade, holds: its new
    /// entries and pins, and what it set of the timer, notifications and
    /// draft. It ran after this copy was last written (it never touches
    /// keyed files), so what it set wins; what it left at the default
    /// (no timer, notify all, no draft) leaves ours.
    fn absorb_newer(&mut self, newer: History, now_ms: u64) {
        self.merge(newer.entries, now_ms);
        if newer.timer_s.is_some() {
            self.timer_s = newer.timer_s;
        }
        if !newer.draft.is_empty() {
            self.draft = newer.draft;
        }
        if newer.notify != Notify::All {
            self.notify = newer.notify;
        }
        self.seen_ms = self.seen_ms.max(newer.seen_ms);
        for p in newer.pinned {
            if !self.pinned.contains(&p) && self.entries.iter().any(|e| e.message_id() == p) {
                self.pinned.push(p);
            }
        }
        let extra = self.pinned.len().saturating_sub(MAX_PINS);
        self.pinned.drain(..extra);
    }

    /// Entries for a transcript: a conversation's (part of) history for
    /// our own other devices (Appendix J).
    pub fn transcript(entries: Vec<Entry>) -> Self {
        Self {
            timer_s: None,
            entries,
            pinned: Vec::new(),
            notify: Notify::All,
            draft: String::new(),
            seen_ms: 0,
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
        let expired: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| e.expires_at_ms.is_some_and(|t| t <= now_ms))
            .cloned()
            .collect();
        self.entries
            .retain(|e| e.expires_at_ms.is_none_or(|t| t > now_ms));
        self.forget_quotes(&expired);
        if self.entries.len() > MAX_ENTRIES {
            let extra = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..extra);
        }
        self.entries.len() != before
    }

    /// Incoming messages since the user last looked. History from before
    /// this was kept (`seen_ms` 0) counts as read.
    pub fn unread(&self) -> impl Iterator<Item = &Entry> {
        let seen = if self.seen_ms == 0 {
            u64::MAX
        } else {
            self.seen_ms
        };
        self.entries
            .iter()
            .filter(move |e| !e.outgoing && e.at_ms > seen)
    }

    /// Pinned messages, oldest pin first.
    pub fn pinned(&self) -> &[u64] {
        &self.pinned
    }

    /// Pins (or with `pin` false, unpins) message `id`, if it's here.
    /// Returns true if that changed anything.
    pub fn pin(&mut self, id: u64, pin: bool) -> bool {
        let had = self.pinned.contains(&id);
        if !pin {
            self.pinned.retain(|p| *p != id);
            return had;
        }
        if had || id == 0 || !self.entries.iter().any(|e| e.message_id() == id) {
            return false;
        }
        self.pinned.push(id);
        if self.pinned.len() > MAX_PINS {
            self.pinned.remove(0);
        }
        true
    }

    /// Forgets what referred to the messages `gone`: replies' quotes of
    /// them, and their pins. A quote mustn't outlive its message.
    pub fn forget_quotes(&mut self, gone: &[Entry]) {
        if gone.is_empty() {
            return;
        }
        let ids: Vec<u64> = gone.iter().map(Entry::message_id).collect();
        self.pinned.retain(|p| !ids.contains(p));
        for e in &mut self.entries {
            if let Some(q) = &mut e.quote
                && gone.iter().any(|g| quotes(q, g))
            {
                q.text.clear();
            }
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        self.encode_with(None)
    }

    /// As stored in conversation `c`'s file: with `c` in it (key 7), which
    /// its keyed name no longer tells.
    fn encode_stored(&self, c: ConversationId) -> Result<Vec<u8>> {
        self.encode_with(Some(c))
    }

    fn encode_with(&self, conversation: Option<ConversationId>) -> Result<Vec<u8>> {
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
            enc.map_len(
                2 + usize::from(self.timer_s.is_some())
                    + usize::from(!self.pinned.is_empty())
                    + usize::from(self.notify != Notify::All)
                    + usize::from(!self.draft.is_empty())
                    + usize::from(self.seen_ms != 0)
                    + usize::from(conversation.is_some()),
            )?;
            enc.u8(0)?.u64(VERSION)?;
            if let Some(c) = conversation {
                enc.u8(7)?.bytes(&c.to_bytes())?;
            }
            if let Some(t) = self.timer_s {
                enc.u8(1)?.u32(t)?;
            }
            if self.notify != Notify::All {
                enc.u8(4)?.u8(self.notify.to_u8())?;
            }
            if !self.draft.is_empty() {
                enc.u8(5)?.str(&self.draft)?;
            }
            if self.seen_ms != 0 {
                enc.u8(6)?.u64(self.seen_ms)?;
            }
            if !self.pinned.is_empty() {
                enc.u8(3)?.array_len(self.pinned.len())?;
                for p in &self.pinned {
                    enc.u64(*p)?;
                }
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
                        + usize::from(e.read_ms != 0)
                        + usize::from(e.quote.is_some())
                        + usize::from(!e.mentions.is_empty())
                        + usize::from(e.poll.is_some())
                        + usize::from(e.forwarded)
                        + usize::from(e.list.is_some()),
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
                            + usize::from(f.album != 0)
                            + usize::from(f.clip.is_some())
                            + usize::from(f.forwarded),
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
                    if let Some(c) = f.clip {
                        enc.u8(5)?.array_len(2)?.bool(c.video)?.u32(c.duration_ms)?;
                    }
                    if f.forwarded {
                        enc.u8(6)?.bool(true)?;
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
                if let Some(q) = &e.quote {
                    enc.u8(15)?
                        .array_len(3)?
                        .u64(q.id)?
                        .bytes(&q.author)?
                        .str(&q.text)?;
                }
                if e.forwarded {
                    enc.u8(18)?.bool(true)?;
                }
                if let Some(l) = &e.list {
                    enc.u8(19)?.bytes(l)?;
                }
                if let Some(p) = &e.poll {
                    enc.u8(17)?.array_len(4)?;
                    enc.array_len(p.options.len())?;
                    for o in &p.options {
                        enc.str(o)?;
                    }
                    enc.bool(p.multi)?.bool(p.closed)?;
                    enc.array_len(p.votes.len())?;
                    for (who, v) in &p.votes {
                        enc.array_len(2)?.bytes(who)?.array_len(v.len())?;
                        for i in v {
                            enc.u32(*i)?;
                        }
                    }
                }
                if !e.mentions.is_empty() {
                    enc.u8(16)?.array_len(e.mentions.len())?;
                    for m in &e.mentions {
                        enc.array_len(3)?.u32(m.start)?.u32(m.len)?.bytes(&m.who)?;
                    }
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
        Self::decode_stored(b).map(|(h, _)| h)
    }

    /// A conversation's file: the history, and the conversation it says
    /// it is (files from before 0.18.0 don't).
    fn decode_stored(b: &[u8]) -> Result<(Self, Option<ConversationId>)> {
        let mut conversation = None;
        let mut dec = Decoder::new(b);
        let (mut ver, mut timer, mut entries) = (None, None, Vec::new());
        let mut pinned = Vec::new();
        let mut notify = Notify::All;
        let mut draft = String::new();
        let mut seen_ms = 0;
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
                        let mut quote = None;
                        let mut mentions = Vec::new();
                        let mut poll = None;
                        let mut forwarded = false;
                        let mut list = None;
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
                                17 => poll = Some(decode_poll(d)?),
                                18 => forwarded = d.bool()?,
                                19 => list = Some(fixed_bytes::<16>(d)?),
                                16 => {
                                    for _ in 0..d.array_len()? {
                                        if d.array_len()? != 3 {
                                            return Err(Error::Malformed("mention"));
                                        }
                                        let (start, len) = (d.u32()?, d.u32()?);
                                        let who = fixed_bytes::<32>(d)?;
                                        mentions.push(Mention { start, len, who });
                                    }
                                }
                                15 => {
                                    if d.array_len()? != 3 {
                                        return Err(Error::Malformed("quote"));
                                    }
                                    let id = d.u64()?;
                                    let author = fixed_bytes::<32>(d)?;
                                    quote = Some(Quote::new(id, author, d.str()?));
                                }
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
                            quote,
                            mentions,
                            poll,
                            forwarded,
                            list,
                        });
                    }
                }
                4 => notify = Notify::from_u8(d.u8()?),
                5 => draft = d.str()?.to_owned(),
                6 => seen_ms = d.u64()?,
                7 => {
                    conversation = Some(
                        ConversationId::from_bytes(d.bytes()?)
                            .ok_or(Error::Malformed("history conversation"))?,
                    )
                }
                3 => {
                    for _ in 0..d.array_len()? {
                        let p = d.u64()?;
                        if pinned.len() < MAX_PINS {
                            pinned.push(p);
                        }
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
        Ok((
            Self {
                timer_s: timer,
                entries,
                pinned,
                notify,
                draft,
                seen_ms,
            },
            conversation,
        ))
    }
}

fn decode_file(d: &mut Decoder<'_>) -> Result<FileNote> {
    let (mut name, mut size, mut location, mut sensitive, mut album) = (None, None, None, false, 0);
    let mut clip = None;
    let mut forwarded = false;
    read_map(d, |k, d| {
        match k {
            0 => name = Some(d.str()?.to_owned()),
            1 => size = Some(d.u64()?),
            2 => location = Some(d.str()?.to_owned()),
            3 => sensitive = d.bool()?,
            4 => album = d.u64()?,
            6 => forwarded = d.bool()?,
            5 => {
                if d.array_len()? != 2 {
                    return Err(Error::Malformed("clip"));
                }
                clip = Some(Clip {
                    video: d.bool()?,
                    duration_ms: d.u32()?,
                });
            }
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
        clip,
        forwarded,
    })
}

/// A message search: whitespace-separated terms (or `"quoted phrases"`),
/// all of which must appear, ignoring case, in a message's text or file
/// name. Search runs over the decrypted history in memory; no index of
/// message contents is ever written to disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    terms: Vec<String>,
}

/// Lowercases `s` for matching, so "Ä" finds "ä" and "STRASSE" finds
/// "strasse".
fn fold(s: &str) -> String {
    s.to_lowercase()
}

impl Query {
    pub fn new(q: &str) -> Self {
        let mut terms = Vec::new();
        let mut rest = q;
        while let Some(start) = rest.find(|c: char| !c.is_whitespace()) {
            rest = &rest[start..];
            let (term, next) = match rest.strip_prefix('"') {
                Some(inner) => match inner.find('"') {
                    Some(end) => (&inner[..end], &inner[end + 1..]),
                    None => (inner, ""),
                },
                None => {
                    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    (&rest[..end], &rest[end..])
                }
            };
            let term = fold(term.trim());
            if !term.is_empty() && !terms.contains(&term) {
                terms.push(term);
            }
            rest = next;
        }
        Self { terms }
    }

    /// No terms: matches nothing (an empty search shows no results).
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub fn terms(&self) -> &[String] {
        &self.terms
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if self.is_empty() {
            return false;
        }
        let text = fold(&e.text);
        let name = e.file.as_ref().map(|f| fold(&f.name)).unwrap_or_default();
        self.terms
            .iter()
            .all(|t| text.contains(t.as_str()) || name.contains(t.as_str()))
    }
}

/// One search result: the conversation and the matching entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub conversation: ConversationId,
    pub entry: Entry,
}

impl History {
    /// The entries matching `q`, newest first, at most `limit`.
    pub fn search(&self, q: &Query, limit: usize) -> Vec<&Entry> {
        self.entries
            .iter()
            .rev()
            .filter(|e| q.matches(e))
            .take(limit)
            .collect()
    }
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
        let _lock = history_lock();
        let mut h = self.read_history(identity, c)?.unwrap_or_default();
        if h.prune(now_ms) {
            self.save_history(identity, c, &h)?;
        }
        Ok(h)
    }

    pub fn save_history(&self, identity: &Identity, c: ConversationId, h: &History) -> Result<()> {
        self.save_state(identity, &c.state_name(identity), &h.encode_stored(c)?)
    }

    /// `c`'s history as stored, if any, first moving it from its older
    /// name if it is still there.
    fn read_history(&self, identity: &Identity, c: ConversationId) -> Result<Option<History>> {
        self.migrate_conversation(identity, c)?;
        match self.load_state(identity, &c.state_name(identity))? {
            Some(b) => Ok(Some(History::decode_stored(&b)?.0)),
            None => Ok(None),
        }
    }

    /// Moves `c`'s history from the name it had before 0.18.0 to its
    /// keyed one ([`ConversationId::state_name`]). A file is already there
    /// if an older version ran here after the upgrade and started the
    /// conversation's file anew: then the two are merged. The keyed file
    /// is written (atomically) before the old one goes, so an interruption
    /// loses nothing and the next run finishes the move. Returns whether
    /// there was anything to move.
    fn migrate_conversation(&self, identity: &Identity, c: ConversationId) -> Result<bool> {
        let old = c.legacy_state_name();
        if !self.state_path(&old)?.exists() {
            return Ok(false);
        }
        let _lock = history_lock();
        let Some(b) = self.load_state(identity, &old)? else {
            return Ok(false);
        };
        let older = History::decode(&b)?;
        let name = c.state_name(identity);
        let h = match self.load_state(identity, &name)? {
            None => older,
            Some(b) => {
                let mut h = History::decode_stored(&b)?.0;
                h.absorb_newer(older, crate::now_ms());
                h
            }
        };
        self.save_state(identity, &name, &h.encode_stored(c)?)?;
        self.remove_state(&old)?;
        Ok(true)
    }

    /// Moves every history file still under the name it had before 0.18.0
    /// (which named the account or group) to its keyed name. Idempotent;
    /// a file that can't be moved stays where it is, and loading that
    /// conversation tries again. Returns how many moved.
    pub fn migrate_history_names(&self, identity: &Identity) -> usize {
        let Ok(names) = self.state_names() else {
            return 0;
        };
        // Each move takes the history lock itself: with nothing to move,
        // this doesn't wait for it.
        names
            .iter()
            .filter_map(|n| ConversationId::parse_legacy_state_name(n))
            .filter(|c| self.migrate_conversation(identity, *c).unwrap_or(false))
            .count()
    }

    /// Every conversation stored under its keyed name, with its history.
    /// Files that can't be read, or don't say which conversation they are
    /// (or say one their name doesn't match), are skipped.
    fn stored_histories<'a>(
        &'a self,
        identity: &'a Identity,
    ) -> impl Iterator<Item = (ConversationId, History)> + 'a {
        self.state_names()
            .unwrap_or_default()
            .into_iter()
            .filter(|n| is_keyed_name(n))
            .filter_map(move |name| {
                let b = self.load_state(identity, &name).ok()??;
                let (h, c) = History::decode_stored(&b).ok()?;
                let c = c.filter(|c| c.state_name(identity) == name)?;
                Some((c, h))
            })
    }

    /// The history files a backup holds: each conversation under the name
    /// it had before 0.18.0, so that older versions restore it too (the
    /// backup is sealed as a whole, so its names show nothing). A
    /// conversation found under both names goes in once, merged as the
    /// move to the keyed name would (the old name's copy is the newer).
    /// Files that can't be read go in as they are. `dropped` (whose
    /// contact an older version blocked or deleted) are left out.
    pub(crate) fn history_for_backup(
        &self,
        identity: &Identity,
        dropped: &[ConversationId],
    ) -> Result<Vec<(String, Vec<u8>)>> {
        let _lock = history_lock();
        // Each conversation's keyed copy, and its old-name one.
        type Copies = (Option<History>, Option<History>);
        let mut convs: Vec<(ConversationId, Copies)> = Vec::new();
        let mut out = Vec::new();
        for name in self.state_names()? {
            let read = || self.load_state(identity, &name).ok().flatten();
            let legacy = ConversationId::parse_legacy_state_name(&name);
            let found = if let Some(c) = legacy {
                read()
                    .and_then(|b| History::decode(&b).ok())
                    .map(|h| (c, h))
            } else if is_keyed_name(&name) {
                read()
                    .and_then(|b| History::decode_stored(&b).ok())
                    .and_then(|(h, c)| Some((c?, h)))
                    .filter(|(c, _)| c.state_name(identity) == name)
            } else {
                continue;
            };
            match found {
                Some((c, _)) if dropped.contains(&c) => {}
                Some((c, h)) => {
                    let i = match convs.iter().position(|(x, _)| *x == c) {
                        Some(i) => i,
                        None => {
                            convs.push((c, (None, None)));
                            convs.len() - 1
                        }
                    };
                    let copies = &mut convs[i].1;
                    if legacy.is_some() {
                        copies.1 = Some(h);
                    } else {
                        copies.0 = Some(h);
                    }
                }
                None => match std::fs::read(self.state_path(&name)?) {
                    Ok(b) => out.push((name, b)),
                    // Removed since we listed it.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }
        for (c, copies) in convs {
            let h = match copies {
                (Some(mut keyed), Some(older)) => {
                    keyed.absorb_newer(older, crate::now_ms());
                    keyed
                }
                (Some(h), None) | (None, Some(h)) => h,
                (None, None) => continue,
            };
            let name = c.legacy_state_name();
            let sealed = self.seal_state(identity, &name, &h.encode()?)?;
            out.push((name, sealed));
        }
        Ok(out)
    }

    /// Appends one entry to a conversation.
    pub fn append_history(
        &self,
        identity: &Identity,
        c: ConversationId,
        e: Entry,
        now_ms: u64,
    ) -> Result<History> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        // Start counting unread messages: what came before is read.
        if h.seen_ms == 0 {
            h.seen_ms = h.entries.last().map_or(1, |l| l.at_ms.max(1));
        }
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
        let _lock = history_lock();
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

    /// Marks the outgoing 1:1 entries whose local ids are in `ids`
    /// delivered, with one load and at most one save (a receipt names up
    /// to 1,024). Returns the ids that changed; for a group, none.
    pub fn mark_delivered_ids(
        &self,
        identity: &Identity,
        c: ConversationId,
        ids: &[u64],
        now_ms: u64,
    ) -> Result<Vec<u64>> {
        if matches!(c, ConversationId::Group(_)) || ids.is_empty() {
            return Ok(Vec::new());
        }
        let _lock = history_lock();
        let wanted: std::collections::HashSet<u64> = ids.iter().copied().collect();
        let mut h = self.load_history(identity, c, now_ms)?;
        let mut changed = Vec::new();
        for e in h.entries.iter_mut().filter(|e| {
            e.outgoing && e.local_id != 0 && !e.delivered && wanted.contains(&e.local_id)
        }) {
            e.delivered = true;
            changed.push(e.local_id);
        }
        if !changed.is_empty() {
            self.save_history(identity, c, &h)?;
        }
        Ok(changed)
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
        let _lock = history_lock();
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
        let _lock = history_lock();
        if from == into {
            return Ok(false);
        }
        let Some(src) = self.read_history(identity, from)? else {
            return Ok(false);
        };
        let mut dst = self.load_history(identity, into, now_ms)?;
        dst.entries.extend(src.entries);
        dst.entries.sort_by_key(|e| e.at_ms);
        if dst.timer_s.is_none() {
            dst.timer_s = src.timer_s;
        }
        dst.prune(now_ms);
        self.save_history(identity, into, &dst)?;
        self.remove_state(&from.state_name(identity))?;
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
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        let before = h.entries.len();
        let gone: Vec<Entry> = h.entries.iter().filter(|e| pick(e)).cloned().collect();
        h.entries.retain(|e| !pick(e));
        h.forget_quotes(&gone);
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
        let _lock = history_lock();
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

    /// Sets `who`'s vote on poll `id` in `c` (no choices: takes it back);
    /// returns true if that changed anything.
    pub fn vote_entry(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        id: u64,
        who: [u8; 32],
        choices: &[u32],
    ) -> Result<bool> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        let changed = h
            .entries
            .iter_mut()
            .find(|e| id != 0 && e.message_id() == id)
            .and_then(|e| e.poll.as_mut())
            .is_some_and(|p| p.vote(who, choices));
        if changed {
            self.save_history(identity, c, &h)?;
        }
        Ok(changed)
    }

    /// Closes poll `id` in `c` if `author` accepts its sender's device;
    /// returns true if that changed anything.
    pub fn close_poll(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        id: u64,
        author: impl Fn(&[u8; 32]) -> bool,
    ) -> Result<bool> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        let changed = h
            .entries
            .iter_mut()
            .find(|e| id != 0 && e.message_id() == id && author(&e.device))
            .and_then(|e| e.poll.as_mut())
            .is_some_and(|p| !std::mem::replace(&mut p.closed, true));
        if changed {
            self.save_history(identity, c, &h)?;
        }
        Ok(changed)
    }

    /// Notes that the user looked at `c` now: its messages are read.
    pub fn mark_seen(&self, identity: &Identity, c: ConversationId, now_ms: u64) -> Result<()> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        if h.unread().next().is_some() {
            h.seen_ms = now_ms;
            self.save_history(identity, c, &h)?;
        }
        Ok(())
    }

    /// Keeps what the user started writing in `c` (empty: none).
    pub fn set_draft(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        draft: &str,
    ) -> Result<()> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        if h.draft != draft {
            h.draft = draft.to_owned();
            self.save_history(identity, c, &h)?;
        }
        Ok(())
    }

    /// Sets when `c`'s new messages notify.
    pub fn set_notify(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        n: Notify,
    ) -> Result<()> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        if h.notify != n {
            h.notify = n;
            self.save_history(identity, c, &h)?;
        }
        Ok(())
    }

    /// Pins (or unpins) message `id` in `c`; returns true if that changed
    /// anything.
    pub fn pin_entry(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        id: u64,
        pin: bool,
    ) -> Result<bool> {
        let _lock = history_lock();
        let mut h = self.load_history(identity, c, now_ms)?;
        let changed = h.pin(id, pin);
        if changed {
            self.save_history(identity, c, &h)?;
        }
        Ok(changed)
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
        let _lock = history_lock();
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
    /// many changed. Outgoing entries: the recipient displayed them.
    /// Incoming ones: we reported them read. Entries already marked keep
    /// their time.
    pub fn mark_read_entries(
        &self,
        identity: &Identity,
        c: ConversationId,
        now_ms: u64,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<usize> {
        let _lock = history_lock();
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
    pub fn delete_history(&self, identity: &Identity, c: ConversationId) -> Result<()> {
        let _lock = history_lock();
        self.remove_state(&c.legacy_state_name())?;
        self.remove_state(&c.state_name(identity))
    }

    /// Every conversation with stored history. Its files' names don't say
    /// (see [`ConversationId::state_name`]), so this reads each one.
    pub fn conversations(&self, identity: &Identity) -> Vec<ConversationId> {
        let _lock = history_lock();
        self.migrate_history_names(identity);
        self.stored_histories(identity).map(|(c, _)| c).collect()
    }

    /// Searches `scope` (one conversation, or every one when `None`) for
    /// `q`; returns at most `limit` hits, newest first. Expired messages
    /// never match, and nothing is written back.
    pub fn search_history(
        &self,
        identity: &Identity,
        scope: Option<ConversationId>,
        q: &Query,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<Hit>> {
        if q.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let mut hits = Vec::new();
        let mut search = |c: ConversationId, mut h: History| {
            h.prune(now_ms);
            hits.extend(h.search(q, limit).into_iter().map(|e| Hit {
                conversation: c,
                entry: e.clone(),
            }));
        };
        match scope {
            Some(c) => {
                if let Some(h) = self.read_history(identity, c)? {
                    search(c, h);
                }
            }
            // One unreadable conversation shouldn't hide the others.
            None => {
                let _lock = history_lock();
                self.migrate_history_names(identity);
                for (c, h) in self.stored_histories(identity) {
                    search(c, h);
                }
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.entry.at_ms));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Prunes expired messages in every conversation; returns how many
    /// conversations changed.
    pub fn sweep_history(&self, identity: &Identity, now_ms: u64) -> usize {
        let _lock = history_lock();
        self.migrate_history_names(identity);
        self.stored_histories(identity)
            .filter(|(c, h)| {
                let mut h = h.clone();
                h.prune(now_ms) && self.save_history(identity, *c, &h).is_ok()
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
            quote: None,
            mentions: Vec::new(),
            poll: None,
            forwarded: false,
            list: None,
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
        let raw = std::fs::read(dir.path().join(format!("{}.state", c.state_name(&id)))).unwrap();
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
        let raw = home.load_state(&id, &c.state_name(&id)).unwrap().unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"));

        assert_eq!(home.conversations(&id), vec![c]);
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
        assert_eq!(home.conversations(&id), vec![acct]);
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
                    clip: None,
                    forwarded: false,
                }),
                ..entry(MAX_ENTRIES as u64 + 6, "", None)
            },
            0,
        );
        h.push(
            Entry {
                file: Some(FileNote {
                    name: "voice.m4a".into(),
                    size: 0,
                    location: None,
                    sensitive: false,
                    album: 0,
                    clip: Some(Clip {
                        video: false,
                        duration_ms: 3_500,
                    }),
                    forwarded: false,
                }),
                ..entry(MAX_ENTRIES as u64 + 7, "", None)
            },
            0,
        );
        let again = History::decode(&h.encode().unwrap()).unwrap();
        assert_eq!(again, h);
    }

    #[test]
    fn query_terms_and_phrases() {
        assert_eq!(Query::new("  Hello   WORLD ").terms(), ["hello", "world"]);
        assert_eq!(
            Query::new(r#"meet "at the cafe" meet"#).terms(),
            ["meet", "at the cafe"]
        );
        assert_eq!(
            Query::new(r#""unclosed phrase"#).terms(),
            ["unclosed phrase"]
        );
        assert!(Query::new("   ").is_empty() && Query::new(r#""""#).is_empty());

        let e = entry(1, "Meet me at the Café later", None);
        assert!(Query::new("café MEET").matches(&e));
        assert!(Query::new(r#""at the café""#).matches(&e));
        assert!(!Query::new(r#""the at café""#).matches(&e));
        assert!(!Query::new("meet tomorrow").matches(&e), "every term");
        assert!(!Query::new("").matches(&e), "empty matches nothing");
        assert!(Query::new("ÄRGER").matches(&entry(1, "kein ärger", None)));

        let photo = Entry {
            file: Some(FileNote {
                name: "Holiday.JPG".into(),
                size: 1,
                location: None,
                sensitive: false,
                album: 0,
                clip: None,
                forwarded: false,
            }),
            ..entry(2, "beach day", None)
        };
        assert!(
            Query::new("holiday beach").matches(&photo),
            "name and caption"
        );
    }

    #[test]
    fn search_spans_conversations_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let (a, b, g) = (
            ConversationId::Peer([1; 32]),
            ConversationId::Peer([2; 32]),
            ConversationId::Group([3; 16]),
        );
        home.append_history(&id, a, entry(10, "lunch at noon?", None), 10)
            .unwrap();
        home.append_history(&id, a, entry(11, "unrelated", None), 11)
            .unwrap();
        home.append_history(&id, b, entry(30, "Lunch was great", None), 30)
            .unwrap();
        home.append_history(&id, g, entry(20, "group lunch", Some(1000)), 20)
            .unwrap();
        let q = Query::new("lunch");
        let all = home.search_history(&id, None, &q, 40, 10).unwrap();
        assert_eq!(
            all.iter()
                .map(|h| (h.conversation, h.entry.at_ms))
                .collect::<Vec<_>>(),
            [(b, 30), (g, 20), (a, 10)]
        );
        assert_eq!(home.search_history(&id, None, &q, 40, 2).unwrap().len(), 2);
        let one = home.search_history(&id, Some(a), &q, 40, 10).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].entry.text, "lunch at noon?");
        // Expired messages don't match.
        assert_eq!(home.search_history(&id, Some(g), &q, 2000, 10).unwrap(), []);
        assert_eq!(
            home.search_history(&id, None, &Query::new(""), 40, 10)
                .unwrap(),
            []
        );
        assert_eq!(
            home.search_history(&id, Some(ConversationId::Peer([9; 32])), &q, 40, 10)
                .unwrap(),
            []
        );
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

    #[test]
    fn quotes_persist_and_go_with_their_message() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let c = ConversationId::Peer([1; 32]);
        let mut original = entry(1, "the plan", Some(100));
        original.remote_id = 5;
        let mut reply = entry(3, "yes", None);
        reply.remote_id = 6;
        reply.quote = Some(Quote::new(5, [7; 32], "the plan"));
        let mut other = entry(5, "and this", None);
        other.remote_id = 8;
        other.quote = Some(Quote::new(6, [7; 32], "yes"));
        for e in [original, reply, other] {
            home.append_history(&id, c, e, 1).unwrap();
        }
        let quotes = |now| {
            home.load_history(&id, c, now)
                .unwrap()
                .entries()
                .iter()
                .filter_map(|e| e.quote.clone().map(|q| q.text))
                .collect::<Vec<_>>()
        };
        assert_eq!(quotes(2), ["the plan", "yes"]);
        // The original disappears: so does what quotes it.
        assert_eq!(quotes(200), ["", "yes"]);
        // Deleting a message blanks its quotes too.
        home.delete_entries(&id, c, 200, |e| e.remote_id == 6)
            .unwrap();
        assert_eq!(quotes(200), [""]);

        // A message from before ids is quoted by sender and text.
        let old = entry(7, "an old message", None);
        let mut reply = entry(9, "re", None);
        reply.quote = Some(Quote::of(&old));
        assert_eq!(reply.quote.as_ref().unwrap().id, 0);
        for e in [old, reply] {
            home.append_history(&id, c, e, 1).unwrap();
        }
        assert_eq!(quotes(200), ["", "an old message"]);
        home.delete_entries(&id, c, 200, |e| e.text == "an old message")
            .unwrap();
        assert_eq!(quotes(200), ["", ""]);
    }

    #[test]
    fn pins_hold_three_persist_and_go_with_their_message() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let c = ConversationId::Peer([1; 32]);
        for i in 1..=5u64 {
            let mut e = entry(i * 2 + 1, "m", None);
            e.remote_id = i;
            home.append_history(&id, c, e, 1).unwrap();
        }
        let pinned = || home.load_history(&id, c, 1).unwrap().pinned().to_vec();
        assert!(
            !home.pin_entry(&id, c, 1, 99, true).unwrap(),
            "no such message"
        );
        for i in 1..=4 {
            assert!(home.pin_entry(&id, c, 1, i, true).unwrap());
        }
        assert_eq!(pinned(), [2, 3, 4], "the oldest pin goes");
        assert!(
            !home.pin_entry(&id, c, 1, 3, true).unwrap(),
            "already pinned"
        );
        assert!(home.pin_entry(&id, c, 1, 3, false).unwrap());
        assert_eq!(pinned(), [2, 4]);
        home.delete_entries(&id, c, 1, |e| e.remote_id == 4)
            .unwrap();
        assert_eq!(pinned(), [2], "a deleted message's pin goes");
    }

    #[test]
    fn polls_count_votes_and_persist() {
        assert!(Poll::new(vec!["only".into(), "  ".into()], false).is_none());
        let mut p = Poll::new(vec!["a".into(), " b ".into(), "c".into()], false).unwrap();
        assert_eq!(p.options, ["a", "b", "c"]);
        assert!(p.vote([1; 32], &[2, 0]), "single choice keeps one");
        assert!(!p.vote([1; 32], &[0]), "same vote again");
        assert!(p.vote([2; 32], &[0, 9]), "out of range ignored");
        assert_eq!(p.counts(), [2, 0, 0]);
        assert!(p.vote([1; 32], &[]), "taken back");
        assert_eq!(p.counts(), [1, 0, 0]);
        p.multi = true;
        assert!(p.vote([3; 32], &[1, 2, 1]));
        assert_eq!(p.counts(), [1, 1, 1]);
        p.closed = true;
        assert!(!p.vote([4; 32], &[0]), "closed");

        let mut e = entry(1, "lunch?", None);
        e.poll = Some(p);
        let h = History::transcript(vec![e]);
        assert_eq!(History::decode(&h.encode().unwrap()).unwrap(), h);
    }

    #[test]
    fn mark_delivered_ids_marks_once_in_one_save() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let c = ConversationId::Peer([4; 32]);
        for (at, local_id) in [(2, 10), (4, 11), (6, 12)] {
            let e = Entry {
                local_id,
                ..entry(at, "ours", None)
            };
            home.append_history(&id, c, e, at).unwrap();
        }
        // Theirs, with a remote id equal to one of ours: never touched.
        let theirs = Entry {
            remote_id: 11,
            local_id: 11,
            ..entry(7, "theirs", None)
        };
        home.append_history(&id, c, theirs, 7).unwrap();
        let file = dir.path().join(format!("{}.state", c.state_name(&id)));
        let mut changed = home.mark_delivered_ids(&id, c, &[12, 10, 99], 8).unwrap();
        changed.sort_unstable();
        assert_eq!(changed, [10, 12]);
        let written = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(
            home.mark_delivered_ids(&id, c, &[10, 12], 9)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            written,
            "nothing changed, nothing saved"
        );
        let h = home.load_history(&id, c, 9).unwrap();
        let delivered: Vec<(bool, bool)> = h
            .entries()
            .iter()
            .map(|e| (e.outgoing, e.delivered))
            .collect();
        assert_eq!(
            delivered,
            [(true, true), (true, false), (true, true), (false, false)]
        );
        // Groups count members' acknowledgements instead.
        let g = ConversationId::Group([5; 16]);
        let e = Entry {
            local_id: 13,
            recipients: 2,
            ..entry(8, "all", None)
        };
        home.append_history(&id, g, e, 8).unwrap();
        assert!(
            home.mark_delivered_ids(&id, g, &[13], 9)
                .unwrap()
                .is_empty()
        );
        assert!(!home.load_history(&id, g, 9).unwrap().entries()[0].delivered);
    }

    #[test]
    fn read_ms_marks_reported_incoming_entries() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = Identity::generate();
        let c = ConversationId::Peer([3; 32]);
        let incoming = Entry {
            remote_id: 41,
            ..entry(1, "from them", None)
        };
        let outgoing = Entry {
            local_id: 42,
            ..entry(2, "from us", None)
        };
        assert!(!incoming.outgoing && outgoing.outgoing);
        home.append_history(&id, c, incoming, 1).unwrap();
        home.append_history(&id, c, outgoing, 2).unwrap();
        let reported = |e: &Entry| !e.outgoing && e.remote_id == 41;
        assert_eq!(home.mark_read_entries(&id, c, 10, reported).unwrap(), 1);
        // Once only: a second report changes nothing and keeps the time.
        assert_eq!(home.mark_read_entries(&id, c, 20, reported).unwrap(), 0);
        let h = home.load_history(&id, c, 30).unwrap();
        let read: Vec<u64> = h.entries().iter().map(|e| e.read_ms).collect();
        assert_eq!(read, [10, 0], "survives a save and load; ours untouched");
    }
}

#[cfg(test)]
mod names_at_rest_tests {
    use super::*;
    use crate::persona::Personas;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn note(at: u64, text: &str) -> Entry {
        Entry {
            at_ms: at,
            outgoing: false,
            device: [7; 32],
            text: text.into(),
            offline: false,
            expires_at_ms: None,
            file: None,
            local_id: 0,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
            remote_id: at,
            edited_ms: 0,
            read_ms: 0,
            reactions: Vec::new(),
            quote: None,
            mentions: Vec::new(),
            poll: None,
            forwarded: false,
            list: None,
        }
    }

    fn texts(h: &History) -> Vec<&str> {
        h.entries().iter().map(|e| e.text.as_str()).collect()
    }

    fn file_names(home: &Home) -> Vec<String> {
        std::fs::read_dir(home.dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    /// Writes history the way 0.17.3 named it: `hist-p-<account id>` and
    /// `hist-g-<group id>`.
    fn write_old(home: &Home, id: &Identity, c: ConversationId, entries: &[Entry]) {
        let name = match c {
            ConversationId::Peer(k) => format!("hist-p-{}", hex(&k)),
            ConversationId::Group(g) => format!("hist-g-{}", hex(&g)),
        };
        let mut h = History::default();
        for e in entries {
            h.push(e.clone(), 0);
        }
        home.save_state(id, &name, &h.encode().unwrap()).unwrap();
    }

    fn write_old_h(home: &Home, id: &Identity, c: ConversationId, h: &History) {
        home.save_state(id, &c.legacy_state_name(), &h.encode().unwrap())
            .unwrap();
    }

    fn assert_no_ids(home: &Home, ids: &[&[u8]]) {
        for name in file_names(home) {
            assert!(
                !name.starts_with("hist-p-") && !name.starts_with("hist-g-"),
                "old name left: {name}"
            );
            for id in ids {
                assert!(!name.contains(&hex(id)), "{name} names a conversation");
            }
            let bytes = std::fs::read(home.dir().join(&name)).unwrap();
            for id in ids {
                assert!(
                    !bytes.windows(id.len()).any(|w| w == *id),
                    "{name} holds a conversation id in clear"
                );
            }
        }
    }

    #[test]
    fn history_file_names_reveal_no_account_or_group() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = home.create_identity(None).unwrap();
        let (acct, group) = ([0xa7; 32], [0x5c; 16]);
        let (p, g) = (ConversationId::Peer(acct), ConversationId::Group(group));
        home.append_history(&id, p, note(1, "hello"), 1).unwrap();
        home.append_history(&id, g, note(2, "team"), 2).unwrap();
        assert_no_ids(&home, &[&acct, &group]);
        // One file each, both keyed, and they still list and load.
        let hist: Vec<_> = file_names(&home)
            .into_iter()
            .filter(|n| n.starts_with("hist-"))
            .collect();
        assert_eq!(hist.len(), 2, "{hist:?}");
        let mut convs = home.conversations(&id);
        convs.sort_by_key(ConversationId::to_bytes);
        assert_eq!(convs, vec![p, g]);
        assert_eq!(texts(&home.load_history(&id, p, 3).unwrap()), ["hello"]);

        // The names depend on the identity: another home's differ.
        let dir2 = tempfile::tempdir().unwrap();
        let other = Home::new(dir2.path());
        let id2 = other.create_identity(None).unwrap();
        other.append_history(&id2, p, note(1, "hello"), 1).unwrap();
        let theirs = file_names(&other);
        assert!(!theirs.iter().any(|n| hist.contains(n)), "unkeyed name");

        home.delete_history(&id, p).unwrap();
        assert_eq!(home.conversations(&id), vec![g]);
    }

    #[test]
    fn old_history_names_move_on_first_load_and_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = home.create_identity(None).unwrap();
        let (acct, group) = ([0x11; 32], [0x22; 16]);
        let (p, g) = (ConversationId::Peer(acct), ConversationId::Group(group));
        write_old(&home, &id, p, &[note(1, "one"), note(2, "two")]);
        write_old(&home, &id, g, &[note(3, "g1")]);

        home.load_contacts(&id).unwrap();
        assert_no_ids(&home, &[&acct, &group]);
        assert_eq!(
            texts(&home.load_history(&id, p, 9).unwrap()),
            ["one", "two"]
        );
        assert_eq!(texts(&home.load_history(&id, g, 9).unwrap()), ["g1"]);
        let mut convs = home.conversations(&id);
        convs.sort_by_key(ConversationId::to_bytes);
        assert_eq!(convs, vec![p, g]);

        // Idempotent.
        let before = file_names(&home);
        home.load_contacts(&id).unwrap();
        assert_eq!(file_names(&home).len(), before.len());

        // An older version ran again and wrote the old name anew (with
        // only what it saw): both are merged, nothing is lost.
        write_old(&home, &id, p, &[note(2, "two"), note(4, "four")]);
        home.load_contacts(&id).unwrap();
        assert_no_ids(&home, &[&acct, &group]);
        assert_eq!(
            texts(&home.load_history(&id, p, 9).unwrap()),
            ["one", "two", "four"]
        );

        // Even without a migration first, a conversation loads, searches
        // and lists from its old name.
        write_old(&home, &id, p, &[note(5, "five")]);
        let q = Query::new("five");
        assert_eq!(home.search_history(&id, None, &q, 9, 9).unwrap().len(), 1);
        write_old(&home, &id, p, &[note(6, "six")]);
        assert_eq!(home.conversations(&id).len(), 2);
        write_old(&home, &id, p, &[note(7, "seven")]);
        assert_eq!(
            texts(&home.load_history(&id, p, 9).unwrap()),
            ["one", "two", "four", "five", "six", "seven"]
        );
        assert_no_ids(&home, &[&acct, &group]);
    }

    /// What 0.17.3 set in a conversation after the upgrade is the newer:
    /// its file under the old name wins over the keyed one's settings.
    #[test]
    fn settings_an_older_version_changed_survive_the_return() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        for (dir, by_backup) in [(a, false), (b, true)] {
            let home = Home::new(dir.path());
            let id = home.create_identity(None).unwrap();
            let (p, q) = (
                ConversationId::Peer([0x81; 32]),
                ConversationId::Peer([0x82; 32]),
            );
            // Before the downgrade: timer off, notify all, a draft.
            for c in [p, q] {
                let mut h = History::default();
                h.push(note(1, "before"), 0);
                h.timer_s = Some(0);
                h.draft = "kept".into();
                h.notify = Notify::Off;
                home.save_history(&id, c, &h).unwrap();
            }
            // Under 0.17.3: p's timer set to an hour, notify to mentions,
            // a new draft; q only gets a message.
            let mut h = History::default();
            h.push(note(2, "under 0.17.3"), 0);
            h.timer_s = Some(3600);
            h.notify = Notify::Mentions;
            h.draft = "newer".into();
            write_old_h(&home, &id, p, &h);
            write_old(&home, &id, q, &[note(3, "only a message")]);

            let restored = tempfile::tempdir().unwrap();
            let home = if by_backup {
                let backup = home.export_backup_with(&id, PW, FAST).unwrap();
                let new = Home::new(restored.path());
                new.restore_backup(&backup, PW, None).unwrap();
                new
            } else {
                home.load_contacts(&id).unwrap();
                home
            };
            let h = home.load_history(&id, p, 9).unwrap();
            assert_eq!(texts(&h), ["before", "under 0.17.3"]);
            assert_eq!(h.timer_s, Some(3600), "backup: {by_backup}");
            assert_eq!(h.notify, Notify::Mentions);
            assert_eq!(h.draft, "newer");
            let h = home.load_history(&id, q, 9).unwrap();
            assert_eq!(texts(&h), ["before", "only a message"]);
            assert_eq!(h.timer_s, Some(0), "unset there: ours stands");
            assert_eq!(h.notify, Notify::Off);
            assert_eq!(h.draft, "kept");
        }
    }

    /// 0.17.3 blocking or deleting a contact deletes only the history
    /// under the old name; the keyed one goes when the contacts merge.
    #[test]
    fn history_of_a_contact_an_older_version_dropped_goes_on_return() {
        for blocked in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let home = Home::new(dir.path());
            let id = home.create_identity(None).unwrap();
            let (bob, carol) = (Identity::generate().public(), Identity::generate().public());
            let (pb, pc) = (
                ConversationId::Peer(*bob.as_bytes()),
                ConversationId::Peer(*carol.as_bytes()),
            );
            let mut state = crate::store::Contacts::default();
            for k in [bob, carol] {
                state.observe(k, None, 1);
                let c = state.get_mut(&k).unwrap();
                c.local_approved = true;
                c.accepted = true;
                c.approval_changed_ms = 10;
            }
            home.save_contacts(&id, &state).unwrap();
            home.append_history(&id, pb, note(1, "pre-1"), 1).unwrap();
            home.append_history(&id, pc, note(1, "carol"), 1).unwrap();

            let mut plain = crate::store::Contacts::default();
            plain.observe(bob, None, 50);
            if blocked {
                let b = plain.get_mut(&bob).unwrap();
                b.blocked = true;
                b.accepted = false;
            } else {
                plain.forget(&bob, crate::now_ms());
            }
            home.write_contacts_file(&plain.encode().unwrap()).unwrap();

            let contacts = home.load_contacts(&id).unwrap();
            assert_eq!(contacts.get(&bob).is_some_and(|c| c.blocked), blocked);
            assert!(
                home.load_history(&id, pb, 9).unwrap().entries().is_empty(),
                "history back (blocked: {blocked})"
            );
            assert_eq!(home.conversations(&id), vec![pc]);
            assert_eq!(texts(&home.load_history(&id, pc, 9).unwrap()), ["carol"]);
        }
    }

    #[test]
    fn a_persona_home_migrates_too() {
        let dir = tempfile::tempdir().unwrap();
        let main = Home::new(dir.path().join("main"));
        let me = main.create_identity(None).unwrap();
        let (_, home, id) = Personas::new(&main, &me)
            .create("market", None, None, 1)
            .unwrap();
        let acct = [0x33; 32];
        let p = ConversationId::Peer(acct);
        write_old(&home, &id, p, &[note(1, "from pat")]);

        home.load_contacts(&id).unwrap();
        assert_no_ids(&home, &[&acct]);
        assert_eq!(texts(&home.load_history(&id, p, 9).unwrap()), ["from pat"]);
        // Its names are its own, not the main identity's.
        main.append_history(&me, p, note(1, "x"), 1).unwrap();
        let mine: Vec<_> = file_names(&main)
            .into_iter()
            .filter(|n| n.starts_with("hist-"))
            .collect();
        assert!(!file_names(&home).iter().any(|n| mine.contains(n)));
    }

    const FAST: crate::sealed::KdfParams = crate::sealed::KdfParams {
        m_kib: 64,
        t: 1,
        lanes: 1,
    };
    const PW: &str = "correct horse battery";

    #[test]
    fn an_older_backup_restores_into_keyed_names() {
        // A home as 0.17.3 left it (and backed it up).
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        let (acct, group) = ([0x44; 32], [0x55; 16]);
        let (p, g) = (ConversationId::Peer(acct), ConversationId::Group(group));
        write_old(&old, &id, p, &[note(1, "kept")]);
        write_old(&old, &id, g, &[note(2, "kept too")]);
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();

        let new = Home::new(b.path());
        let r = new.restore_backup(&backup, PW, None).unwrap();
        assert_no_ids(&new, &[&acct, &group]);
        assert_eq!(
            texts(&new.load_history(&r.identity, p, 9).unwrap()),
            ["kept"]
        );
        assert_eq!(
            texts(&new.load_history(&r.identity, g, 9).unwrap()),
            ["kept too"]
        );
    }

    #[test]
    fn a_backup_keeps_the_names_older_versions_read() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::new(a.path());
        let id = home.create_identity(None).unwrap();
        let (acct, group) = ([0x66; 32], [0x77; 16]);
        let (p, g) = (ConversationId::Peer(acct), ConversationId::Group(group));
        home.append_history(&id, p, note(1, "hi"), 1).unwrap();
        home.append_history(&id, g, note(2, "all"), 2).unwrap();
        let backup = home.export_backup_with(&id, PW, FAST).unwrap();

        // Inside the sealed backup, history goes under the old names, so
        // 0.17.3 restores it as it always did: each file as stored, read
        // under its name.
        let states = crate::backup::states_in(&backup, PW);
        let old = Home::new(b.path());
        for (name, bytes) in &states {
            old.write_state_file(name, bytes).unwrap();
        }
        let read = |name: String| {
            History::decode(&old.load_state(&id, &name).unwrap().expect(&name)).unwrap()
        };
        assert_eq!(texts(&read(format!("hist-p-{}", hex(&acct)))), ["hi"]);
        assert_eq!(texts(&read(format!("hist-g-{}", hex(&group)))), ["all"]);
        assert!(
            !states
                .iter()
                .any(|(n, _)| n.starts_with("hist-") && n.len() == 69),
            "keyed names in the backup"
        );

        // And this version restores it to keyed names.
        let c = tempfile::tempdir().unwrap();
        let new = Home::new(c.path());
        new.restore_backup(&backup, PW, None).unwrap();
        assert_no_ids(&new, &[&acct, &group]);
        assert_eq!(texts(&new.load_history(&id, p, 9).unwrap()), ["hi"]);
        let mut convs = new.conversations(&id);
        convs.sort_by_key(ConversationId::to_bytes);
        assert_eq!(convs, vec![p, g]);
    }
}
