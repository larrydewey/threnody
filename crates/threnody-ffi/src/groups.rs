//! MLS groups for apps (Appendix F), on the shared
//! [`threnody_groups::node::GroupNode`]: persistence, consent, and delivery
//! through sessions, mailboxes, other members or relays.

use threnody_core::PublicIdentity;
use threnody_core::history::{ConversationId, FileNote};
use threnody_groups::GroupId;
use threnody_groups::node::{Invite, Update};

use threnody_net::history::OutgoingFile;

use crate::{
    FileOptions, HistoryEntry, NodeEvent, Result, ThrenodyNode, fail, fp, history_entries,
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

fn hex(g: &GroupId) -> String {
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
        Update::Reacted { group, from } => NodeEvent::Reacted {
            peer: fp(&from),
            group: Some(hex(&group)),
        },
        Update::Text {
            group,
            from,
            text,
            ours,
        } => NodeEvent::GroupMessage {
            group: hex(&group),
            from: fp(&from),
            text,
            ours,
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

    fn queue_updates(&self, updates: Vec<Update>) {
        self.queued().extend(updates.into_iter().map(event));
    }

    /// A group (or invitation) by hex id prefix.
    fn group_id(&self, id: &str) -> Result<GroupId> {
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

    /// The last `limit` messages in a group (oldest first).
    pub fn group_history(&self, group: String, limit: u32) -> Result<Vec<HistoryEntry>> {
        let g = self.group_id(&group)?;
        let h = self.node.history(ConversationId::Group(g)).map_err(fail)?;
        let me = self.node.reactor_of(&self.node.identity());
        Ok(history_entries(h.recent(limit as usize), me))
    }
}
