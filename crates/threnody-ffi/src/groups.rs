//! MLS groups for apps (Appendix F), on the shared
//! [`threnody_groups::node::GroupNode`]: persistence, consent, and delivery
//! through sessions, mailboxes, other members or relays.

use threnody_core::PublicIdentity;
use threnody_core::history::{ConversationId, FileNote};
use threnody_core::message::Mention;
use threnody_groups::node::{Banner, Invite, MemberAction, MemberRow, Update};
use threnody_groups::{GroupError, GroupId};

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
    /// The coordinating device: the owner device that applies changes.
    pub owner: String,
    /// Member device fingerprints, including ours.
    pub members: Vec<String>,
    /// Member roles corresponding to `members`: 0=Owner, 1=Admin, 2=Member
    pub member_roles: Vec<u8>,
    /// Whether we're an owner (owners and admins add and remove members;
    /// only owners change roles or delete the group).
    pub owned: bool,
    /// The owner's devices, the admins and the successor's devices.
    pub owners: Vec<String>,
    pub admins: Vec<String>,
    pub successors: Vec<String>,
    /// Same as `owner`.
    pub lead: String,
    pub my_role: MemberRole,
    /// Our changes waiting for the coordinating device.
    pub pending_changes: u32,
    /// Members whose version doesn't support roles yet.
    pub outdated: Vec<String>,
    /// We're the successor and the owner has been silent long enough.
    pub can_claim: bool,
    /// The group's roles come from a newer version: update Threnody.
    pub update_needed: bool,
    /// We asked to be brought back in step and haven't been yet.
    pub out_of_step: bool,
    /// The one banner to show over the chat, if any.
    pub banner: Option<GroupBanner>,
}

/// The one banner a group shows, most pressing first.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum GroupBanner {
    /// "This device fell out of step with the group. Asking the others to
    /// bring it back…"
    OutOfStep,
    /// "<owner> hasn't been seen in this group for 14 days. You're next in
    /// line." with Take over as owner (`claim_group_ownership`).
    CanClaim { owner: String },
    /// "N changes are waiting for <coordinator> to come online." On an
    /// owner device, also Apply from this device (`take_over_group`).
    PendingChanges { count: u32, coordinator: String },
    /// "Admins can be added once <names> update Threnody."
    UpgradePending { outdated: Vec<String> },
    /// "Only you are in this group."
    OnlyYou,
}

/// What can be done to one row of `group_member_rows`, each matching one
/// method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum GroupMemberAction {
    /// `promote_in_group`
    MakeAdmin,
    /// `demote_in_group`
    RemoveAdmin,
    /// `set_group_successor` with the row
    MakeSuccessor,
    /// `set_group_successor` with none
    NoLongerSuccessor,
    /// `transfer_group_ownership`
    MakeOwner,
    /// `use_as_coordinator` (this device)
    UseAsCoordinator,
    /// `remove_from_group`
    Remove,
    /// `remove_lost_coordinator`
    RemoveThisDevice,
    /// `demote_in_group` with ourselves
    StepDown,
}

/// One row of a group's members: an account, or a device we can't tie
/// to one.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupMemberRow {
    /// The device that stands for the row (pass it to the methods).
    pub fingerprint: String,
    pub devices: Vec<String>,
    pub role: MemberRole,
    pub successor: bool,
    pub is_me: bool,
    pub coordinator: bool,
    pub actions: Vec<GroupMemberAction>,
}

fn role(r: threnody_groups::MemberRole) -> MemberRole {
    match r {
        threnody_groups::MemberRole::Owner => MemberRole::Owner,
        threnody_groups::MemberRole::Admin => MemberRole::Admin,
        threnody_groups::MemberRole::Member => MemberRole::Member,
    }
}

fn action(a: MemberAction) -> GroupMemberAction {
    match a {
        MemberAction::MakeAdmin => GroupMemberAction::MakeAdmin,
        MemberAction::RemoveAdmin => GroupMemberAction::RemoveAdmin,
        MemberAction::MakeSuccessor => GroupMemberAction::MakeSuccessor,
        MemberAction::NoLongerSuccessor => GroupMemberAction::NoLongerSuccessor,
        MemberAction::MakeOwner => GroupMemberAction::MakeOwner,
        MemberAction::UseAsCoordinator => GroupMemberAction::UseAsCoordinator,
        MemberAction::Remove => GroupMemberAction::Remove,
        MemberAction::RemoveThisDevice => GroupMemberAction::RemoveThisDevice,
        MemberAction::StepDown => GroupMemberAction::StepDown,
    }
}

