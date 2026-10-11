//! MLS groups for apps (Appendix F), on the shared
//! [`threnody_groups::node::GroupNode`]: persistence, consent, and delivery
//! through sessions, mailboxes, other members or relays.

use threnody_core::PublicIdentity;
use threnody_core::history::{ConversationId, FileNote};
use threnody_core::message::Mention;
use threnody_groups::GroupId;
use threnody_groups::node::{Invite, Update};

use threnody_net::history::OutgoingFile;

use crate::{
    FileOptions, HistoryEntry, MentionInfo, NodeEvent, NotifyLevel, Result, ThrenodyNode, fail, fp,
    history_entries,
};

/// A group this device belongs to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupInfo {
    /// Hex group id, used to address the group in every call.
    pub id: String,
    pub name: String,
    pub owner: String,
    /// Member device fingerprints, including ours.
    pub members: Vec<String>,
    /// Member roles corresponding to `members`: 0=Owner, 1=Admin, 2=Member
    pub member_roles: Vec<u8>,
    /// Whether we own it (only the owner adds and removes members).
    pub owned: bool,
}

/// Member role in a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum MemberRole {
    Owner = 0,
    Admin = 1,
    Member = 2,
}

/// An invitation waiting for the user's consent.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupInvite {
    pub group: String,
    pub name: String,
    pub from: String,
}

pub(crate) fn hex(g: &GroupId) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

fn invite(i: &Invite) -> GroupInvite {
    GroupInvite {
        group: hex(&i.group),
        name: i.name.clone(),
        from: fp(&i.from),
    }
}

fn event(u: Update) -> NodeEvent {
    match u {
        Update::Joined { group, name, owner } => NodeEvent::GroupJoined {
            group: hex(&group),
            name,
            owner: fp(&owner),
        },
        Update::Invited(i) => {
            let i = invite(&i);
            NodeEvent::GroupInvited {
                group: i.group,
                name: i.name,
                from: i.from,
            }
        }
        Update::MemberAdded { group, member } => NodeEvent::GroupMembersChanged {
            group: hex(&group),
            added: vec![fp(&member)],
            removed: vec![],
        },
        Update::MemberRemoved { group, member } => NodeEvent::GroupMembersChanged {
            group: hex(&group),
            added: vec![],
            removed: vec![fp(&member)],
        },
        Update::Left { group } => NodeEvent::GroupLeft { group: hex(&group) },
        Update::RoleChanged {
            group,
            member,
            role,
        } => NodeEvent::GroupRoleChanged {
            group: hex(&group),
            member: fp(&member),
            role: role as u8,
        },
        Update::Voted { group, from } => NodeEvent::Reacted {
            peer: fp(&from),
            group: Some(hex(&group)),
        },
        Update::Pinned { group, from, pin } => NodeEvent::Pinned {
            peer: fp(&from),
            group: Some(hex(&group)),
            pin,
        },
        Update::Reacted { group, from } => NodeEvent::Reacted {
            peer: fp(&from),
            group: Some(hex(&group)),
        },
        Update::Text {
            group,
            from,
            text,
            ours,
            mentioned,
        } => NodeEvent::GroupMessage {
            group: hex(&group),
            from: fp(&from),
            text,
            ours,
            mentioned,
            quiet: false,
        },
        Update::File {
            group,
            from,
            name,
            data,
            ours,
            sensitive,
            caption,
            album,
            id,
            clip,
            forwarded,
        } => NodeEvent::GroupFile {
            id,
            group: hex(&group),
            from: fp(&from),
            name,
            data,
            ours,
            sensitive,
            caption,
            album,
            clip: clip.map(Into::into),
            quiet: false,
            forwarded,
        },
    }
}

impl ThrenodyNode {
    /// Handles an incoming group payload, queueing the resulting events.
    pub(crate) fn group_incoming(&self, from: PublicIdentity, payload: &[u8]) {
        let _guard = self.rt.enter();
        let r = self.group_node().incoming(&self.node, from, payload);
        let mut q = self.queued();
        match r {
            Ok(updates) => q.extend(updates.into_iter().map(event)),
            Err(e) => q.push_back(NodeEvent::Other {
                description: format!("group message from {}: {e}", fp(&from)),
            }),
        }
    }

    /// Sends group messages held for a peer that just connected.
    pub(crate) fn group_connected(&self, peer: &PublicIdentity) {
        let _guard = self.rt.enter();
        self.group_node().connected(&self.node, peer);
    }

