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

use threnody_core::history::{ConversationId, Entry, FileNote};
use threnody_core::message::Clip;
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity};
use threnody_net::history::OutgoingFile;
use threnody_net::{Node, Tag};

use crate::{Content, GroupError, GroupEvent, GroupId, GroupWire, Groups, MemberRole, Output};

type Result<T> = std::result::Result<T, GroupError>;

const STATE: &str = "groups";
const INVITES: &str = "group-invites";
const HELD: &str = "group-held";
const VERSION: u8 = 1;
const HELD_VERSION: u8 = 3;
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
    /// A member's role was changed.
    RoleChanged {
        group: GroupId,
        member: PublicIdentity,
        role: MemberRole,
    },
    /// The owner removed us.
    Left { group: GroupId },
    /// `ours`: sent by another device of our own account (shown as ours,
    /// not notified).
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
        ours: bool,
    },
    /// A file from a member. Save it, then record it with
    /// [`GroupNode::record_file`], passing `id`.
    File {
        group: GroupId,
        from: PublicIdentity,
        name: String,
        data: Vec<u8>,
        ours: bool,
        sensitive: bool,
        caption: String,
        album: u64,
        id: u64,
        clip: Option<Clip>,
    },
    /// `from` changed its reactions on a message in `group`.
    Reacted {
        group: GroupId,
        from: PublicIdentity,
    },
}

pub struct GroupNode {
    groups: Groups,
    invites: Vec<Invite>,
    /// Encoded `GroupWire::Message`s waiting for their member to connect,
    /// with the delivery tag to send them with.
    held: Vec<(PublicIdentity, Vec<u8>, Tag)>,
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