fn member_row(r: MemberRow) -> GroupMemberRow {
    GroupMemberRow {
        fingerprint: fp(&r.device),
        devices: r.devices.iter().map(fp).collect(),
        role: role(r.role),
        successor: r.successor,
        is_me: r.me,
        coordinator: r.coordinator,
        actions: r.actions.into_iter().map(action).collect(),
    }
}

fn group_banner(b: Banner) -> GroupBanner {
    match b {
        Banner::OutOfStep => GroupBanner::OutOfStep,
        Banner::CanClaim { owner } => GroupBanner::CanClaim { owner: fp(&owner) },
        Banner::PendingChanges { count, coordinator } => GroupBanner::PendingChanges {
            count,
            coordinator: fp(&coordinator),
        },
        Banner::UpgradePending { outdated } => GroupBanner::UpgradePending {
            outdated: outdated.iter().map(fp).collect(),
        },
        Banner::OnlyYou => GroupBanner::OnlyYou,
    }
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
        // Routed to the call's media before this (`split_legs`).
        Update::CallLeg { call, .. } => NodeEvent::Other {
            description: format!("group call {call:x}: leg signalling"),
        },
        Update::Joined { group, name, owner } => NodeEvent::GroupJoined {
            group: hex(&group),
            name,
            owner: fp(&owner),
        },
        Update::Invited { invite: i, quiet } => {
            let i = invite(&i);
            NodeEvent::GroupInvited {
                group: i.group,
                name: i.name,
                from: i.from,
                quiet,
            }
        }
        Update::MemberAdded { group, member, by } => NodeEvent::GroupMembersChanged {
            group: hex(&group),
            added: vec![fp(&member)],
            removed: vec![],
            by: by.as_ref().map(fp),
        },
        Update::MemberRemoved { group, member, by } => NodeEvent::GroupMembersChanged {
            group: hex(&group),
            added: vec![],
            removed: vec![fp(&member)],
            by: by.as_ref().map(fp),
        },
        Update::ChangeFailed { group, reason } => NodeEvent::GroupChangeFailed {
            group: hex(&group),
            reason,
        },
        Update::UpgradePending { group, outdated } => NodeEvent::GroupUpgradePending {
            group: hex(&group),
            outdated: outdated.iter().map(fp).collect(),
        },
        Update::OutOfStep { group } => NodeEvent::GroupOutOfStep { group: hex(&group) },
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
        let r = r.map(|u| self.split_legs(u));
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

    /// Devices left their accounts: they lose their roles and are removed
    /// from our groups (by us if we coordinate, proposed if we may, taken
    /// over from if one coordinated for our account), and none of our
    /// groups delivers to them any more.
    pub(crate) fn group_revoked(&self, removed: &[PublicIdentity]) {
        let _guard = self.rt.enter();
        let updates = self.group_node().revoked(&self.node, removed);
        self.queue_updates(updates);
    }

    pub(crate) fn queue_group_updates(&self, updates: Vec<Update>) {
        self.queue_updates(updates);
    }

    fn queue_updates(&self, updates: Vec<Update>) {
        let updates = self.split_legs(updates);
        self.queued().extend(updates.into_iter().map(event));
    }

    /// Hands group call leg signalling to the call's media (never the app
    /// or its logs) and returns the rest.
    fn split_legs(&self, updates: Vec<Update>) -> Vec<Update> {
        updates
            .into_iter()
            .filter_map(|u| match u {
                Update::CallLeg { call, from, data } => {
                    self.group_leg_signal(call, from, data);
                    None
                }
                u => Some(u),
            })
            .collect()
    }

    /// `peer` as a member of group `g` (by fingerprint), or else as a
    /// contact: members needn't be contacts.
    fn member(&self, g: &GroupId, peer: &str) -> Result<PublicIdentity> {
        let found = self
            .group_node()
            .list()
            .into_iter()
            .find(|(id, ..)| id == g)
            .and_then(|(.., members)| members.into_iter().find(|m| fp(m) == peer));
        match found {
            Some(p) => Ok(p),
            None => self.resolve(peer),
        }
    }

    fn group_info(
        &self,
        g: GroupId,
        name: String,
        owner: PublicIdentity,
        members: Vec<PublicIdentity>,
        roles: Vec<threnody_groups::MemberRole>,
    ) -> GroupInfo {
        let me = self.node.identity();
        let gn = self.group_node();
        let now = threnody_core::now_ms();
        let roster = gn
            .roster(&g)
            .unwrap_or_else(|_| threnody_groups::Roster::legacy(owner));
        let my_role = roster.role(&me);
        GroupInfo {
            id: hex(&g),
            name,
            owner: fp(&owner),
            members: members.iter().map(fp).collect(),
            member_roles: roles.iter().map(|r| r.to_wire()).collect(),
            owned: my_role == threnody_groups::MemberRole::Owner,
            owners: roster.owners.iter().map(fp).collect(),
            admins: roster.admins.iter().map(fp).collect(),
            successors: roster.successors.iter().map(fp).collect(),
            lead: fp(&roster.lead),
            my_role: role(my_role),
            pending_changes: u32::try_from(gn.pending_changes(&g)).unwrap_or(u32::MAX),
            outdated: gn.outdated(&g).iter().map(fp).collect(),
            can_claim: gn.can_claim(&self.node, &g, now),
            update_needed: roster.update_needed(),
            out_of_step: gn.out_of_step(&g),
            banner: gn.banner(&self.node, &g, now).map(group_banner),
        }
    }

    /// Runs a group change and queues what it brings.
    fn group_change(
        &self,
        f: impl FnOnce(
            &mut threnody_groups::node::GroupNode,
        ) -> std::result::Result<Vec<Update>, GroupError>,
    ) -> Result<()> {
        let _guard = self.rt.enter();
        let updates = f(&mut self.group_node()).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// The mentions an app names (by member device) in group `g`, as a
    /// message carries them; those naming no member are left out.
    pub(crate) fn group_mentions(&self, g: &GroupId, mentions: &[MentionInfo]) -> Vec<Mention> {
        let members = self
            .group_node()
            .list()
            .into_iter()
            .find(|(id, ..)| id == g)
            .map(|(.., m)| m)
            .unwrap_or_default();
        mentions
            .iter()
            .filter_map(|m| {
                let d = members.iter().find(|d| fp(d) == m.device)?;
                Some(Mention {
                    start: m.start,
                    len: m.len,
                    who: self.node.reactor_of(d),
                })
            })
            .collect()
    }

    /// Whether we're a member of group `g`.
    pub(crate) fn in_group(&self, g: &GroupId) -> bool {
        self.group_node()
            .list()
            .into_iter()
            .any(|(id, ..)| id == *g)
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
        let list = self.group_node().list_with_roles();
        list.into_iter()
            .map(|(g, name, owner, members, roles)| self.group_info(g, name, owner, members, roles))
            .collect()
    }

    /// One row per account of `group`'s members (a device we can't tie to
    /// an account is its own row): owners, then admins, then members, with
    /// what this device may do to each.
    pub fn group_member_rows(&self, group: String) -> Result<Vec<GroupMemberRow>> {
        let g = self.group_id(&group)?;
        let rows = self
            .group_node()
            .member_rows(&self.node, &g)
            .map_err(fail)?;
        Ok(rows.into_iter().map(member_row).collect())
    }

    /// Invitations waiting for `accept_group_invite` or `decline_group_invite`.
    pub fn group_invites(&self) -> Vec<GroupInvite> {
        self.group_node().invites().iter().map(invite).collect()
    }

    /// Invites every device of `peer`'s account (owners and admins may).
    /// They join once they accept (automatically if they approved us); an
    /// admin's invitation completes once the coordinating device applies it.
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

    /// Accepts `from`'s invitation to `group` (both as `group_invites`
    /// lists them), when more than one person invites us to that group.
    pub fn accept_group_invite_from(&self, group: String, from: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let inviter = self
            .group_node()
            .invites()
            .iter()
            .find(|i| i.group == g && fp(&i.from) == from)
            .map(|i| i.from)
            .ok_or_else(|| fail("no such invitation"))?;
        let _guard = self.rt.enter();
        let updates = self
            .group_node()
            .accept_from(&self.node, &g, &inviter)
            .map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    pub fn decline_group_invite(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_node().decline(&g);
        Ok(())
    }

    /// Declines `from`'s invitation to `group` (both as `group_invites`
    /// lists them), keeping anyone else's to the same group.
    pub fn decline_group_invite_from(&self, group: String, from: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let mut groups = self.group_node();
        let inviter = groups
            .invites()
            .iter()
            .find(|i| i.group == g && fp(&i.from) == from)
            .map(|i| i.from)
            .ok_or_else(|| fail("no such invitation"))?;
        groups.decline_from(&g, &inviter);
        Ok(())
    }

    /// Removes every device of `peer`'s account (owners may remove anyone
    /// but the owner; admins, members only).
    pub fn remove_from_group(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = self.member(&g, &peer)?;
        let _guard = self.rt.enter();
        let updates = self.group_node().remove(&self.node, &g, &p).map_err(fail)?;
        self.queue_updates(updates);
        Ok(())
    }

    /// Owner: makes every device of `peer`'s account in the group an
    /// admin (members see who the owner's and that account's devices are).
    pub fn promote_in_group(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = self.member(&g, &peer)?;
        self.group_change(|gn| gn.promote(&self.node, &g, p))
    }

    /// Owner: `peer`'s account is no longer an admin. With our own
    /// fingerprint, an admin steps down.
    pub fn demote_in_group(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = self.member(&g, &peer)?;
        self.group_change(|gn| gn.demote(&self.node, &g, p))
    }

    /// Owner: hands the group to `peer`'s account; our devices become
    /// admins. Can't be undone by us.
    pub fn transfer_group_ownership(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = self.member(&g, &peer)?;
        self.group_change(|gn| gn.transfer(&self.node, &g, p))
    }

    /// Owner: hands the group to `peer`'s account and leaves it ("Choose a
    /// new owner first"). Fails, changing nothing, while the group waits
    /// for members to update.
    pub fn transfer_group_ownership_and_leave(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = self.member(&g, &peer)?;
        self.group_change(|gn| gn.transfer_and_leave(&self.node, &g, p))
    }

    /// Owner: makes `peer`'s account (an admin) the successor, who may take
    /// over should the owner be silent for 14 days; `None` clears it.
    pub fn set_group_successor(&self, group: String, peer: Option<String>) -> Result<()> {
        let g = self.group_id(&group)?;
        let p = peer.map(|p| self.member(&g, &p)).transpose()?;
        self.group_change(|gn| gn.set_successor(&self.node, &g, p))
    }

    /// The successor takes ownership (offered when `GroupInfo::can_claim`).
    /// The old owner becomes an admin; it can't be undone.
    pub fn claim_group_ownership(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_change(|gn| gn.claim(&self.node, &g))
    }

    /// An owner device applies waiting changes itself, taking over as the
    /// coordinating device without the current one (the banner's Apply
    /// from this device).
    pub fn take_over_group(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_change(|gn| gn.take_over(&self.node, &g))
    }

    /// An owner device asks for `device` (an owner device, usually this
    /// one) to coordinate from now on.
    pub fn use_as_coordinator(&self, group: String, device: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let d = self.member(&g, &device)?;
        self.group_change(|gn| gn.use_as_coordinator(&self.node, &g, d))
    }

    /// Takes over from a lost coordinating device of ours and removes it
    /// from the group.
    pub fn remove_lost_coordinator(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_change(|gn| gn.remove_coordinator(&self.node, &g))
    }

    /// Owner: deletes the group for everyone. Its history stays here.
    pub fn delete_group(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_change(|gn| gn.delete(&self.node, &g))
    }

    /// Debug builds only (a no-op in release builds): the owner's silence
    /// before a successor may take over, in ms (0: the usual 14 days).
    pub fn set_succession_silence_for_testing(&self, ms: u64) {
        #[cfg(debug_assertions)]
        threnody_groups::node::set_succession_silence_for_testing(ms);
        #[cfg(not(debug_assertions))]
        let _ = ms;
    }

    /// Leaves a group: the coordinating device removes us. The last owner
    /// account must transfer ownership (`transfer_group_ownership`) or
    /// delete the group (`delete_group`) instead, unless nobody else is in
    /// it. Its history stays.
    pub fn leave_group(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_change(|gn| gn.leave(&self.node, &g))?;
        // Leaving is our choice, like deleting a chat: what we scheduled
        // there goes (being removed fails it when due, so we hear of it).
        self.node.drop_scheduled(ConversationId::Group(g));
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
        let mentions = self.group_mentions(&g, &mentions);
        let _guard = self.rt.enter();
        self.group_node()
            .send_message(&self.node, &g, &text, quote, mentions)
            .map_err(fail)
    }

    /// Whether we may pin messages in `group` (we're an owner or admin, as
    /// every member agrees).
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