    /// Adds our newly linked device to our groups.
    pub(crate) fn group_linked(&self, device: &PublicIdentity) {
        let _guard = self.rt.enter();
        self.group_node().linked(&self.node, device);
    }

    fn queue_updates(&self, updates: Vec<Update>) {
        self.queued().extend(updates.into_iter().map(event));
    }

    /// A group (or invitation) by hex id prefix.
    pub(crate) fn group_id(&self, id: &str) -> Result<GroupId> {
        let gn = self.group_node();
        let id = id.to_ascii_lowercase();
        let mut hits: Vec<GroupId> = gn
            .list()
            .into_iter()
            .map(|(g, ..)| g)
            .chain(gn.invites().iter().map(|i| i.group))
            .filter(|g| id.len() >= 2 && hex(g).starts_with(&id))
            .collect();
        hits.sort_unstable();
        hits.dedup();
        match hits.as_slice() {
            [g] => Ok(*g),
            [] => Err(fail(format!("no group {id}"))),
            _ => Err(fail(format!("{id} matches several groups"))),
        }
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Creates a group that we own; returns its id.
    pub fn create_group(&self, name: String) -> Result<String> {
        self.group_node()
            .create(&name)
            .map(|g| hex(&g))
            .map_err(fail)
    }

    pub fn groups(&self) -> Vec<GroupInfo> {
        let me = self.node.identity();
        self.group_node()
            .list_with_roles()
            .into_iter()
            .map(|(g, name, owner, members, roles)| GroupInfo {
                id: hex(&g),
                name,
                owner: fp(&owner),
                members: members.iter().map(fp).collect(),
                member_roles: roles.iter().map(|r| *r as u8).collect(),
                owned: owner == me,
            })
            .collect()
    }

    /// Invitations waiting for `accept_group_invite` or `decline_group_invite`.
    pub fn group_invites(&self) -> Vec<GroupInvite> {
        self.group_node().invites().iter().map(invite).collect()
    }

    /// Invites every device of `peer`'s account (we must own the group).
    /// They join once they accept (automatically if they approved us).
    pub fn invite_to_group(&self, group: String, peer: String) -> Result<()> {
        let (g, p) = (self.group_id(&group)?, self.resolve(&peer)?);
        let _guard = self.rt.enter();
        self.group_node()
            .invite(&self.node, &g, &p)
            .map(|_| ())
            .map_err(fail)
    }

    pub fn accept_group_invite(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        let updates = self.group_node().accept(&self.node, &g).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    pub fn decline_group_invite(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_node().decline(&g);
        Ok(())
    }

    /// Removes every device of `peer`'s account (we must own the group).
    pub fn remove_from_group(&self, group: String, peer: String) -> Result<()> {
        let (g, p) = (self.group_id(&group)?, self.resolve(&peer)?);
        let _guard = self.rt.enter();
        let updates = self.group_node().remove(&self.node, &g, &p).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// Promotes `peer` to Admin (only the group owner can do this).
    pub fn promote_in_group(&self, group: String, peer: String) -> Result<()> {
        let (g, p) = (self.group_id(&group)?, self.resolve(&peer)?);
        let _guard = self.rt.enter();
        let updates = self.group_node().promote(&self.node, &g, p).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// Demotes `peer` from Admin to Member (only the group owner can do this).
    pub fn demote_in_group(&self, group: String, peer: String) -> Result<()> {
        let (g, p) = (self.group_id(&group)?, self.resolve(&peer)?);
        let _guard = self.rt.enter();
        let updates = self.group_node().demote(&self.node, &g, p).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// Leaves a group (a member asks the owner to remove it) or, for the
    /// owner, deletes it for everyone. Its history stays.
    pub fn leave_group(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        let updates = self.group_node().leave(&self.node, &g).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// Sends text to every other member (recorded in the group's history).
    /// Members we can't reach get it through another member, or once
    /// they connect.
    pub fn send_group_text(&self, group: String, text: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .send_text(&self.node, &g, &text)
            .map_err(fail)
    }

    /// `send_group_text`, replying to the message in the group that
    /// `at_ms` and `device` name (as in [`crate::HistoryEntry`]).
    pub fn send_group_reply(
        &self,
        group: String,
        text: String,
        at_ms: u64,
        device: String,
    ) -> Result<()> {
        let g = self.group_id(&group)?;
        let quote = self.node.quote_of(
            threnody_core::history::ConversationId::Group(g),
            crate::entry_at(at_ms, &device),
        );
        let _guard = self.rt.enter();
        self.group_node()
            .send_reply(&self.node, &g, &text, quote)
            .map_err(fail)
    }

    /// Sends a file to every other member, recorded in the group's history
    /// with `location` (where it is on this device).
    pub fn send_group_file(
        &self,
        group: String,
        name: String,
        data: Vec<u8>,
        location: Option<String>,
        options: FileOptions,
    ) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        let file = OutgoingFile {
            name,
            data,
            location,
            sensitive: options.sensitive,
            caption: options.caption,
            album: options.album,
            clip: options.clip.map(Into::into),
            forwarded: options.forwarded,
        };
        self.group_node()
            .send_file(&self.node, &g, file)
            .map_err(fail)
    }

    /// Records a file from a `GroupFile` event in the group's history once
    /// the app has saved it at `location`.
    #[allow(clippy::too_many_arguments)]
    pub fn record_received_group_file(
        &self,
        group: String,
        from: String,
        name: String,
        size: u64,
        location: Option<String>,
        id: u64,
        options: FileOptions,
    ) -> Result<()> {
        let g = self.group_id(&group)?;
        // Members needn't be contacts: find the sender among them.
        let from = self
            .group_node()
            .list()
            .into_iter()
            .find(|(id, ..)| *id == g)
            .and_then(|(.., members)| members.into_iter().find(|m| fp(m) == from))
            .ok_or_else(|| fail(format!("{from} is not in this group")))?;
        self.group_node().record_file(
            &self.node,
            &g,
            &from,
            FileNote {
                name,
                size,
                location,
                sensitive: options.sensitive,
                album: options.album,
                clip: options.clip.map(Into::into),
                forwarded: options.forwarded,
            },
            &options.caption,
            id,
        );
        Ok(())
    }

    /// Adds (or takes away) our `emoji` on message `id` in `group`, for
    /// every member. Returns false if there's no such message.
    pub fn react_in_group(&self, group: String, id: u64, emoji: String, add: bool) -> Result<bool> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .react(&self.node, &g, id, &emoji, add)
            .map_err(fail)
    }

    /// Pins (or unpins) message `id` in `group` for every member; only the
    /// owner and admins may (see `may_pin_in_group`). Returns false if
    /// there's no such message.
    pub fn pin_in_group(&self, group: String, id: u64, pin: bool) -> Result<bool> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node().pin(&self.node, &g, id, pin).map_err(fail)
    }

    /// What we started writing to `group` and haven't sent ("" if nothing).
    pub fn group_draft(&self, group: String) -> Result<String> {
        let g = self.group_id(&group)?;
        Ok(self.draft_in(ConversationId::Group(g)))
    }

    /// Keeps what we started writing to `group` (encrypted, on this device).
    pub fn set_group_draft(&self, group: String, text: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.node
            .set_draft(ConversationId::Group(g), &text)
            .map_err(fail)
    }

    /// `send_group_text`, marked as forwarded from another chat.
    pub fn forward_group_text(&self, group: String, text: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .send_text_with(
                &self.node,
                &g,
                &text,
                threnody_net::history::Extras {
                    forwarded: true,
                    ..Default::default()
                },
            )
            .map_err(fail)
    }

    /// Asks `group` a question with 2 to 12 `options`; `multi` lets members
    /// choose several.
    pub fn send_group_poll(
        &self,
        group: String,
        question: String,
        options: Vec<String>,
        multi: bool,
    ) -> Result<()> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .send_poll(&self.node, &g, &question, options, multi)
            .map_err(fail)
    }

    /// Sets our vote in poll `id` (a [`HistoryEntry::id`]) in `group`, by
    /// option index; none takes it back. False if nothing changed.
    pub fn vote_in_group(&self, group: String, id: u64, choices: Vec<u32>) -> Result<bool> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .vote(&self.node, &g, id, &choices)
            .map_err(fail)
    }

    /// Closes our poll `id` in `group`: it takes no more votes.
    pub fn close_group_poll(&self, group: String, id: u64) -> Result<bool> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        self.group_node()
            .close_poll(&self.node, &g, id)
            .map_err(fail)
    }

