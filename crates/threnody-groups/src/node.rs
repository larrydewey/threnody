//! Groups on a running [`Node`]: persistence, consent and delivery, shared
//! by the CLI and the app bindings (feature `node`).
//!
//! Every outgoing group message goes to its member by the first of these
//! that works (Appendix F, "Delivery"):
//!
//! 1. the live session with that member;
//! 2. sealed for the member's mailboxes (Appendix H);
//! 3. forwarded by another member we have a session with, preferring the
//!    owner (`GroupWire::Forward`), for MLS messages only;
//! 4. held here, and sent when the member connects; meanwhile we try to
//!    reach them through relays (Appendix G).
//!
//! A forwarder delivers the same way but never forwards again, so a
//! message crosses at most one forwarding member.

use threnody_core::history::{ConversationId, Entry};
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity};
use threnody_net::{Node, Tag};

use crate::{GroupError, GroupEvent, GroupId, GroupWire, Groups, Output};

type Result<T> = std::result::Result<T, GroupError>;

const STATE: &str = "groups";
const INVITES: &str = "group-invites";
const HELD: &str = "group-held";
const VERSION: u8 = 1;
const HELD_VERSION: u8 = 2;
/// Messages held for one member; the oldest are dropped first. Messages
/// from more than a few epochs back can't be read anyway.
pub const MAX_HELD_PER_MEMBER: usize = 200;

/// An invitation waiting for the user's consent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub group: GroupId,
    pub name: String,
    pub from: PublicIdentity,
}

/// What the user should see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    /// We joined (after consenting, or automatically for an invitation
    /// from a mutually approved contact).
    Joined {
        group: GroupId,
        name: String,
        owner: PublicIdentity,
    },
    /// Someone who isn't a mutually approved contact invites us.
    Invited(Invite),
    MemberAdded {
        group: GroupId,
        member: PublicIdentity,
    },
    MemberRemoved {
        group: GroupId,
        member: PublicIdentity,
    },
    /// The owner removed us.
    Left { group: GroupId },
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
    },
}

pub struct GroupNode {
    groups: Groups,
    invites: Vec<Invite>,
    /// Encoded `GroupWire::Message`s waiting for their member to connect,
    /// with the local id of the history entry they belong to (0 = none).
    held: Vec<(PublicIdentity, Vec<u8>, u64)>,
    home: Home,
    identity: Identity,
}

impl GroupNode {
    /// Loads saved groups, invitations and held messages. Call it before
    /// `Node::new` takes `home` and `identity`.
    pub fn load(home: &Home, identity: &Identity) -> Result<Self> {
        let groups = match home.load_state(identity, STATE)? {
            Some(b) => Groups::restore(identity, &b)?,
            None => Groups::new(identity),
        };
        let load = |name| {
            home.load_state(identity, name)
                .ok()
                .flatten()
                .unwrap_or_default()
        };
        Ok(Self {
            groups,
            invites: decode_invites(&load(INVITES)),
            held: decode_held(&load(HELD)),
            home: Home::new(home.dir()),
            identity: Identity::from_seed(&identity.seed()),
        })
    }

    /// `(id, name, owner, members)` for every group we are in.
    pub fn list(&self) -> Vec<(GroupId, String, PublicIdentity, Vec<PublicIdentity>)> {
        self.groups.list()
    }

    pub fn invites(&self) -> &[Invite] {
        &self.invites
    }

    /// How many messages are waiting for `member` to connect.
    pub fn held_for(&self, member: &PublicIdentity) -> usize {
        self.held.iter().filter(|(p, ..)| p == member).count()
    }

    /// Creates a group that we own.
    pub fn create(&mut self, name: &str) -> Result<GroupId> {
        let g = self.groups.create(name)?;
        self.save()?;
        Ok(g)
    }

    /// Invites every device of `peer`'s account (we must own the group).
    /// Returns how many devices were invited.
    pub fn invite(&mut self, node: &Node, group: &GroupId, peer: &PublicIdentity) -> Result<usize> {
        let mut invited = 0;
        for d in account_devices(node, peer) {
            match self.groups.invite(group, d) {
                Ok(out) => {
                    invited += 1;
                    self.apply(node, out, true);
                }
                Err(GroupError::AlreadyMember(_)) => {}
                Err(e) => return Err(e),
            }
        }
        if invited == 0 {
            return Err(GroupError::AlreadyMember(peer.fingerprint().to_string()));
        }
        Ok(invited)
    }

