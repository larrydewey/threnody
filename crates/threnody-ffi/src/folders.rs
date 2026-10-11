//! Chat folders, pinned and archived chats, unread counts, and when
//! messages notify (see `threnody_core::folders`).
//!
//! Chats are named as the apps name conversations: a contact by its
//! account fingerprint (or device fingerprint without an account), a
//! group by its hex id, and the notes to self as `"notes"`.

use std::sync::atomic::{AtomicI32, Ordering};

use threnody_core::folders::{Breakthrough, ChatFacts, Folder, QuietSlot};
use threnody_core::history::{ConversationId, Notify};
use threnody_core::{PublicIdentity, now_ms};

use crate::groups::hex;
use crate::{NotifyLevel, Result, ThrenodyNode, fail, fp};

/// The notes to self, as a chat key.
pub const NOTES: &str = "notes";

/// Minutes east of UTC, as the app last said (for quiet hours).
static UTC_OFFSET: AtomicI32 = AtomicI32::new(0);

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct QuietSlotInfo {
    /// Bit 0 Monday … bit 6 Sunday.
    pub days: u8,
    /// Minutes after local midnight; an end before the start runs past
    /// midnight.
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderInfo {
    /// 0 for a new folder.
    pub id: u64,
    pub name: String,
    pub emoji: String,
    /// Picked chats (chat keys).
    pub chats: Vec<String>,
    /// `RULE_*` bits: 1 contacts, 2 groups, 4 verified contacts (these
    /// add chats); 8 unread only, 16 unread mentions only (these keep).
    pub rules: u32,
    /// Over each chat's own setting; none: theirs.
    pub notify: Option<NotifyLevel>,
    pub quiet: Vec<QuietSlotInfo>,
    /// Let a quiet folder's messages through when one person writes to
    /// us directly `count` times within `window_min` minutes.
    pub breakthrough_count: u8,
    pub breakthrough_window_min: u16,
}

/// A chat's unread messages, and how many of them mention or reply to us.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChatUnread {
    pub chat: String,
    pub unread: u32,
    pub mentions: u32,
}

impl ThrenodyNode {
    /// The app's name for `conv`.
    pub(crate) fn chat_key(&self, conv: ConversationId) -> String {
        match conv {
            ConversationId::Group(g) => hex(&g),
            ConversationId::Peer(k) => {
                if conv == self.node.notes() {
                    return NOTES.into();
                }
                let contacts = self.node.contacts();
                if let Some(a) = contacts.iter().find_map(|c| c.account.filter(|a| a.0 == k)) {
                    return a.fingerprint().to_string();
                }
                PublicIdentity::from_bytes(&k)
                    .map(|d| fp(&d))
                    .unwrap_or_default()
            }
        }
    }

    /// The conversation an app's chat key names.
    pub(crate) fn chat_conv(&self, key: &str) -> Result<ConversationId> {
        if key == NOTES {
            return Ok(self.node.notes());
        }
        if let Ok(g) = self.group_id(key) {
            return Ok(ConversationId::Group(g));
        }
        let by_account = self
            .node
            .contacts()
            .iter()
            .find(|c| {
                c.account
                    .is_some_and(|a| a.fingerprint().to_string() == key)
            })
            .map(|c| c.key);
        let device = match by_account {
            Some(d) => d,
            None => self.resolve(key)?,
        };
        Ok(self.node.conversation_for(&device))
    }

    /// Every chat the apps list: accepted contacts, groups, the notes.
    fn all_chats(&self) -> Vec<(ConversationId, ChatFacts)> {
        let me = self.node.reactor_of(&self.node.identity());
        let device = *self.node.identity().as_bytes();
        let facts = |conv, group, verified| {
            let (unread, mention) = self
                .node
                .history(conv)
                .map(|h| {
                    let n = h.unread().count();
                    let m = h.unread().filter(|e| mentions_us(e, me, device)).count();
                    (n, m)
                })
                .unwrap_or_default();
            ChatFacts {
                group,
                verified,
                unread: unread > 0,
                unread_mention: mention > 0,
            }
        };
        let mut out: Vec<(ConversationId, ChatFacts)> = Vec::new();
        for c in self.node.contacts().iter() {
            if !c.accepted || c.blocked || self.node.is_own_device(&c.key) {
                continue;
            }
            let conv = self.node.conversation_for(&c.key);
            if out.iter().any(|(x, _)| *x == conv) {
                continue;
            }
            out.push((conv, facts(conv, false, c.verified)));
        }
        for (g, ..) in self.group_node().list() {
            let conv = ConversationId::Group(g);
            out.push((conv, facts(conv, true, false)));
        }
        let notes = self.node.notes();
        out.push((notes, facts(notes, false, true)));
        out
    }

