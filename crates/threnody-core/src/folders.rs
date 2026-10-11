//! Chat folders: the user's own groupings of conversations, with when
//! they notify; plus pinned and archived chats. Kept encrypted, and
//! shared by an account's devices (the newest copy wins).

use const_cbor::Decoder;

use crate::cbor::{self, finish, read_map, required};
use crate::error::{Error, Result};
use crate::history::{ConversationId, Notify};

/// Most folders, chats per folder, quiet slots per folder, and pinned
/// chats.
pub const MAX_FOLDERS: usize = 32;
pub const MAX_FOLDER_CHATS: usize = 512;
pub const MAX_QUIET_SLOTS: usize = 16;
pub const MAX_PINNED_CHATS: usize = 10;
/// Longest folder name, in characters.
pub const MAX_FOLDER_NAME: usize = 40;

/// Folder rules. The first three add chats (besides the picked ones);
/// the last two keep only some of them.
pub const RULE_CONTACTS: u32 = 1;
pub const RULE_GROUPS: u32 = 2;
pub const RULE_VERIFIED: u32 = 4;
pub const RULE_UNREAD: u32 = 8;
pub const RULE_MENTIONS: u32 = 16;
const ADDING: u32 = RULE_CONTACTS | RULE_GROUPS | RULE_VERIFIED;

/// A stretch of the week when a folder stays quiet: on the days in
/// `days` (bit 0 Monday … bit 6 Sunday), from `start` to `end`, in
/// minutes after local midnight. An `end` before `start` runs past
/// midnight into the next day.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuietSlot {
    pub days: u8,
    pub start: u16,
    pub end: u16,
}

impl QuietSlot {
    /// Whether local `minute_of_week` (0 = Monday 00:00) falls in it.
    pub fn covers(&self, minute_of_week: u32) -> bool {
        let day = (minute_of_week / 1440) % 7;
        let min = (minute_of_week % 1440) as u16;
        let on = |d: u32| self.days & (1 << (d % 7)) != 0;
        if self.start <= self.end {
            on(day) && min >= self.start && min < self.end
        } else {
            // Overnight: the evening of a chosen day, or the morning after.
            (on(day) && min >= self.start) || (on(day + 6) && min < self.end)
        }
    }
}

/// Lets a quiet folder's messages through anyway: when one person writes
/// to us directly (1:1, or mentioning or replying to us in a group)
/// `count` times within `window_min` minutes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Breakthrough {
    pub count: u8,
    pub window_min: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folder {
    pub id: u64,
    pub name: String,
    pub emoji: String,
    /// Chats picked for it.
    pub chats: Vec<ConversationId>,
    /// `RULE_*` bits.
    pub rules: u32,
    /// When its chats notify, over their own setting (`None`: theirs).
    pub notify: Option<Notify>,
    pub quiet: Vec<QuietSlot>,
    pub breakthrough: Option<Breakthrough>,
}

/// What a folder's rules know about one chat.
#[derive(Clone, Copy, Debug, Default)]
pub struct ChatFacts {
    pub group: bool,
    pub verified: bool,
    pub unread: bool,
    pub unread_mention: bool,
}

impl Folder {
    /// Whether `conv` is in the folder.
    pub fn holds(&self, conv: ConversationId, f: ChatFacts) -> bool {
        let picked = self.chats.contains(&conv);
        let added = (self.rules & RULE_CONTACTS != 0 && !f.group)
            || (self.rules & RULE_GROUPS != 0 && f.group)
            || (self.rules & RULE_VERIFIED != 0 && !f.group && f.verified);
        // Only keeping rules: they apply to every chat.
        let base = picked || added || (self.chats.is_empty() && self.rules & ADDING == 0);
        base && (self.rules & RULE_UNREAD == 0 || f.unread)
            && (self.rules & RULE_MENTIONS == 0 || f.unread_mention)
    }

    /// Whether the folder quiets a new message now: by its own setting,
    /// or a quiet slot. `mentioned` says it mentions or replies to us.
    pub fn quiets(&self, minute_of_week: u32, mentioned: bool) -> bool {
        let by_setting = match self.notify {
            None | Some(Notify::All) => false,
            Some(Notify::Mentions) => !mentioned,
            Some(Notify::Off) => true,
        };
        by_setting || self.quiet.iter().any(|q| q.covers(minute_of_week))
    }

    /// Whether `recent` (the times of one person's direct messages in the
    /// chat, the new one included) breaks through at `now_ms`.
    pub fn breaks_through(&self, recent: &[u64], now_ms: u64) -> bool {
        self.breakthrough.is_some_and(|b| {
            let since = now_ms.saturating_sub(u64::from(b.window_min) * 60_000);
            recent.iter().filter(|t| **t >= since).count() >= usize::from(b.count.max(1))
        })
    }
}