    /// Accepts the invitation to `group`.
    pub fn accept(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let i = self
            .invites
            .iter()
            .position(|i| i.group == *group)
            .ok_or(GroupError::UnknownGroup)?;
        let inv = self.invites.remove(i);
        self.save_invites();
        let out = self.groups.accept_invite(&inv.group, inv.from)?;
        Ok(self.apply(node, out, true))
    }

    pub fn decline(&mut self, group: &GroupId) {
        self.invites.retain(|i| i.group != *group);
        self.save_invites();
    }

    /// Removes every device of `peer`'s account that is in the group (we
    /// must own it).
    pub fn remove(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: &PublicIdentity,
    ) -> Result<Vec<Update>> {
        let members = self.members(group)?;
        let devices: Vec<PublicIdentity> = account_devices(node, peer)
            .into_iter()
            .filter(|d| members.contains(d))
            .collect();
        if devices.is_empty() {
            return Err(GroupError::NotMember(peer.fingerprint().to_string()));
        }
        let mut updates = Vec::new();
        for d in devices {
            let out = self.groups.remove(group, &d)?;
            updates.extend(self.apply(node, out, true));
        }
        Ok(updates)
    }

    /// Sends text to every other member and records it in the group's
    /// history, to be marked as members acknowledge their copies.
    pub fn send_text(&mut self, node: &Node, group: &GroupId, text: &str) -> Result<()> {
        let recipients = self.members(group)?.len().saturating_sub(1);
        let out = self.groups.send_text(group, text)?;
        let local_id = threnody_net::history::local_id();
        self.apply_tagged(node, out, true, local_id);
        let now = threnody_core::now_ms();
        node.append(
            ConversationId::Group(*group),
            Entry {
                at_ms: now,
                outgoing: true,
                device: *node.identity().as_bytes(),
                text: text.to_owned(),
                offline: false,
                expires_at_ms: None,
                file: None,
                local_id,
                delivered: recipients == 0,
                recipients: u32::try_from(recipients).unwrap_or(u32::MAX),
                delivered_to: Vec::new(),
            },
        );
        Ok(())
    }

    /// Handles an `AppMessage::Group` payload from `from`.
    pub fn incoming(
        &mut self,
        node: &Node,
        from: PublicIdentity,
        payload: &[u8],
    ) -> Result<Vec<Update>> {
        let wire = GroupWire::decode(payload)?;
        // What a forward yields is delivered, but not forwarded again.
        let may_forward = !matches!(wire, GroupWire::Forward { .. });
        let out = self.groups.handle(from, wire)?;
        Ok(self.apply(node, out, may_forward))
    }

