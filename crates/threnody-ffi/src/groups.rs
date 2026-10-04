//! MLS groups for apps (Appendix F). The node only carries group payloads;
//! like the CLI, the app side owns the [`Groups`] engine, persists it, and
//! fans its output out over sessions, mailboxes or relays.

use std::collections::VecDeque;

use threnody_core::history::ConversationId;
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity};
use threnody_groups::{GroupError, GroupEvent, GroupId, GroupWire, Groups, Output};
use threnody_net::Node;

use crate::{HistoryEntry, NodeEvent, Result, ThrenodyNode, fail, fp, history_entries};

const STATE: &str = "groups";

/// A group this device belongs to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupInfo {
    /// Hex group id, used to address the group in every call.
    pub id: String,
    pub name: String,
    pub owner: String,
    /// Member device fingerprints, including ours.
    pub members: Vec<String>,
    /// Whether we own it (only the owner adds and removes members).
    pub owned: bool,
}

/// An invitation waiting for the user's consent.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupInvite {
    pub group: String,
    pub name: String,
    pub from: String,
}

pub(crate) struct GroupState {
    groups: Groups,
    /// Invitations from contacts we haven't mutually approved. Kept in
    /// memory only: after a restart the owner invites again.
    pending: Vec<(GroupId, String, PublicIdentity)>,
    home: Home,
    identity: Identity,
}

impl GroupState {
    /// Loads saved groups; call before the node takes `home` and `identity`.
    pub(crate) fn load(home: &Home, identity: &Identity) -> Result<Self> {
        let groups = match home.load_state(identity, STATE).map_err(fail)? {
            Some(b) => Groups::restore(identity, &b).map_err(fail)?,
            None => Groups::new(identity),
        };
        Ok(Self {
            groups,
            pending: Vec::new(),
            home: Home::new(home.dir()),
            identity: Identity::from_seed(&identity.seed()),
        })
    }

    fn save(&self) {
        if let Ok(b) = self.groups.export() {
            let _ = self.home.save_state(&self.identity, STATE, &b);
        }
    }

    /// Persists, sends `out` and turns its events into app events.
    fn apply(&mut self, node: &Node, out: Output, events: &mut VecDeque<NodeEvent>) {
        // Persist before anything leaves: a crash must not lose an epoch
        // that peers have already moved to.
        self.save();
        for o in out.send {
            let Ok(bytes) = o.wire.encode() else { continue };
            let msg = AppMessage::Group(bytes);
            if node.send(&o.to, msg.clone()).is_ok() {
                continue;
            }
            // No live session: seal it for the member's mailboxes, else
            // try to reach the member through relays.
            if node.can_send_offline(&o.to) && node.send_offline(&o.to, &msg).is_ok() {
                continue;
            }
            let (node, to) = (node.clone(), o.to);
            tokio::spawn(async move {
                let _ = node.connect_relayed(to.fingerprint()).await.is_ok()
                    && node.send(&to, msg).is_ok();
            });
        }
        for e in out.events {
            self.event(node, e, events);
        }
    }

    fn event(&mut self, node: &Node, e: GroupEvent, events: &mut VecDeque<NodeEvent>) {
        events.push_back(match e {
            GroupEvent::Joined { group, name, owner } => NodeEvent::GroupJoined {
                group: hex(&group),
                name,
                owner: fp(&owner),
            },
            GroupEvent::InviteRequested { group, name, peer } => {
                // Mutually approved contacts are trusted to add us; anyone
                // else needs explicit consent.
                if node
                    .contacts()
                    .get(&peer)
                    .is_some_and(|c| c.mutually_approved())
                    && let Ok(out) = self.groups.accept_invite(&group, peer)
                {
                    self.apply(node, out, events);
                    return;
                }
                self.pending
                    .retain(|(g, _, p)| !(*g == group && *p == peer));
                self.pending.push((group, name.clone(), peer));
                NodeEvent::GroupInvited {
                    group: hex(&group),
                    name,
                    from: fp(&peer),
                }
            }
            GroupEvent::MemberAdded { group, member } => NodeEvent::GroupMembersChanged {
                group: hex(&group),
                added: vec![fp(&member)],
                removed: vec![],
            },
            GroupEvent::MemberRemoved { group, member } => NodeEvent::GroupMembersChanged {
                group: hex(&group),
                added: vec![],
                removed: vec![fp(&member)],
            },
            GroupEvent::Left { group } => NodeEvent::GroupLeft { group: hex(&group) },
            GroupEvent::Text { group, from, text } => {
                node.record(
                    ConversationId::Group(group),
                    *from.as_bytes(),
                    false,
                    &text,
                    false,
                    None,
                );
                NodeEvent::GroupMessage {
                    group: hex(&group),
                    from: fp(&from),
                    text,
                }
            }
        });
    }
}

fn hex(g: &GroupId) -> String {
    g.iter().map(|b| format!("{b:02x}")).collect()
}

impl ThrenodyNode {
    /// Handles an incoming group payload, queueing the resulting events.
    pub(crate) fn group_incoming(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(w) = GroupWire::decode(payload) else {
            return;
        };
        let _guard = self.rt.enter();
        let mut st = self.group_state();
        let mut q = self.queued();
        match st.groups.handle(from, w) {
            Ok(out) => st.apply(&self.node, out, &mut q),
            Err(e) => q.push_back(NodeEvent::Other {
                description: format!("group message from {}: {e}", fp(&from)),
            }),
        }
    }