    /// When new messages in `group` notify.
    pub fn group_notify_level(&self, group: String) -> Result<NotifyLevel> {
        let g = self.group_id(&group)?;
        let h = self.node.history(ConversationId::Group(g)).map_err(fail)?;
        Ok(h.notify.into())
    }

    pub fn set_group_notify_level(&self, group: String, level: NotifyLevel) -> Result<()> {
        let g = self.group_id(&group)?;
        self.node
            .set_notify(ConversationId::Group(g), level.into())
            .map_err(fail)
    }

    /// Sends text to the group, mentioning `mentions` (each naming a
    /// member's device; `start` and `len` count characters of `text`) and,
    /// with `quote_at_ms` and `quote_device`, replying to that message.
    pub fn send_group_message(
        &self,
        group: String,
        text: String,
        quote_at_ms: Option<u64>,
        quote_device: Option<String>,
        mentions: Vec<MentionInfo>,
    ) -> Result<()> {
        let g = self.group_id(&group)?;
        let conv = ConversationId::Group(g);
        let quote = match (quote_at_ms, &quote_device) {
            (Some(at), Some(d)) => self.node.quote_of(conv, crate::entry_at(at, d)),
            _ => None,
        };
        let members = self
            .group_node()
            .list()
            .into_iter()
            .find(|(id, ..)| *id == g)
            .map(|(.., m)| m)
            .unwrap_or_default();
        let mentions = mentions
            .iter()
            .filter_map(|m| {
                let d = members.iter().find(|d| fp(d) == m.device)?;
                Some(Mention {
                    start: m.start,
                    len: m.len,
                    who: self.node.reactor_of(d),
                })
            })
            .collect();
        let _guard = self.rt.enter();
        self.group_node()
            .send_message(&self.node, &g, &text, quote, mentions)
            .map_err(fail)
    }