    /// Sends what was held for `peer`, who just connected.
    pub fn connected(&mut self, node: &Node, peer: &PublicIdentity) {
        if !self.held.iter().any(|(p, ..)| p == peer) {
            return;
        }
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(p, ..)| p == peer);
        self.held = rest;
        for (p, bytes, local_id) in mine {
            let tag = GroupWire::decode(&bytes)
                .map(|w| tag(local_id, w.group()))
                .unwrap_or(Tag::NONE);
            if node
                .send_tagged(&p, AppMessage::Group(bytes.clone()), tag)
                .is_err()
            {
                self.held.push((p, bytes, local_id));
            }
        }
        self.save_held();
    }

    fn members(&self, group: &GroupId) -> Result<Vec<PublicIdentity>> {
        self.groups
            .list()
            .into_iter()
            .find(|(g, ..)| g == group)
            .map(|(.., m)| m)
            .ok_or(GroupError::UnknownGroup)
    }

    fn save(&self) -> Result<()> {
        self.home
            .save_state(&self.identity, STATE, &self.groups.export()?)?;
        Ok(())
    }

    fn save_invites(&self) {
        let _ = self
            .home
            .save_state(&self.identity, INVITES, &encode_invites(&self.invites));
    }

    fn save_held(&self) {
        let _ = self
            .home
            .save_state(&self.identity, HELD, &encode_held(&self.held));
    }

    /// Persists, delivers `out`, and turns its events into updates.
    fn apply(&mut self, node: &Node, out: Output, may_forward: bool) -> Vec<Update> {
        self.apply_tagged(node, out, may_forward, 0)
    }

    /// [`Self::apply`], with copies tagged for history entry `local_id`.
    fn apply_tagged(
        &mut self,
        node: &Node,
        out: Output,
        may_forward: bool,
        local_id: u64,
    ) -> Vec<Update> {
        // Persist before anything leaves: a crash must not lose an epoch
        // that peers have already moved to.
        let _ = self.save();
        let held = self.held.len();
        for o in out.send {
            self.deliver(node, o.to, o.wire, may_forward, local_id);
        }
        if self.held.len() != held {
            self.save_held();
        }
        let mut updates = Vec::new();
        for e in out.events {
            self.event(node, e, &mut updates);
        }
        updates
    }

    /// Only a copy the member acknowledges itself counts towards the
    /// entry's delivery; mailboxes and forwarders aren't the member.
    fn deliver(
        &mut self,
        node: &Node,
        to: PublicIdentity,
        wire: GroupWire,
        may_forward: bool,
        local_id: u64,
    ) {
        let Ok(bytes) = wire.encode() else { return };
        let msg = AppMessage::Group(bytes.clone());
        if node
            .send_tagged(&to, msg.clone(), tag(local_id, wire.group()))
            .is_ok()
        {
            return;
        }
        if node.can_send_offline(&to) && node.send_offline(&to, &msg).is_ok() {
            return;
        }
        if let (true, GroupWire::Message { group, message }) = (may_forward, &wire)
            && let Some(via) = self.forwarder(node, group, &to)
        {
            let fwd = GroupWire::Forward {
                group: *group,
                to: *to.as_bytes(),
                message: message.clone(),
            };
            if let Ok(b) = fwd.encode()
                && node.send(&via, AppMessage::Group(b)).is_ok()
            {
                return;
            }
        }
        // Hold it for when they connect, and try to make that happen.
        self.held.push((to, bytes, local_id));
        let mine: Vec<usize> = (0..self.held.len())
            .filter(|&i| self.held[i].0 == to)
            .collect();
        if let Some(extra) = mine.len().checked_sub(MAX_HELD_PER_MEMBER) {
            for i in mine.into_iter().take(extra).rev() {
                self.held.remove(i);
            }
        }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let node = node.clone();
            rt.spawn(async move {
                let _ = node.connect_relayed(to.fingerprint()).await;
            });
        }
    }

    /// A member other than `to` with a live session, the owner first.
    fn forwarder(
        &self,
        node: &Node,
        group: &GroupId,
        to: &PublicIdentity,
    ) -> Option<PublicIdentity> {
        let (_, _, owner, members) = self.groups.list().into_iter().find(|(g, ..)| g == group)?;
        let me = node.identity();
        let live: Vec<PublicIdentity> = node.sessions().iter().map(|s| s.peer).collect();
        let candidates = members
            .into_iter()
            .filter(|m| m != to && *m != me && live.contains(m));
        let mut candidates: Vec<_> = candidates.collect();
        candidates.sort_by_key(|m| *m != owner);
        candidates.into_iter().next()
    }

    fn event(&mut self, node: &Node, e: GroupEvent, updates: &mut Vec<Update>) {
        updates.push(match e {
            GroupEvent::Joined { group, name, owner } => Update::Joined { group, name, owner },
            GroupEvent::InviteRequested { group, name, peer } => {
                // Mutually approved contacts are trusted to add us; anyone
                // else needs explicit consent.
                if node
                    .contacts()
                    .get(&peer)
                    .is_some_and(|c| c.mutually_approved())
                    && let Ok(out) = self.groups.accept_invite(&group, peer)
                {
                    updates.extend(self.apply(node, out, true));
                    return;
                }
                let inv = Invite {
                    group,
                    name,
                    from: peer,
                };
                self.invites
                    .retain(|i| !(i.group == group && i.from == peer));
                self.invites.push(inv.clone());
                self.save_invites();
                Update::Invited(inv)
            }
            GroupEvent::MemberAdded { group, member } => Update::MemberAdded { group, member },
            GroupEvent::MemberRemoved { group, member } => Update::MemberRemoved { group, member },
            GroupEvent::Left { group } => Update::Left { group },
            GroupEvent::Text { group, from, text } => {
                node.record(
                    ConversationId::Group(group),
                    *from.as_bytes(),
                    false,
                    &text,
                    false,
                    None,
                );
                Update::Text { group, from, text }
            }
        });
    }
}

