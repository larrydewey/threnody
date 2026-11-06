//! Group control messages, carried in `AppMessage::Group` over the 1:1
//! ratchets (so MLS traffic is itself end-to-end encrypted per hop).
//!
//! ```text
//! GroupWire = { 0: kind uint, 1: group_id bstr, ? 2: payload bstr, ? 3: name tstr,
//!               ? 4: member bstr .size 32, ? 5: reference uint, ? 6: owner bstr .size 32,
//!               ? 7: [* bstr], ? 8: [epoch uint, term uint, auth bstr .size 8, lead bstr .size 8] }
//! kind: 1 key-package request, 2 key package, 3 welcome, 4 MLS message,
//!       5 forward (an MLS message for another member),
//!       6 receipt (that member acknowledged a forwarded message),
//!       7 leave (a member asks the coordinator to remove it;
//!         2: its own Remove proposal, in groups with a roster),
//!       8 add device (6: the group's owner, to the device itself),
//!       9 pass welcome (coordinator -> proposer: 2 the Welcome for 4, 3 the name),
//!       10 resync (8: the stale device's own hint; 4: that device, when relayed)
//! 7: the proposals a coordinator's commit references (kinds 4 and 5)
//! 8: the sender's sync hint (kinds 4 and 5 in groups with a roster; 10)
//! ```

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::history::{MAX_POLL_OPTIONS, MAX_POLL_QUESTION};
use threnody_core::message::{Clip, FORWARDED, MAX_MENTIONS, Mention, Quote};
use threnody_core::{Error, Result};

/// Proposals a commit may carry (the coordinator stores at most 32 an
/// epoch).
pub const MAX_ATTACHED: usize = 64;

/// Group ids are 16 random bytes.
pub type GroupId = [u8; 16];

/// Where the sender stands in a group with a roster: its epoch, the
/// roster's term, the first 8 bytes of its epoch authenticator (RFC 9420
/// §8.7) and of its coordinator. Unauthenticated: it only ever prompts a
/// request for repair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncHint {
    pub epoch: u64,
    pub term: u32,
    pub auth: [u8; 8],
    pub lead: [u8; 8],
}