    /// Whether folders quiet a new message in `conv` now. `mentioned`: it
    /// mentions or replies to us; `from`: its sender, whose direct
    /// messages may break through.
    pub(crate) fn folder_quiets(
        &self,
        conv: ConversationId,
        mentioned: bool,
        direct: bool,
        from: Option<[u8; 32]>,
    ) -> bool {
        let folders = self.node.folders();
        if folders.folders.is_empty() {
            return false;
        }
        let chats = self.all_chats();
        let facts = chats
            .iter()
            .find(|(c, _)| *c == conv)
            .map(|(_, f)| *f)
            .unwrap_or_default();
        let now = now_ms();
        let minute = minute_of_week(now, UTC_OFFSET.load(Ordering::Relaxed));
        let me = self.node.reactor_of(&self.node.identity());
        let device = *self.node.identity().as_bytes();
        // The sender's direct messages here, newest last.
        let recent: Vec<u64> = match (from, direct || mentioned) {
            (Some(who), true) => self
                .node
                .history(conv)
                .map(|h| {
                    h.entries()
                        .iter()
                        .filter(|e| !e.outgoing && e.device == who)
                        .filter(|e| direct || mentions_us(e, me, device))
                        .map(|e| e.at_ms)
                        .collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        folders
            .folders
            .iter()
            // Notifications follow only what a folder holds for good, not
            // its unread filters (every new message is unread).
            .filter(|f| {
                let lasting = Folder {
                    rules: f.rules
                        & !(threnody_core::folders::RULE_UNREAD
                            | threnody_core::folders::RULE_MENTIONS),
                    ..(*f).clone()
                };
                lasting.holds(conv, facts)
            })
            .any(|f| f.quiets(minute, mentioned) && !f.breaks_through(&recent, now))
    }
}

/// Whether `e` mentions us or replies to one of our messages.
fn mentions_us(e: &threnody_core::history::Entry, me: [u8; 32], device: [u8; 32]) -> bool {
    e.mentions.iter().any(|m| m.who == me || m.who == device)
        || e.quote.as_ref().is_some_and(|q| q.author == device)
}

/// Local minute of the week (0 = Monday 00:00) at `now_ms`.
fn minute_of_week(now_ms: u64, offset_min: i32) -> u32 {
    let local = (now_ms / 60_000) as i64 + i64::from(offset_min);
    // 1970-01-01 was a Thursday: Monday-based day 3.
    (local + 3 * 1440).rem_euclid(7 * 1440) as u32
}

fn notify_of(n: Option<NotifyLevel>) -> Option<Notify> {
    n.map(Into::into)
}

#[uniffi::export]
impl ThrenodyNode {
    /// Tells the node the local time zone (minutes east of UTC), for
    /// quiet hours. Call it at start and when the zone changes.
    pub fn set_utc_offset(&self, minutes: i32) {
        UTC_OFFSET.store(minutes, Ordering::Relaxed);
    }

    /// The folders, in order.
    pub fn folders(&self) -> Vec<FolderInfo> {
        self.node
            .folders()
            .folders
            .into_iter()
            .map(|f| FolderInfo {
                id: f.id,
                name: f.name,
                emoji: f.emoji,
                chats: f.chats.iter().map(|c| self.chat_key(*c)).collect(),
                rules: f.rules,
                notify: f.notify.map(Into::into),
                quiet: f
                    .quiet
                    .iter()
                    .map(|q| QuietSlotInfo {
                        days: q.days,
                        start: q.start,
                        end: q.end,
                    })
                    .collect(),
                breakthrough_count: f.breakthrough.map_or(0, |b| b.count),
                breakthrough_window_min: f.breakthrough.map_or(0, |b| b.window_min),
            })
            .collect()
    }

    /// Adds (id 0) or changes a folder; returns its id. On all our devices.
    pub fn save_folder(&self, folder: FolderInfo) -> Result<u64> {
        let name = folder.name.trim().to_owned();
        if name.is_empty() {
            return Err(fail("a folder needs a name"));
        }
        let mut chats = Vec::new();
        for k in &folder.chats {
            let c = self.chat_conv(k)?;
            if !chats.contains(&c) {
                chats.push(c);
            }
        }
        let id = if folder.id == 0 {
            threnody_net::history::local_id()
        } else {
            folder.id
        };
        let f = Folder {
            id,
            name,
            emoji: folder.emoji,
            chats,
            rules: folder.rules,
            notify: notify_of(folder.notify),
            quiet: folder
                .quiet
                .iter()
                .map(|q| QuietSlot {
                    days: q.days,
                    start: q.start,
                    end: q.end,
                })
                .collect(),
            breakthrough: (folder.breakthrough_count > 0).then_some(Breakthrough {
                count: folder.breakthrough_count,
                window_min: folder.breakthrough_window_min.max(1),
            }),
        };
        self.node.update_folders(|all| {
            match all.folders.iter_mut().find(|x| x.id == id) {
                Some(x) => *x = f,
                None => all.folders.push(f),
            }
            true
        });
        Ok(id)
    }

    pub fn delete_folder(&self, id: u64) {
        self.node.update_folders(|all| {
            let before = all.folders.len();
            all.folders.retain(|f| f.id != id);
            all.folders.len() != before
        });
    }

    /// Puts the folders in the order of `ids` (others keep theirs, after).
    pub fn order_folders(&self, ids: Vec<u64>) {
        self.node.update_folders(|all| {
            all.folders
                .sort_by_key(|f| ids.iter().position(|i| *i == f.id).unwrap_or(usize::MAX));
            true
        });
    }

    /// Adds (or takes away) one chat in a folder.
    pub fn set_in_folder(&self, folder: u64, chat: String, inside: bool) -> Result<()> {
        let c = self.chat_conv(&chat)?;
        self.node.update_folders(|all| {
            let Some(f) = all.folders.iter_mut().find(|f| f.id == folder) else {
                return false;
            };
            let has = f.chats.contains(&c);
            if inside && !has {
                f.chats.push(c);
            } else if !inside && has {
                f.chats.retain(|x| *x != c);
            } else {
                return false;
            }
            true
        });
        Ok(())
    }

    /// The chats folder `id` shows now (chat keys).
    pub fn folder_chats(&self, id: u64) -> Vec<String> {
        let Some(f) = self.node.folders().folders.into_iter().find(|f| f.id == id) else {
            return Vec::new();
        };
        self.all_chats()
            .into_iter()
            .filter(|(c, facts)| f.holds(*c, *facts))
            .map(|(c, _)| self.chat_key(c))
            .collect()
    }

    /// Pinned chats, in order (they go first in every list).
    pub fn pinned_chats(&self) -> Vec<String> {
        self.node
            .folders()
            .pinned
            .iter()
            .map(|c| self.chat_key(*c))
            .collect()
    }

    /// Pins (at the end) or unpins a chat; at most ten stay pinned.
    pub fn set_chat_pinned(&self, chat: String, pinned: bool) -> Result<()> {
        let c = self.chat_conv(&chat)?;
        self.node.update_folders(|all| {
            let has = all.pinned.contains(&c);
            if pinned && !has {
                if all.pinned.len() >= threnody_core::folders::MAX_PINNED_CHATS {
                    return false;
                }
                all.pinned.push(c);
            } else if !pinned && has {
                all.pinned.retain(|x| *x != c);
            } else {
                return false;
            }
            true
        });
        Ok(())
    }

    /// Sets the pinned chats, in order (at most ten; others are unpinned).
    pub fn set_pinned_chats(&self, chats: Vec<String>) -> Result<()> {
        let mut pinned = Vec::new();
        for k in &chats {
            let c = self.chat_conv(k)?;
            if !pinned.contains(&c) {
                pinned.push(c);
            }
        }
        pinned.truncate(threnody_core::folders::MAX_PINNED_CHATS);
        self.node.update_folders(|all| {
            if all.pinned == pinned {
                return false;
            }
            all.archived.retain(|c| !pinned.contains(c));
            all.pinned = pinned;
            true
        });
        Ok(())
    }

    /// Archived chats: hidden from the lists until a message arrives.
    pub fn archived_chats(&self) -> Vec<String> {
        self.node
            .folders()
            .archived
            .iter()
            .map(|c| self.chat_key(*c))
            .collect()
    }

    pub fn set_chat_archived(&self, chat: String, archived: bool) -> Result<()> {
        let c = self.chat_conv(&chat)?;
        self.node.update_folders(|all| {
            let has = all.archived.contains(&c);
            if archived && !has {
                all.archived.push(c);
                all.pinned.retain(|x| *x != c);
            } else if !archived && has {
                all.archived.retain(|x| *x != c);
            } else {
                return false;
            }
            true
        });
        Ok(())
    }

    /// Unread messages per chat (only chats with some).
    pub fn unread_counts(&self) -> Vec<ChatUnread> {
        let me = self.node.reactor_of(&self.node.identity());
        let device = *self.node.identity().as_bytes();
        self.all_chats()
            .into_iter()
            .filter(|(_, f)| f.unread)
            .filter_map(|(c, _)| {
                let h = self.node.history(c).ok()?;
                Some(ChatUnread {
                    chat: self.chat_key(c),
                    unread: u32::try_from(h.unread().count()).unwrap_or(u32::MAX),
                    mentions: u32::try_from(
                        h.unread().filter(|e| mentions_us(e, me, device)).count(),
                    )
                    .unwrap_or(u32::MAX),
                })
            })
            .collect()
    }

    /// The user looked at `chat`: its messages are read.
    pub fn mark_seen(&self, chat: String) -> Result<()> {
        let c = self.chat_conv(&chat)?;
        self.node.mark_seen(c);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minutes_of_the_week_start_on_monday() {
        // 1970-01-05 00:00 UTC was a Monday.
        let monday = 4 * 86_400_000;
        assert_eq!(minute_of_week(monday, 0), 0);
        assert_eq!(minute_of_week(monday + 90 * 60_000, 0), 90);
        // Sunday 23:00 local, an hour behind UTC midnight Monday.
        assert_eq!(minute_of_week(monday, -60), 7 * 1440 - 60);
    }
}