fn tag(local_id: u64, group: &GroupId) -> Tag {
    if local_id == 0 {
        Tag::NONE
    } else {
        Tag {
            local_id,
            group: Some(*group),
        }
    }
}

/// Every device of `peer`'s account except ours (just `peer` without one).
fn account_devices(node: &Node, peer: &PublicIdentity) -> Vec<PublicIdentity> {
    let me = node.identity();
    node.account_of(peer)
        .map(|a| {
            a.state()
                .devices
                .iter()
                .map(|(d, _)| *d)
                .filter(|d| *d != me)
                .collect()
        })
        .unwrap_or_else(|| vec![*peer])
}

// Local state, inside encrypted state files:
//   invites = VERSION || * ( group (16) | inviter (32) | name len (u16 LE) | name )
//   held    = HELD_VERSION || * ( member (32) | local id (u64 BE) | len (u32 LE) | GroupWire bytes )

fn encode_invites(invites: &[Invite]) -> Vec<u8> {
    let mut out = vec![VERSION];
    for i in invites {
        let name = &i.name.as_bytes()[..i.name.len().min(usize::from(u16::MAX))];
        out.extend_from_slice(&i.group);
        out.extend_from_slice(i.from.as_bytes());
        out.extend_from_slice(&u16::try_from(name.len()).unwrap_or(u16::MAX).to_le_bytes());
        out.extend_from_slice(name);
    }
    out
}

fn decode_invites(b: &[u8]) -> Vec<Invite> {
    let mut out = Vec::new();
    let Some((&VERSION, mut rest)) = b.split_first() else {
        return out;
    };
    while let Some((head, r)) = rest.split_at_checked(50) {
        let len = usize::from(u16::from_le_bytes([head[48], head[49]]));
        let Some((name, r)) = r.split_at_checked(len) else {
            break;
        };
        rest = r;
        let group: GroupId = head[..16].try_into().unwrap_or_default();
        let from: [u8; 32] = head[16..48].try_into().unwrap_or_default();
        if let Ok(from) = PublicIdentity::from_bytes(&from) {
            out.push(Invite {
                group,
                name: String::from_utf8_lossy(name).into_owned(),
                from,
            });
        }
    }
    out
}

fn encode_held(held: &[(PublicIdentity, Vec<u8>, u64)]) -> Vec<u8> {
    let mut out = vec![HELD_VERSION];
    for (p, b, local_id) in held {
        out.extend_from_slice(p.as_bytes());
        out.extend_from_slice(&local_id.to_be_bytes());
        out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

fn decode_held(b: &[u8]) -> Vec<(PublicIdentity, Vec<u8>, u64)> {
    let mut out = Vec::new();
    let Some((&HELD_VERSION, mut rest)) = b.split_first() else {
        return out;
    };
    while let Some((head, r)) = rest.split_at_checked(44) {
        let len = u32::from_le_bytes([head[40], head[41], head[42], head[43]]) as usize;
        let Some((msg, r)) = r.split_at_checked(len) else {
            break;
        };
        rest = r;
        let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
        let local_id = u64::from_be_bytes(head[32..40].try_into().unwrap_or_default());
        if let Ok(p) = PublicIdentity::from_bytes(&peer) {
            out.push((p, msg.to_vec(), local_id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_state_round_trips_and_tolerates_damage() {
        let p = Identity::generate().public();
        let invites = vec![
            Invite {
                group: [1; 16],
                name: "book club".into(),
                from: p,
            },
            Invite {
                group: [2; 16],
                name: String::new(),
                from: p,
            },
        ];
        let b = encode_invites(&invites);
        assert_eq!(decode_invites(&b), invites);
        assert_eq!(decode_invites(&b[..b.len() - 1]), invites[..1]);
        let held = vec![(p, vec![1, 2, 3], 9), (p, vec![], 0)];
        let b = encode_held(&held);
        assert_eq!(decode_held(&b), held);
        assert_eq!(decode_held(&b[..b.len() - 2]), held[..1]);
        for d in [&[][..], &[9, 0, 0]] {
            assert!(decode_invites(d).is_empty() && decode_held(d).is_empty());
        }
    }
}