impl SyncHint {
    /// See [`crate::roster::Rank`].
    pub fn rank(&self) -> crate::roster::Rank {
        (self.term, std::cmp::Reverse(self.lead))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupWire {
    /// "Send me a key package so I can add you to this group."
    KeyPackageRequest { group: GroupId, name: String },
    KeyPackage {
        group: GroupId,
        key_package: Vec<u8>,
    },
    Welcome {
        group: GroupId,
        name: String,
        welcome: Vec<u8>,
    },
    /// A serialized `MlsMessageOut` (application message, proposal or
    /// commit). A coordinator's commit carries the proposals it references,
    /// and in groups with a roster every message carries a sync hint.
    Message {
        group: GroupId,
        message: Vec<u8>,
        proposals: Vec<Vec<u8>>,
        sync: Option<SyncHint>,
    },
    /// "Deliver this MLS message to member `to` for me": sent to a member
    /// the sender can reach when it can't reach `to` itself.
    /// `reference` (if not 0) asks the forwarder to send a `Receipt`
    /// once `to` acknowledges.
    Forward {
        group: GroupId,
        to: [u8; 32],
        message: Vec<u8>,
        reference: u64,
        proposals: Vec<Vec<u8>>,
        sync: Option<SyncHint>,
    },
    /// From a forwarder: `member` acknowledged the message we forwarded
    /// with `reference`.
    Receipt {
        group: GroupId,
        member: [u8; 32],
        reference: u64,
    },
    /// To the coordinator: remove me (the sender) from the group. In a
    /// group with a roster, with our own Remove proposal.
    Leave {
        group: GroupId,
        proposal: Option<Vec<u8>>,
    },
    /// Between devices of one account: add `device` (another device of the
    /// sender's account) to this group. To the owner, a request; to
    /// `device` itself, word to accept the invitation that follows from
    /// `owner` (absent from older senders).
    AddDevice {
        group: GroupId,
        device: [u8; 32],
        owner: Option<[u8; 32]>,
    },
    /// From the coordinator to whoever proposed adding `member`: the
    /// Welcome to pass on to it.
    PassWelcome {
        group: GroupId,
        member: [u8; 32],
        name: String,
        welcome: Vec<u8>,
    },
    /// "I fell out of step with the group": to a member on the branch that
    /// won, with our own hint; from that member to its coordinator, naming
    /// us as `member`.
    Resync {
        group: GroupId,
        member: Option<[u8; 32]>,
        sync: SyncHint,
    },
}

impl GroupWire {
    /// A plain MLS message, without attachments or hint.
    pub fn message(group: GroupId, message: Vec<u8>) -> Self {
        Self::Message {
            group,
            message,
            proposals: Vec::new(),
            sync: None,
        }
    }

    pub fn group(&self) -> &GroupId {
        match self {
            Self::KeyPackageRequest { group, .. }
            | Self::KeyPackage { group, .. }
            | Self::Welcome { group, .. }
            | Self::Message { group, .. }
            | Self::Forward { group, .. }
            | Self::Receipt { group, .. }
            | Self::Leave { group, .. }
            | Self::AddDevice { group, .. }
            | Self::PassWelcome { group, .. }
            | Self::Resync { group, .. } => group,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        #[allow(clippy::type_complexity)]
        let (kind, payload, name, to, reference): (
            u8,
            Option<&[u8]>,
            Option<&str>,
            Option<&[u8; 32]>,
            u64,
        ) = match self {
            Self::KeyPackageRequest { name, .. } => (1, None, Some(name), None, 0),
            Self::KeyPackage { key_package, .. } => (2, Some(key_package), None, None, 0),
            Self::Welcome { welcome, name, .. } => (3, Some(welcome), Some(name), None, 0),
            Self::Message { message, .. } => (4, Some(message), None, None, 0),
            Self::Forward {
                to,
                message,
                reference,
                ..
            } => (5, Some(message), None, Some(to), *reference),
            Self::Receipt {
                member, reference, ..
            } => (6, None, None, Some(member), *reference),
            Self::Leave { proposal, .. } => (7, proposal.as_deref(), None, None, 0),
            Self::AddDevice { device, .. } => (8, None, None, Some(device), 0),
            Self::PassWelcome {
                member,
                name,
                welcome,
                ..
            } => (9, Some(welcome), Some(name), Some(member), 0),
            Self::Resync { member, .. } => (10, None, None, member.as_ref(), 0),
        };
        let owner = match self {
            Self::AddDevice { owner, .. } => owner.as_ref(),
            _ => None,
        };
        let (proposals, sync): (&[Vec<u8>], Option<&SyncHint>) = match self {
            Self::Message {
                proposals, sync, ..
            }
            | Self::Forward {
                proposals, sync, ..
            } => (proposals, sync.as_ref()),
            Self::Resync { sync, .. } => (&[], Some(sync)),
            _ => (&[], None),
        };
        let size = payload.map_or(0, <[u8]>::len)
            + name.map_or(0, str::len)
            + proposals.iter().map(|p| p.len() + 9).sum::<usize>()
            + 128;
        cbor::to_vec(size, |e| {
            e.map_len(
                2 + usize::from(payload.is_some())
                    + usize::from(name.is_some())
                    + usize::from(to.is_some())
                    + usize::from(reference != 0)
                    + usize::from(owner.is_some())
                    + usize::from(!proposals.is_empty())
                    + usize::from(sync.is_some()),
            )?;
            e.u8(0)?.u8(kind)?;
            e.u8(1)?.bytes(self.group())?;
            if let Some(p) = payload {
                e.u8(2)?.bytes(p)?;
            }
            if let Some(n) = name {
                e.u8(3)?.str(n)?;
            }
            if let Some(t) = to {
                e.u8(4)?.bytes(t)?;
            }
            if reference != 0 {
                e.u8(5)?.u64(reference)?;
            }
            if let Some(o) = owner {
                e.u8(6)?.bytes(o)?;
            }
            if !proposals.is_empty() {
                e.u8(7)?.array_len(proposals.len())?;
                for p in proposals {
                    e.bytes(p)?;
                }
            }
            if let Some(h) = sync {
                e.u8(8)?
                    .array_len(4)?
                    .u64(h.epoch)?
                    .u32(h.term)?
                    .bytes(&h.auth)?
                    .bytes(&h.lead)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut kind, mut group, mut payload, mut name, mut to) = (None, None, None, None, None);
        let mut reference = 0;
        let mut owner = None;
        let mut proposals = Vec::new();
        let mut sync = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => group = Some(cbor::fixed_bytes::<16>(d)?),
                2 => payload = Some(d.bytes()?.to_vec()),
                3 => name = Some(d.str()?.to_owned()),
                4 => to = Some(cbor::fixed_bytes::<32>(d)?),
                5 => reference = d.u64()?,
                6 => owner = Some(cbor::fixed_bytes::<32>(d)?),
                7 => {
                    let n = d.array_len()?;
                    if n > MAX_ATTACHED {
                        return Err(Error::Malformed("too many attached proposals"));
                    }
                    for _ in 0..n {
                        proposals.push(d.bytes()?.to_vec());
                    }
                }
                8 => {
                    if d.array_len()? != 4 {
                        return Err(Error::Malformed("sync hint"));
                    }
                    sync = Some(SyncHint {
                        epoch: d.u64()?,
                        term: d.u32()?,
                        auth: cbor::fixed_bytes::<8>(d)?,
                        lead: cbor::fixed_bytes::<8>(d)?,
                    });
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let group = required(group, "group id")?;
        Ok(match required(kind, "group message kind")? {
            1 => Self::KeyPackageRequest {
                group,
                name: name.unwrap_or_default(),
            },
            2 => Self::KeyPackage {
                group,
                key_package: required(payload, "key package")?,
            },
            3 => Self::Welcome {
                group,
                name: name.unwrap_or_default(),
                welcome: required(payload, "welcome")?,
            },
            4 => Self::Message {
                group,
                message: required(payload, "mls message")?,
                proposals,
                sync,
            },
            5 => Self::Forward {
                group,
                to: required(to, "forward target")?,
                message: required(payload, "mls message")?,
                reference,
                proposals,
                sync,
            },
            6 => Self::Receipt {
                group,
                member: required(to, "receipt member")?,
                reference,
            },
            7 => Self::Leave {
                group,
                proposal: payload,
            },
            8 => Self::AddDevice {
                group,
                device: required(to, "device")?,
                owner,
            },
            9 => Self::PassWelcome {
                group,
                member: required(to, "joining member")?,
                name: name.unwrap_or_default(),
                welcome: required(payload, "welcome")?,
            },
            10 => Self::Resync {
                group,
                member: to,
                sync: required(sync, "sync hint")?,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// What a member sends inside an MLS application message.
///
/// ```text
/// Content = text (UTF-8, as first sent; no id)
///         / 0xFF || { 0: kind uint, ? 1: text, file name or emoji tstr,
///                     ? 2: data bstr, ? 3: flags uint, ? 4: caption tstr,
///                     ? 5: album uint, ? 6: message id uint,
///                     ? 7: clip duration_ms uint, ? 8: quoted id uint,
///                     ? 9: quoted author bstr, ? 10: quoted text tstr,
///                     ? 11: mentions [* [start uint, len uint, who bstr]] }
/// kind: 1 text, 2 file, 3 reaction, 4 pin, 5 poll (1: question,
/// 12: options, flag 1 multiple choice), 6 vote (13: choices), 7 close poll,
/// 8 list (2: the list), 9 call (2: `threnody_core::call::GroupCallMsg`)
/// flags: 1 sensitive, 2 voice message, 4 video message, 8 forwarded (file);
///        8 forwarded (text);
///        1 remove (reaction)
/// ```
///
/// 0xFF never starts UTF-8, so plain text stays as it was. Text and files
/// carry the sender's id for the message; a reaction names the message
/// it's for by that id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text {
        text: String,
        id: u64,
        /// The message this one replies to.
        quote: Option<Quote>,
        /// Who it mentions.
        mentions: Vec<Mention>,
        /// Forwarded from another chat.
        forwarded: bool,
    },
    File {
        name: String,
        data: Vec<u8>,
        sensitive: bool,
        caption: String,
        album: u64,
        id: u64,
        /// A voice or video message rather than a plain file.
        clip: Option<Clip>,
        /// Forwarded from another chat.
        forwarded: bool,
    },
    /// Add (or with `add` false, take away) the sender's `emoji` on
    /// message `id`.
    React { id: u64, emoji: String, add: bool },
    /// Pin (or with `pin` false, unpin) message `id` (owner and admins).
    Pin { id: u64, pin: bool },
    /// A poll, message `id`: a question, its options, and whether several
    /// may be chosen.
    Poll {
        id: u64,
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    /// The sender's choices in poll `id` (none: takes the vote back).
    Vote { id: u64, choices: Vec<u32> },
    /// Closes poll `id` (its author only).
    ClosePoll { id: u64 },
    /// A list shared with the group (`threnody_core::lists::List`), to merge.
    List(Vec<u8>),
    /// Group call signalling (`threnody_core::call::GroupCallMsg`).
    Call(Vec<u8>),
}

const TAGGED: u8 = 0xFF;

impl Content {
    pub fn encode(&self) -> Result<Vec<u8>> {
        // (kind, text, data, flags, caption, album, id, duration)
        let (kind, text, data, flags, caption, album, id, duration): (
            u8,
            &str,
            Option<&[u8]>,
            u64,
            &str,
            u64,
            u64,
            u32,
        ) = match self {
            // Without an id, exactly as the first versions sent it.
            Self::Text {
                text,
                id: 0,
                quote: None,
                mentions,
                forwarded: false,
            } if mentions.is_empty() => return Ok(text.as_bytes().to_vec()),
            Self::Text {
                text,
                id,
                forwarded,
                ..
            } => (
                1,
                text,
                None,
                u64::from(*forwarded) * FORWARDED,
                "",
                0,
                *id,
                0,
            ),
            Self::File {
                name,
                data,
                sensitive,
                caption,
                album,
                id,
                clip,
                forwarded,
            } => (
                2,
                name,
                Some(data),
                u64::from(*sensitive)
                    | clip.map_or(0, |c| c.flag())
                    | (u64::from(*forwarded) * FORWARDED),
                caption,
                *album,
                *id,
                clip.map_or(0, |c| c.duration_ms),
            ),
            Self::React { id, emoji, add } => (3, emoji, None, u64::from(!*add), "", 0, *id, 0),
            Self::Pin { id, pin } => (4, "", None, u64::from(!*pin), "", 0, *id, 0),
            Self::Poll {
                id,
                question,
                multi,
                ..
            } => (5, question, None, u64::from(*multi), "", 0, *id, 0),
            Self::Vote { id, .. } => (6, "", None, 0, "", 0, *id, 0),
            Self::ClosePoll { id } => (7, "", None, 0, "", 0, *id, 0),
            Self::List(l) => (8, "", Some(l.as_slice()), 0, "", 0, 0, 0),
            Self::Call(c) => (9, "", Some(c.as_slice()), 0, "", 0, 0, 0),
        };
        let (options, choices): (&[String], &[u32]) = match self {
            Self::Poll { options, .. } => (options, &[]),
            Self::Vote { choices, .. } => (&[], choices),
            _ => (&[], &[]),
        };
        let (quote, mentions) = match self {
            Self::Text {
                quote, mentions, ..
            } => (quote.as_ref(), mentions.as_slice()),
            _ => (None, &[][..]),
        };
        let size = text.len()
            + data.map_or(0, <[u8]>::len)
            + caption.len()
            + quote.map_or(0, |q| q.text.len() + 48)
            + mentions.len() * 48
            + options.iter().map(|o| o.len() + 4).sum::<usize>()
            + choices.len() * 5
            + 48;
        let body = cbor::to_vec(size, |e| {
            e.map_len(
                2 + usize::from(data.is_some())
                    + usize::from(flags != 0)
                    + usize::from(!caption.is_empty())
                    + usize::from(album != 0)
                    + usize::from(id != 0)
                    + usize::from(duration != 0)
                    + 3 * usize::from(quote.is_some())
                    + usize::from(!mentions.is_empty())
                    + usize::from(!options.is_empty())
                    + usize::from(!choices.is_empty()),
            )?;
            e.u8(0)?.u8(kind)?;
            e.u8(1)?.str(text)?;
            if let Some(d) = data {
                e.u8(2)?.bytes(d)?;
            }
            if flags != 0 {
                e.u8(3)?.u64(flags)?;
            }
            if !caption.is_empty() {
                e.u8(4)?.str(caption)?;
            }
            if album != 0 {
                e.u8(5)?.u64(album)?;
            }
            if id != 0 {
                e.u8(6)?.u64(id)?;
            }
            if duration != 0 {
                e.u8(7)?.u32(duration)?;
            }
            if let Some(q) = quote {
                e.u8(8)?.u64(q.id)?;
                e.u8(9)?.bytes(&q.author)?;
                e.u8(10)?.str(&q.text)?;
            }
            if !options.is_empty() {
                e.u8(12)?.array_len(options.len())?;
                for o in options {
                    e.str(o)?;
                }
            }
            if !choices.is_empty() {
                e.u8(13)?.array_len(choices.len())?;
                for c in choices {
                    e.u32(*c)?;
                }
            }
            if !mentions.is_empty() {
                e.u8(11)?.array_len(mentions.len())?;
                for m in mentions {
                    e.array_len(3)?.u32(m.start)?.u32(m.len)?.bytes(&m.who)?;
                }
            }
            Ok(())
        })?;
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(TAGGED);
        out.extend_from_slice(&body);
        Ok(out)
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let Some((&TAGGED, rest)) = b.split_first() else {
            return Ok(Self::Text {
                text: String::from_utf8_lossy(b).into_owned(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            });
        };
        let mut dec = Decoder::new(rest);
        let (mut kind, mut text, mut data) = (None, None, None);
        let (mut flags, mut caption, mut album, mut id) = (0, String::new(), 0, 0);
        let mut duration = 0;
        let (mut quote_id, mut quote_author, mut quote_text) = (0, None, None);
        let mut mentions = Vec::new();
        let (mut options, mut choices) = (Vec::new(), Vec::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => text = Some(d.str()?.to_owned()),
                2 => data = Some(d.bytes()?.to_vec()),
                3 => flags = d.u64()?,
                4 => caption = d.str()?.to_owned(),
                5 => album = d.u64()?,
                6 => id = d.u64()?,
                7 => duration = d.u32()?,
                8 => quote_id = d.u64()?,
                9 => quote_author = Some(cbor::fixed_bytes::<32>(d)?),
                10 => quote_text = Some(d.str()?.to_owned()),
                12 => {
                    for _ in 0..d.array_len()? {
                        let o = d.str()?.to_owned();
                        if options.len() < MAX_POLL_OPTIONS {
                            options.push(o);
                        }
                    }
                }
                13 => {
                    for _ in 0..d.array_len()? {
                        let c = d.u32()?;
                        if choices.len() < MAX_POLL_OPTIONS {
                            choices.push(c);
                        }
                    }
                }
                11 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 3 {
                            return Err(Error::Malformed("mention"));
                        }
                        let (start, len) = (d.u32()?, d.u32()?);
                        let who = cbor::fixed_bytes::<32>(d)?;
                        if mentions.len() < MAX_MENTIONS {
                            mentions.push(Mention { start, len, who });
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(kind, "content kind")? {
            1 => Self::Text {
                mentions: Mention::valid(mentions, text.as_deref().unwrap_or("")),
                text: text.unwrap_or_default(),
                id,
                forwarded: flags & FORWARDED != 0,
                quote: quote_author
                    .map(|a| Quote::new(quote_id, a, quote_text.as_deref().unwrap_or(""))),
            },
            2 => Self::File {
                name: required(text, "file name")?,
                data: required(data, "file data")?,
                sensitive: flags & 1 != 0,
                caption,
                album,
                id,
                clip: Clip::from_flags(flags, duration)?,
                forwarded: flags & FORWARDED != 0,
            },
            3 => {
                let emoji = required(text, "emoji")?;
                if id == 0 || !threnody_core::history::valid_emoji(&emoji) {
                    return Err(Error::Malformed("reaction"));
                }
                Self::React {
                    id,
                    emoji,
                    add: flags & 1 == 0,
                }
            }
            5 if id != 0 => Self::Poll {
                id,
                question: required(text, "poll question")?
                    .chars()
                    .take(MAX_POLL_QUESTION)
                    .collect(),
                options,
                multi: flags & 1 != 0,
            },
            6 if id != 0 => Self::Vote { id, choices },
            7 if id != 0 => Self::ClosePoll { id },
            8 => Self::List(required(data, "list")?),
            9 => Self::Call(required(data, "call")?),
            4 if id != 0 => Self::Pin {
                id,
                pin: flags & 1 == 0,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips_and_plain_text_still_reads() {
        for c in [
            Content::Text {
                text: "hello".into(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            },
            Content::Text {
                text: String::new(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            },
            Content::Text {
                text: "with an id".into(),
                id: 42,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            },
            Content::Text {
                text: "a reply".into(),
                id: 43,
                quote: Some(Quote::new(42, [3; 32], "with an id")),
                mentions: Vec::new(),
                forwarded: false,
            },
            Content::Text {
                text: "@Bo and @Al".into(),
                id: 44,
                quote: None,
                mentions: vec![
                    Mention {
                        start: 0,
                        len: 3,
                        who: [1; 32],
                    },
                    Mention {
                        start: 8,
                        len: 3,
                        who: [2; 32],
                    },
                ],
                forwarded: false,
            },
            Content::React {
                id: 42,
                emoji: "🎉".into(),
                add: true,
            },
            Content::React {
                id: 42,
                emoji: "🎉".into(),
                add: false,
            },
            Content::Pin { id: 42, pin: true },
            Content::Poll {
                id: 50,
                question: "Lunch?".into(),
                options: vec!["Pizza".into(), "Tacos".into()],
                multi: true,
            },
            Content::Vote {
                id: 50,
                choices: vec![1],
            },
            Content::Vote {
                id: 50,
                choices: vec![],
            },
            Content::ClosePoll { id: 50 },
            Content::List(vec![1, 2, 3]),
            Content::Call(vec![4, 5, 6]),
            Content::Pin { id: 42, pin: false },
            Content::File {
                name: "a.txt".into(),
                data: vec![0xFF; 3000],
                sensitive: false,
                caption: String::new(),
                album: 0,
                id: 0,
                clip: None,
                forwarded: false,
            },
            Content::File {
                name: "IMG.jpg".into(),
                data: vec![1; 10],
                sensitive: true,
                caption: "look".into(),
                album: 5,
                id: 9,
                clip: None,
                forwarded: false,
            },
            Content::File {
                name: "voice.ogg".into(),
                data: vec![2; 10],
                sensitive: false,
                caption: String::new(),
                album: 0,
                id: 10,
                clip: Some(Clip {
                    video: false,
                    duration_ms: 2_000,
                }),
                forwarded: false,
            },
        ] {
            assert_eq!(Content::decode(&c.encode().unwrap()).unwrap(), c);
        }
        assert_eq!(
            Content::decode("hi ✓".as_bytes()).unwrap(),
            Content::Text {
                text: "hi ✓".into(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            }
        );
        // A tagged text, as a later sender might write it.
        let tagged = [&[TAGGED][..], &[0xa2, 0, 1, 1, 0x62, b'o', b'k']].concat();
        assert_eq!(
            Content::decode(&tagged).unwrap(),
            Content::Text {
                text: "ok".into(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            }
        );
        assert!(Content::decode(&[TAGGED, 0xa1, 0, 9]).is_err());
        assert!(Content::decode(&[TAGGED]).is_err());
    }

    #[test]
    fn round_trips() {
        let g = [7u8; 16];
        for m in [
            GroupWire::KeyPackageRequest {
                group: g,
                name: "team".into(),
            },
            GroupWire::KeyPackage {
                group: g,
                key_package: vec![1, 2],
            },
            GroupWire::Welcome {
                group: g,
                name: "team".into(),
                welcome: vec![3],
            },
            GroupWire::message(g, vec![4, 5, 6]),
            GroupWire::Forward {
                group: g,
                to: [9; 32],
                message: vec![7],
                reference: 0,
                proposals: Vec::new(),
                sync: None,
            },
            GroupWire::Forward {
                group: g,
                to: [9; 32],
                message: vec![7],
                reference: u64::MAX,
                proposals: Vec::new(),
                sync: None,
            },
            GroupWire::Receipt {
                group: g,
                member: [8; 32],
                reference: 3,
            },
            GroupWire::Leave {
                group: g,
                proposal: None,
            },
            GroupWire::AddDevice {
                group: g,
                device: [7; 32],
                owner: None,
            },
            GroupWire::AddDevice {
                group: g,
                device: [7; 32],
                owner: Some([6; 32]),
            },
        ] {
            assert_eq!(GroupWire::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(GroupWire::decode(&[0xa0]).is_err());
        // A forward must name its target.
        let mut no_target = GroupWire::message(g, vec![1]).encode().unwrap();
        no_target[2] = 5; // { 0: 5, ... }
        assert!(GroupWire::decode(&no_target).is_err());
    }

    #[test]
    fn group_wire_round_trips_new_fields() {
        let g = [7u8; 16];
        let sync = SyncHint {
            epoch: 9,
            term: 2,
            auth: [1; 8],
            lead: [2; 8],
        };
        for m in [
            GroupWire::Message {
                group: g,
                message: vec![4],
                proposals: vec![vec![1, 2], vec![3]],
                sync: Some(sync),
            },
            GroupWire::Message {
                group: g,
                message: vec![4],
                proposals: Vec::new(),
                sync: Some(sync),
            },
            GroupWire::Forward {
                group: g,
                to: [9; 32],
                message: vec![7],
                reference: 5,
                proposals: vec![vec![8]],
                sync: Some(sync),
            },
            GroupWire::Leave {
                group: g,
                proposal: Some(vec![6, 6]),
            },
            GroupWire::PassWelcome {
                group: g,
                member: [3; 32],
                name: "team".into(),
                welcome: vec![1, 2, 3],
            },
            GroupWire::Resync {
                group: g,
                member: None,
                sync,
            },
            GroupWire::Resync {
                group: g,
                member: Some([4; 32]),
                sync,
            },
        ] {
            assert_eq!(GroupWire::decode(&m.encode().unwrap()).unwrap(), m);
        }
        // A resync needs its hint.
        let mut bare = GroupWire::message(g, vec![1]).encode().unwrap();
        bare[2] = 10;
        assert!(GroupWire::decode(&bare).is_err());
    }

    #[test]
    fn legacy_messages_encode_as_before() {
        let g = [7u8; 16];
        // As 0.18.0 encoded them: { 0: 4, 1: g, 2: h'040506' } and
        // { 0: 7, 1: g }.
        let message = [
            &[0xa3, 0x00, 0x04, 0x01, 0x50][..],
            &[7; 16],
            &[0x02, 0x43, 4, 5, 6],
        ]
        .concat();
        assert_eq!(
            GroupWire::message(g, vec![4, 5, 6]).encode().unwrap(),
            message
        );
        let leave = [&[0xa2, 0x00, 0x07, 0x01, 0x50][..], &[7; 16]].concat();
        assert_eq!(
            GroupWire::Leave {
                group: g,
                proposal: None
            }
            .encode()
            .unwrap(),
            leave
        );
        // Keys a later version adds are skipped.
        let later = [
            &[0xa4, 0x00, 0x04, 0x01, 0x50][..],
            &[7; 16],
            &[0x02, 0x43, 4, 5, 6, 0x18, 99, 0x01],
        ]
        .concat();
        assert_eq!(
            GroupWire::decode(&later).unwrap(),
            GroupWire::message(g, vec![4, 5, 6])
        );
    }
}