/// The folders, pinned chats and archived chats, as one shared record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folders {
    /// When it last changed (on any of our devices); the newest wins.
    pub updated_ms: u64,
    pub folders: Vec<Folder>,
    /// Pinned chats, in order.
    pub pinned: Vec<ConversationId>,
    pub archived: Vec<ConversationId>,
}

const VERSION: u64 = 1;

fn put_conv(
    e: &mut const_cbor::Encoder<'_>,
    c: &ConversationId,
) -> core::result::Result<(), const_cbor::Error> {
    e.bytes(&c.to_bytes())?;
    Ok(())
}

fn get_convs(d: &mut Decoder<'_>, max: usize) -> Result<Vec<ConversationId>> {
    let mut out = Vec::new();
    for _ in 0..d.array_len()? {
        let b = d.bytes()?;
        if let Some(c) = ConversationId::from_bytes(b)
            && out.len() < max
            && !out.contains(&c)
        {
            out.push(c);
        }
    }
    Ok(out)
}

impl Folders {
    /// Rejects what doesn't fit the limits, so a bad copy from another
    /// device can't grow without bound.
    pub fn trimmed(mut self) -> Self {
        self.folders.truncate(MAX_FOLDERS);
        for f in &mut self.folders {
            f.name = f.name.chars().take(MAX_FOLDER_NAME).collect();
            f.emoji = f.emoji.chars().take(8).collect();
            f.chats.truncate(MAX_FOLDER_CHATS);
            f.quiet.truncate(MAX_QUIET_SLOTS);
            for q in &mut f.quiet {
                q.days &= 0x7F;
                q.start = q.start.min(1439);
                q.end = q.end.min(1440);
            }
        }
        self.pinned.truncate(MAX_PINNED_CHATS);
        self
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let hint = 64 + self.folders.len() * 256 + (self.pinned.len() + self.archived.len()) * 40;
        cbor::to_vec(hint, |e| {
            e.map_len(5)?;
            e.u8(0)?.u64(VERSION)?;
            e.u8(1)?.u64(self.updated_ms)?;
            e.u8(2)?.array_len(self.folders.len())?;
            for f in &self.folders {
                e.map_len(
                    5 + usize::from(f.notify.is_some())
                        + usize::from(!f.quiet.is_empty())
                        + usize::from(f.breakthrough.is_some()),
                )?;
                e.u8(0)?.u64(f.id)?;
                e.u8(1)?.str(&f.name)?;
                e.u8(2)?.str(&f.emoji)?;
                e.u8(3)?.array_len(f.chats.len())?;
                for c in &f.chats {
                    put_conv(e, c)?;
                }
                e.u8(4)?.u32(f.rules)?;
                if let Some(n) = f.notify {
                    e.u8(5)?.u8(n.to_u8())?;
                }
                if !f.quiet.is_empty() {
                    e.u8(6)?.array_len(f.quiet.len())?;
                    for q in &f.quiet {
                        e.array_len(3)?.u8(q.days)?.u16(q.start)?.u16(q.end)?;
                    }
                }
                if let Some(b) = f.breakthrough {
                    e.u8(7)?.array_len(2)?.u8(b.count)?.u16(b.window_min)?;
                }
            }
            e.u8(3)?.array_len(self.pinned.len())?;
            for c in &self.pinned {
                put_conv(e, c)?;
            }
            e.u8(4)?.array_len(self.archived.len())?;
            for c in &self.archived {
                put_conv(e, c)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut out = Self::default();
        let mut ver = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => out.updated_ms = d.u64()?,
                2 => {
                    for _ in 0..d.array_len()? {
                        let f = decode_folder(d)?;
                        if out.folders.len() < MAX_FOLDERS {
                            out.folders.push(f);
                        }
                    }
                }
                3 => out.pinned = get_convs(d, MAX_PINNED_CHATS)?,
                4 => out.archived = get_convs(d, usize::MAX)?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(VERSION) {
            return Err(Error::Malformed("folders version"));
        }
        Ok(out.trimmed())
    }
}

fn decode_folder(d: &mut Decoder<'_>) -> Result<Folder> {
    let mut f = Folder::default();
    let mut id = None;
    read_map(d, |k, d| {
        match k {
            0 => id = Some(d.u64()?),
            1 => f.name = d.str()?.to_owned(),
            2 => f.emoji = d.str()?.to_owned(),
            3 => f.chats = get_convs(d, MAX_FOLDER_CHATS)?,
            4 => f.rules = d.u32()?,
            5 => f.notify = Some(Notify::from_u8(d.u8()?)),
            6 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 3 {
                        return Err(Error::Malformed("quiet slot"));
                    }
                    let q = QuietSlot {
                        days: d.u8()?,
                        start: d.u16()?,
                        end: d.u16()?,
                    };
                    if f.quiet.len() < MAX_QUIET_SLOTS {
                        f.quiet.push(q);
                    }
                }
            }
            7 => {
                if d.array_len()? != 2 {
                    return Err(Error::Malformed("breakthrough"));
                }
                f.breakthrough = Some(Breakthrough {
                    count: d.u8()?,
                    window_min: d.u16()?,
                });
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    f.id = required(id, "folder id")?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: u8 = 1;
    const FRI: u8 = 1 << 4;
    const SAT: u8 = 1 << 5;
    const SUN: u8 = 1 << 6;
    fn at(day: u32, h: u32, m: u32) -> u32 {
        day * 1440 + h * 60 + m
    }

    #[test]
    fn quiet_slots_cover_days_and_nights() {
        let evenings = QuietSlot {
            days: MON | FRI,
            start: 18 * 60,
            end: 8 * 60,
        };
        assert!(evenings.covers(at(0, 19, 0)), "Monday evening");
        assert!(evenings.covers(at(1, 7, 59)), "Tuesday morning after");
        assert!(!evenings.covers(at(1, 8, 0)));
        assert!(!evenings.covers(at(1, 19, 0)), "Tuesday isn't chosen");
        assert!(
            evenings.covers(at(5, 3, 0)),
            "Saturday morning after Friday"
        );
        let weekend = QuietSlot {
            days: SAT | SUN,
            start: 0,
            end: 1440,
        };
        assert!(weekend.covers(at(6, 23, 59)) && !weekend.covers(at(0, 0, 0)));
        // Sunday night runs into Monday morning.
        let sunday_night = QuietSlot {
            days: SUN,
            start: 22 * 60,
            end: 6 * 60,
        };
        assert!(sunday_night.covers(at(0, 5, 0)));
    }

    #[test]
    fn folders_hold_by_pick_and_rule_and_break_through() {
        let a = ConversationId::Peer([1; 32]);
        let g = ConversationId::Group([2; 16]);
        let fam = Folder {
            id: 1,
            chats: vec![a],
            rules: RULE_UNREAD,
            ..Folder::default()
        };
        let unread = ChatFacts {
            unread: true,
            ..ChatFacts::default()
        };
        assert!(fam.holds(a, unread) && !fam.holds(a, ChatFacts::default()));
        assert!(!fam.holds(g, unread), "not picked");
        let all_unread = Folder {
            id: 2,
            rules: RULE_UNREAD,
            ..Folder::default()
        };
        assert!(
            all_unread.holds(g, unread),
            "keeping rules alone cover every chat"
        );
        let groups = Folder {
            id: 3,
            rules: RULE_GROUPS,
            ..Folder::default()
        };
        let group = ChatFacts {
            group: true,
            ..ChatFacts::default()
        };
        assert!(groups.holds(g, group) && !groups.holds(a, ChatFacts::default()));

        let work = Folder {
            id: 4,
            notify: Some(Notify::Mentions),
            quiet: vec![QuietSlot {
                days: SAT | SUN,
                start: 0,
                end: 1440,
            }],
            breakthrough: Some(Breakthrough {
                count: 2,
                window_min: 10,
            }),
            ..Folder::default()
        };
        assert!(work.quiets(at(0, 9, 0), false) && !work.quiets(at(0, 9, 0), true));
        assert!(
            work.quiets(at(5, 9, 0), true),
            "weekend quiets even mentions"
        );
        let now = 100 * 60_000;
        assert!(!work.breaks_through(&[now], now));
        assert!(work.breaks_through(&[now - 9 * 60_000, now], now));
        assert!(!work.breaks_through(&[now - 11 * 60_000, now], now));
    }

    #[test]
    fn folders_round_trip_and_trim() {
        let f = Folders {
            updated_ms: 7,
            folders: vec![Folder {
                id: 9,
                name: "Family".into(),
                emoji: "🏡".into(),
                chats: vec![
                    ConversationId::Peer([3; 32]),
                    ConversationId::Group([4; 16]),
                ],
                rules: RULE_UNREAD | RULE_CONTACTS,
                notify: Some(Notify::Off),
                quiet: vec![QuietSlot {
                    days: 0x7F,
                    start: 1320,
                    end: 420,
                }],
                breakthrough: Some(Breakthrough {
                    count: 3,
                    window_min: 5,
                }),
            }],
            pinned: vec![ConversationId::Group([4; 16])],
            archived: vec![ConversationId::Peer([5; 32])],
        };
        assert_eq!(Folders::decode(&f.encode().unwrap()).unwrap(), f);
        let long = Folders {
            folders: vec![Folder {
                name: "x".repeat(100),
                ..Folder::default()
            }],
            ..Folders::default()
        }
        .trimmed();
        assert_eq!(long.folders[0].name.chars().count(), MAX_FOLDER_NAME);
    }
}