    /// Whether we may pin messages in `group` (we own it, or are an admin).
    pub fn may_pin_in_group(&self, group: String) -> Result<bool> {
        let g = self.group_id(&group)?;
        Ok(self.group_node().may_pin(&g, &self.node.identity()))
    }

    /// The pinned messages in `group`, oldest pin first.
    pub fn group_pinned(&self, group: String) -> Result<Vec<HistoryEntry>> {
        let g = self.group_id(&group)?;
        let me = self.node.reactor_of(&self.node.identity());
        Ok(history_entries(
            &self.node,
            &self.node.pinned(ConversationId::Group(g)),
            me,
        ))
    }

    /// Clears a group's messages on this device; members keep theirs.
    pub fn clear_group_conversation(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.node.clear_group_conversation(g);
        Ok(())
    }

    /// A group's disappearing timer on this device (`None` = off).
    pub fn group_disappearing(&self, group: String) -> Result<Option<u32>> {
        let g = self.group_id(&group)?;
        Ok(self.node.effective_timer(ConversationId::Group(g)))
    }

    pub fn set_group_disappearing(&self, group: String, seconds: Option<u32>) -> Result<()> {
        let g = self.group_id(&group)?;
        self.node
            .set_conversation_timer(ConversationId::Group(g), seconds)
            .map_err(fail)
    }

    /// Deletes one message from a group's history on this device.
    pub fn delete_group_entry(&self, group: String, at_ms: u64, device: String) -> Result<u32> {
        let g = self.group_id(&group)?;
        Ok(self.delete_any(ConversationId::Group(g), at_ms, &device))
    }

    /// Messages in `group` matching `query` (as `search_messages`),
    /// newest first, at most `limit`.
    pub fn search_group_messages(
        &self,
        group: String,
        query: String,
        limit: u32,
    ) -> Result<Vec<HistoryEntry>> {
        let g = self.group_id(&group)?;
        let hits = self
            .node
            .search(Some(ConversationId::Group(g)), &query, limit as usize)
            .map_err(fail)?;
        let me = self.node.reactor_of(&self.node.identity());
        let entries: Vec<_> = hits.into_iter().map(|h| h.entry).collect();
        Ok(history_entries(&self.node, &entries, me))
    }

    /// The last `limit` messages in a group (oldest first).
    pub fn group_history(&self, group: String, limit: u32) -> Result<Vec<HistoryEntry>> {
        let g = self.group_id(&group)?;
        let h = self.node.history(ConversationId::Group(g)).map_err(fail)?;
        let me = self.node.reactor_of(&self.node.identity());
        Ok(history_entries(&self.node, h.recent(limit as usize), me))
    }
}