    fn group_id(&self, id: &str) -> Result<GroupId> {
        let st = self.group_state();
        let mut hits = st
            .groups
            .list()
            .into_iter()
            .map(|(g, ..)| g)
            .chain(st.pending.iter().map(|(g, ..)| *g))
            .filter(|g| hex(g).starts_with(&id.to_ascii_lowercase()) && id.len() >= 2);
        match (hits.next(), hits.next()) {
            (Some(g), None) => Ok(g),
            (Some(a), Some(b)) if a == b => Ok(a),
            (None, _) => Err(fail(format!("no group {id}"))),
            _ => Err(fail(format!("{id} matches several groups"))),
        }
    }

    /// Runs a group operation and sends its output.
    fn group_op(
        &self,
        f: impl FnOnce(&mut Groups) -> std::result::Result<Output, GroupError>,
    ) -> Result<()> {
        let _guard = self.rt.enter();
        let mut st = self.group_state();
        let out = f(&mut st.groups).map_err(fail)?;
        let mut q = self.queued();
        st.apply(&self.node, out, &mut q);
        Ok(())
    }

    /// Every device of `peer`'s account except ours.
    fn devices_of(&self, peer: &str) -> Result<Vec<PublicIdentity>> {
        let p = self.resolve(peer)?;
        let me = self.node.identity();
        Ok(self
            .node
            .account_of(&p)
            .map(|a| {
                a.state()
                    .devices
                    .iter()
                    .map(|(d, _)| *d)
                    .filter(|d| *d != me)
                    .collect()
            })
            .unwrap_or_else(|| vec![p]))
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Creates a group that we own; returns its id.
    pub fn create_group(&self, name: String) -> Result<String> {
        let mut st = self.group_state();
        let g = st.groups.create(&name).map_err(fail)?;
        st.save();
        Ok(hex(&g))
    }

    pub fn groups(&self) -> Vec<GroupInfo> {
        let me = self.node.identity();
        self.group_state()
            .groups
            .list()
            .into_iter()
            .map(|(g, name, owner, members)| GroupInfo {
                id: hex(&g),
                name,
                owner: fp(&owner),
                members: members.iter().map(fp).collect(),
                owned: owner == me,
            })
            .collect()
    }

    /// Invitations waiting for `accept_group_invite` or `decline_group_invite`.
    pub fn group_invites(&self) -> Vec<GroupInvite> {
        self.group_state()
            .pending
            .iter()
            .map(|(g, name, p)| GroupInvite {
                group: hex(g),
                name: name.clone(),
                from: fp(p),
            })
            .collect()
    }

    /// Invites every device of `peer`'s account (we must own the group).
    /// They join once they accept (automatically if they approved us).
    pub fn invite_to_group(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let devices = self.devices_of(&peer)?;
        let _guard = self.rt.enter();
        let mut st = self.group_state();
        let mut q = self.queued();
        let mut invited = 0;
        for d in devices {
            match st.groups.invite(&g, d) {
                Ok(out) => {
                    invited += 1;
                    st.apply(&self.node, out, &mut q);
                }
                Err(GroupError::AlreadyMember(_)) => {}
                Err(e) => return Err(fail(e)),
            }
        }
        if invited == 0 {
            return Err(fail("every device of that contact is already in the group"));
        }
        Ok(())
    }

    pub fn accept_group_invite(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let peer = {
            let mut st = self.group_state();
            let i = st
                .pending
                .iter()
                .position(|(x, ..)| *x == g)
                .ok_or_else(|| fail("no invitation to that group"))?;
            st.pending.remove(i).2
        };
        self.group_op(|gs| gs.accept_invite(&g, peer))
    }

    pub fn decline_group_invite(&self, group: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_state().pending.retain(|(x, ..)| *x != g);
        Ok(())
    }

    /// Removes every device of `peer`'s account (we must own the group).
    pub fn remove_from_group(&self, group: String, peer: String) -> Result<()> {
        let g = self.group_id(&group)?;
        let members = self
            .groups()
            .into_iter()
            .find(|i| i.id == hex(&g))
            .map(|i| i.members)
            .unwrap_or_default();
        let devices: Vec<PublicIdentity> = self
            .devices_of(&peer)?
            .into_iter()
            .filter(|d| members.contains(&fp(d)))
            .collect();
        if devices.is_empty() {
            return Err(fail("that contact isn't in the group"));
        }
        for d in devices {
            self.group_op(|gs| gs.remove(&g, &d))?;
        }
        Ok(())
    }

    /// Sends text to every other member; recorded in the group's history.
    pub fn send_group_text(&self, group: String, text: String) -> Result<()> {
        let g = self.group_id(&group)?;
        self.group_op(|gs| gs.send_text(&g, &text))?;
        self.node.record(
            ConversationId::Group(g),
            *self.node.identity().as_bytes(),
            true,
            &text,
            false,
            None,
        );
        Ok(())
    }

    /// The last `limit` messages in a group (oldest first).
    pub fn group_history(&self, group: String, limit: u32) -> Result<Vec<HistoryEntry>> {
        let g = self.group_id(&group)?;
        let h = self.node.history(ConversationId::Group(g)).map_err(fail)?;
        Ok(history_entries(h.recent(limit as usize)))
    }
}