    /// `(id, name, owner, members, roles)` for every group we are in.
    #[allow(clippy::type_complexity)]
    pub fn list_with_roles(
        &self,
    ) -> Vec<(
        GroupId,
        String,
        PublicIdentity,
        Vec<PublicIdentity>,
        Vec<MemberRole>,
    )> {
        self.groups.list_with_roles()
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

    /// Promotes `peer` to Admin (only the group owner can do this).
    pub fn promote(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let members = self.members(group)?;
        if !members.contains(&peer) {
            return Err(GroupError::NotMember(peer.fingerprint().to_string()));
        }
        let out = self.groups.promote(group, peer)?;
        Ok(self.apply(node, out, true))
    }

    /// Demotes `peer` from Admin to Member (only the group owner can do this).
    pub fn demote(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let members = self.members(group)?;
        if !members.contains(&peer) {
            return Err(GroupError::NotMember(peer.fingerprint().to_string()));
        }
        let out = self.groups.demote(group, peer)?;
        Ok(self.apply(node, out, true))
    }

    /// Leaves a group: a member asks the owner to remove it, the owner
    /// removes everyone. Either way the group is gone here at once.
    pub fn leave(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let me = node.identity();
        let owned = self
            .groups
            .list()
            .iter()
            .any(|(g, _, o, _)| g == group && *o == me);
        let out = if owned {
            self.groups.disband(group)?
        } else {
            self.groups.leave(group)?
        };
        // Copies held for this group's members are moot now.
        self.held
            .retain(|(_, b, _)| GroupWire::decode(b).map_or(true, |w| w.group() != group));
        self.save_held();
        Ok(self.apply(node, out, true))
    }

    /// Sends text to every other member and records it in the group's
    /// history, to be marked as members acknowledge their copies.
    pub fn send_text(&mut self, node: &Node, group: &GroupId, text: &str) -> Result<()> {
        let id = threnody_net::history::local_id();
        let content = Content::Text {
            text: text.to_owned(),
            id,
        };
        self.send(node, group, &content, None, id)
    }

    /// Adds (or with `add` false, takes away) our `emoji` on message `id`
    /// in `group`, for every member. Returns false if there's no such
    /// message here.
    pub fn react(
        &mut self,
        node: &Node,
        group: &GroupId,
        id: u64,
        emoji: &str,
        add: bool,
    ) -> Result<bool> {
        let me = node.reactor_of(&node.identity());
        if !node.apply_reaction(ConversationId::Group(*group), me, id, emoji, add) {
            return Ok(false);
        }
        let content = Content::React {
            id,
            emoji: emoji.to_owned(),
            add,
        };
        let out = self.groups.send(group, &content)?;
        self.apply(node, out, true);
        Ok(true)
    }

    /// Sends a file to every other member and records it (with
    /// `file.location`, where it is on this device) in the group's history.
    pub fn send_file(&mut self, node: &Node, group: &GroupId, file: OutgoingFile) -> Result<()> {
        let data = node
            .prepare_file(file.data)
            .map_err(|e| GroupError::File(e.to_string()))?;
        let note = FileNote {
            name: file.name.clone(),
            size: data.len() as u64,
            location: file.location,
            sensitive: file.sensitive,
            album: file.album,
            clip: file.clip,
        };
        let id = threnody_net::history::local_id();
        let content = Content::File {
            name: file.name,
            data,
            sensitive: file.sensitive,
            caption: file.caption,
            album: file.album,
            id,
            clip: file.clip,
        };
        self.send(node, group, &content, Some(note), id)
    }

    /// Sends text or a file, recorded under `local_id` (also its id in
    /// the content, which members' reactions name).
    fn send(
        &mut self,
        node: &Node,
        group: &GroupId,
        content: &Content,
        file: Option<FileNote>,
        local_id: u64,
    ) -> Result<()> {
        let recipients = self.members(group)?.len().saturating_sub(1);
        let out = self.groups.send(group, content)?;
        self.apply_tagged(node, out, true, tag(local_id, group));
        let now = threnody_core::now_ms();
        node.append(
            ConversationId::Group(*group),
            Entry {
                at_ms: now,
                outgoing: true,
                device: *node.identity().as_bytes(),
                text: match content {
                    Content::Text { text: t, .. } | Content::File { caption: t, .. } => t.clone(),
                    Content::React { .. } => String::new(),
                },
                offline: false,
                expires_at_ms: node
                    .effective_timer(ConversationId::Group(*group))
                    .map(|s| now + u64::from(s) * 1000),
                file,
                local_id,
                delivered: recipients == 0,
                recipients: u32::try_from(recipients).unwrap_or(u32::MAX),
                delivered_to: Vec::new(),
                remote_id: 0,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
            },
        );
        Ok(())
    }

    /// Records a file `from` sent to `group` (see [`Update::File`]), once
    /// it has been saved at `file.location`.
    pub fn record_file(
        &self,
        node: &Node,
        group: &GroupId,
        from: &PublicIdentity,
        file: FileNote,
        caption: &str,
        remote_id: u64,
    ) {
        let conv = ConversationId::Group(*group);
        let now = threnody_core::now_ms();
        node.append(
            conv,
            Entry {
                at_ms: now,
                outgoing: node.is_own_device(from),
                device: *from.as_bytes(),
                text: caption.to_owned(),
                offline: false,
                expires_at_ms: node
                    .effective_timer(conv)
                    .map(|s| now + u64::from(s) * 1000),
                file: Some(file),
                local_id: 0,
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
                remote_id,
                edited_ms: 0,
                read_ms: 0,
                reactions: Vec::new(),
            },
        );
    }

    /// Handles an `AppMessage::Group` payload from `from`.
    pub fn incoming(
        &mut self,
        node: &Node,
        from: PublicIdentity,
        payload: &[u8],
    ) -> Result<Vec<Update>> {
        let wire = GroupWire::decode(payload)?;
        match wire {
            GroupWire::Receipt {
                group,
                member,
                reference,
            } => {
                // A forwarder says `member` has our message: believe members
                // only, about members only.
                let members = self.members(&group)?;
                let member = PublicIdentity::from_bytes(&member)?;
                if members.contains(&from) && members.contains(&member) {
                    node.mark_delivered(&member, tag(reference, &group));
                }
                Ok(Vec::new())
            }
            GroupWire::Forward {
                group, reference, ..
            } => {
                // Deliver it (never forwarding again); with a reference, the
                // member's acknowledgement earns the sender a receipt.
                let out = self.groups.handle(from, wire)?;
                let t = if reference == 0 {
                    Tag::NONE
                } else {
                    Tag {
                        local_id: reference,
                        group: Some(group),
                        relay_for: Some(*from.as_bytes()),
                    }
                };
                Ok(self.apply_tagged(node, out, false, t))
            }
            wire => {
                let out = self.groups.handle(from, wire)?;
                Ok(self.apply(node, out, true))
            }
        }
    }

    /// A member acknowledged a message we forwarded for `origin` (from
    /// `Event::Delivered` with `relay_for`): send `origin` a receipt.
    pub fn relayed(
        &mut self,
        node: &Node,
        member: &PublicIdentity,
        group: &GroupId,
        reference: u64,
        origin: &PublicIdentity,
    ) {
        let receipt = GroupWire::Receipt {
            group: *group,
            member: *member.as_bytes(),
            reference,
        };
        let held = self.held.len();
        self.deliver(node, *origin, receipt, false, Tag::NONE);
        if self.held.len() != held {
            self.save_held();
        }
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
        for (p, bytes, t) in mine {
            if node
                .send_tagged(&p, AppMessage::Group(bytes.clone()), t)
                .is_err()
            {
                self.held.push((p, bytes, t));
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
        self.apply_tagged(node, out, may_forward, Tag::NONE)
    }

    /// [`Self::apply`], sending the copies with delivery tag `t`.
    fn apply_tagged(&mut self, node: &Node, out: Output, may_forward: bool, t: Tag) -> Vec<Update> {
        // Persist before anything leaves: a crash must not lose an epoch
        // that peers have already moved to.
        let _ = self.save();
        let held = self.held.len();
        for o in out.send {
            self.deliver(node, o.to, o.wire, may_forward, t);
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
        t: Tag,
    ) {
        let Ok(bytes) = wire.encode() else { return };
        let msg = AppMessage::Group(bytes.clone());
        if node.send_tagged(&to, msg.clone(), t).is_ok() {
            return;
        }
        if node.can_send_offline(&to) && node.send_offline(&to, &msg).is_ok() {
            return;
        }
        if let (true, GroupWire::Message { group, message }) = (may_forward, &wire)
            && let Some(via) = self.forwarder(node, group, &to)
        {
            // Our own message asks for a receipt; one we forward doesn't.
            let fwd = GroupWire::Forward {
                group: *group,
                to: *to.as_bytes(),
                message: message.clone(),
                reference: if t.relay_for.is_none() { t.local_id } else { 0 },
            };
            if let Ok(b) = fwd.encode()
                && node.send(&via, AppMessage::Group(b)).is_ok()
            {
                return;
            }
        }
        // Hold it for when they connect, and try to make that happen.
        self.held.push((to, bytes, t));
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
            GroupEvent::RoleChanged {
                group,
                member,
                role,
            } => Update::RoleChanged {
                group,
                member,
                role,
            },
            GroupEvent::Text {
                group,
                from,
                text,
                id,
            } => {
                // What our other devices send to the group is ours too.
                let ours = node.is_own_device(&from);
                let timer = node.effective_timer(ConversationId::Group(group));
                node.record_with_id(
                    ConversationId::Group(group),
                    *from.as_bytes(),
                    ours,
                    &text,
                    false,
                    timer,
                    id,
                );
                Update::Text {
                    group,
                    from,
                    text,
                    ours,
                }
            }
            GroupEvent::File {
                group,
                from,
                name,
                data,
                sensitive,
                caption,
                album,
                id,
                clip,
            } => Update::File {
                group,
                from,
                name,
                data,
                ours: node.is_own_device(&from),
                sensitive,
                caption,
                album,
                id,
                clip,
            },
            GroupEvent::React {
                group,
                from,
                id,
                emoji,
                add,
            } => {
                let who = node.reactor_of(&from);
                node.apply_reaction(ConversationId::Group(group), who, id, &emoji, add);
                Update::Reacted { group, from }
            }
        });
    }
}

/// The tag for our own message, history entry `local_id` in `group`.
fn tag(local_id: u64, group: &GroupId) -> Tag {
    Tag {
        local_id,
        group: Some(*group),
        relay_for: None,
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
//   held    = HELD_VERSION || * ( member (32) | delivery tag (Tag::LEN) | len (u32 LE) | GroupWire bytes )

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

fn encode_held(held: &[(PublicIdentity, Vec<u8>, Tag)]) -> Vec<u8> {
    let mut out = vec![HELD_VERSION];
    for (p, b, t) in held {
        out.extend_from_slice(p.as_bytes());
        t.encode(&mut out);
        out.extend_from_slice(&u32::try_from(b.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(b);
    }
    out
}

fn decode_held(b: &[u8]) -> Vec<(PublicIdentity, Vec<u8>, Tag)> {
    let mut out = Vec::new();
    let Some((&HELD_VERSION, mut rest)) = b.split_first() else {
        return out;
    };
    let head_len = 32 + Tag::LEN + 4;
    while let Some((head, r)) = rest.split_at_checked(head_len) {
        let n = 32 + Tag::LEN;
        let len = u32::from_le_bytes([head[n], head[n + 1], head[n + 2], head[n + 3]]) as usize;
        let Some((msg, r)) = r.split_at_checked(len) else {
            break;
        };
        rest = r;
        let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
        if let Ok(p) = PublicIdentity::from_bytes(&peer) {
            out.push((p, msg.to_vec(), Tag::decode(&head[32..n])));
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
        let t = Tag {
            local_id: 9,
            group: Some([1; 16]),
            relay_for: Some([2; 32]),
        };
        let held = vec![(p, vec![1, 2, 3], t), (p, vec![], Tag::NONE)];
        let b = encode_held(&held);
        assert_eq!(decode_held(&b), held);
        assert_eq!(decode_held(&b[..b.len() - 2]), held[..1]);
        for d in [&[][..], &[9, 0, 0]] {
            assert!(decode_invites(d).is_empty() && decode_held(d).is_empty());
        }
    }
}
