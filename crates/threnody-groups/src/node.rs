//! Groups on a running [`Node`]: persistence, consent and delivery, shared
//! by the CLI and the app bindings (feature `node`).
//!
//! Every outgoing group message goes to its member by the first of these
//! that works (Appendix F, "Delivery"):
//!
//! 1. the live session with that member;
//! 2. sealed for the member's mailboxes (Appendix H);
//! 3. forwarded by another member we have a session with, preferring the
//!    coordinator, then the owner's other devices, then admins
//!    (`GroupWire::Forward`), for MLS messages only;
//! 4. held here, and sent when the member connects; meanwhile we try to
//!    reach them through relays (Appendix G).
//!
//! A forwarder delivers the same way but never forwards again, so a
//! message crosses at most one forwarding member.

use threnody_core::call::GroupCallMsg;
use threnody_core::history::{ConversationId, Entry, FileNote, MAX_POLL_QUESTION, Poll};
use threnody_core::message::{Clip, Mention, Quote};
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity};
use threnody_net::history::{Extras, OutgoingFile};
use threnody_net::{Node, Tag};

use crate::{
    Content, GroupError, GroupEvent, GroupId, GroupWire, Groups, MemberRole, Output, Roster,
    RosterChange,
};

type Result<T> = std::result::Result<T, GroupError>;

const STATE: &str = "groups";
const INVITES: &str = "group-invites";
const HELD: &str = "group-held";
const EXPECTED: &str = "group-expected";
const OFFERED: &str = "group-offered";
const OFFERS: &str = "group-offers";
const REFUSED: &str = "group-refused";
const LEFT: &str = "group-left";
const OWNER_SEEN: &str = "group-owner-seen";
const VERSION: u8 = 1;
const HELD_VERSION: u8 = 4;
/// `group-held` before it said whose message each one is.
const HELD_VERSION_NO_ORIGIN: u8 = 3;
/// `group-expected` with each group's owner.
const EXPECTED_VERSION: u8 = 2;
/// Messages held for one member. Messages from more than a few epochs
/// back can't be read anyway.
pub const MAX_HELD_PER_MEMBER: usize = 200;
/// Groups remembered as left or declined; the oldest are forgotten first.
const MAX_LEFT: usize = 256;
/// Bytes held for one member (a few files' worth), and for everyone. Any
/// member can have us hold what it forwards, so this bounds what one can
/// make us keep and rewrite. Past any of these, we drop from
/// whoever has us hold the most, keeping commits and Welcomes longest.
pub const MAX_HELD_BYTES_PER_MEMBER: usize = 32 << 20;
pub const MAX_HELD_BYTES: usize = 128 << 20;
/// Invitations waiting for consent from one inviter, and in all; the
/// oldest are dropped first, those from strangers before the rest.
pub const MAX_INVITES_PER_PEER: usize = 3;
pub const MAX_INVITES: usize = 32;
/// After a stranger's invitation, others from strangers in this long
/// come quietly: new identities cost nothing.
pub const STRANGER_INVITE_QUIET: std::time::Duration = std::time::Duration::from_secs(600);
/// How long after our own device offers us a group we left the
/// invitation that offer sets off is turned down without asking (it may
/// wait for the group's coordinator to connect); later ones ask again.
const REFUSED_FOR_MS: u64 = 2 * 24 * 60 * 60 * 1000;
/// How many times one of our own devices is offered a group on
/// connecting (besides when it is linked) while it neither joins nor says
/// it stays out. An older device that left the group ignores the offer,
/// and the invitation it sets off asks its user again.
const MAX_OFFERS: u8 = 2;
/// How long after one of those offers the next may follow.
const OFFER_RETRY_MS: u64 = 60 * 60 * 1000;
/// How long the owner must have been silent in a group before its
/// successor is offered to take over (client policy: the protocol takes a
/// claim from the successor at any time).
pub const SUCCESSION_SILENCE_MS: u64 = 14 * 24 * 60 * 60 * 1000;

#[cfg(debug_assertions)]
static SILENCE_FOR_TESTING: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Debug builds only: lowers the succession silence (0 restores 14 days),
/// for trying the successor's banner without waiting.
#[cfg(debug_assertions)]
pub fn set_succession_silence_for_testing(ms: u64) {
    SILENCE_FOR_TESTING.store(ms, std::sync::atomic::Ordering::Relaxed);
}

/// The succession silence in force: 14 days, unless lowered in a debug
/// build (`THRENODY_SUCCESSION_SILENCE_MS`, or
/// [`set_succession_silence_for_testing`]).
pub fn succession_silence() -> u64 {
    #[cfg(debug_assertions)]
    {
        let set = SILENCE_FOR_TESTING.load(std::sync::atomic::Ordering::Relaxed);
        if set != 0 {
            return set;
        }
        if let Some(ms) = std::env::var("THRENODY_SUCCESSION_SILENCE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            return ms;
        }
    }
    SUCCESSION_SILENCE_MS
}

/// Whether a successor may claim: the owner was last seen at `seen` (never
/// since we became successor: `None`), and `silence` has passed since.
pub fn claim_allowed(seen: Option<u64>, now: u64, silence: u64) -> bool {
    seen.is_some_and(|s| now.saturating_sub(s) >= silence)
}

/// What can be done to one row of a group's members.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberAction {
    MakeAdmin,
    RemoveAdmin,
    MakeSuccessor,
    NoLongerSuccessor,
    /// Hand the group over to them.
    MakeOwner,
    /// This owner device applies changes from now on.
    UseAsCoordinator,
    /// Every device of theirs in the group.
    Remove,
    /// Take over from a lost coordinating device of ours and remove it.
    RemoveThisDevice,
    /// An admin gives up being one.
    StepDown,
}

/// One row of a group's members: an account (one device, for devices we
/// can't tie to an account).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRow {
    /// The device that stands for the row.
    pub device: PublicIdentity,
    pub devices: Vec<PublicIdentity>,
    /// The highest role of its devices.
    pub role: MemberRole,
    pub successor: bool,
    pub me: bool,
    /// One of its devices coordinates.
    pub coordinator: bool,
    pub actions: Vec<MemberAction>,
}

/// The one banner a group shows, most pressing first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Banner {
    /// This device fell out of step and asked to be brought back.
    OutOfStep,
    /// We're the successor and the owner has been silent long enough.
    CanClaim { owner: PublicIdentity },
    /// Our changes wait for the coordinator.
    PendingChanges {
        count: u32,
        coordinator: PublicIdentity,
    },
    /// Roles wait for these members to update.
    UpgradePending { outdated: Vec<PublicIdentity> },
    /// Nobody else is in the group.
    OnlyYou,
}

/// What a group's state calls for: at most one banner.
pub fn banner(
    out_of_step: bool,
    can_claim: Option<PublicIdentity>,
    pending: Option<(u32, PublicIdentity)>,
    outdated: &[PublicIdentity],
    only_you: bool,
) -> Option<Banner> {
    if out_of_step {
        Some(Banner::OutOfStep)
    } else if let Some(owner) = can_claim {
        Some(Banner::CanClaim { owner })
    } else if let Some((count, coordinator)) = pending.filter(|(n, _)| *n > 0) {
        Some(Banner::PendingChanges { count, coordinator })
    } else if !outdated.is_empty() {
        Some(Banner::UpgradePending {
            outdated: outdated.to_vec(),
        })
    } else if only_you {
        Some(Banner::OnlyYou)
    } else {
        None
    }
}

/// What this device (`me`, in `roster`) may do to the row of `row`
/// devices; `mine` are our own account's devices in the group.
pub fn member_actions(
    roster: &Roster,
    me: &PublicIdentity,
    mine: &[PublicIdentity],
    row: &[PublicIdentity],
) -> Vec<MemberAction> {
    use MemberAction::*;
    let my_role = roster.role(me);
    let ours = row.iter().any(|d| d == me || mine.contains(d));
    let row_role = row
        .iter()
        .map(|d| roster.role(d))
        .min_by_key(|r| r.to_wire())
        .unwrap_or(MemberRole::Member);
    if ours {
        return match my_role {
            MemberRole::Admin => vec![StepDown],
            MemberRole::Owner if roster.lead != *me => {
                let mut v = vec![UseAsCoordinator];
                if row.contains(&roster.lead) {
                    v.push(RemoveThisDevice);
                }
                v
            }
            _ => Vec::new(),
        };
    }
    let successor = row.iter().any(|d| roster.successors.contains(d));
    match (my_role, row_role) {
        (MemberRole::Owner, MemberRole::Member) => vec![MakeAdmin, MakeOwner, Remove],
        (MemberRole::Owner, MemberRole::Admin) => vec![
            RemoveAdmin,
            if successor {
                NoLongerSuccessor
            } else {
                MakeSuccessor
            },
            MakeOwner,
            Remove,
        ],
        (MemberRole::Admin, MemberRole::Member) => vec![Remove],
        _ => Vec::new(),
    }
}

/// An invitation waiting for the user's consent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub group: GroupId,
    pub name: String,
    pub from: PublicIdentity,
}

/// A message waiting for its member to connect.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    to: PublicIdentity,
    /// The encoded `GroupWire`.
    bytes: Vec<u8>,
    /// The delivery tag to send it with.
    tag: Tag,
    /// The member whose message we forward; `None` for our own.
    origin: Option<PublicIdentity>,
    /// A commit, proposal, Welcome (or one to pass on), Resync or Leave
    /// with its proposal: the group can't move on without it, so it is
    /// kept longest.
    handshake: bool,
}

impl Held {
    fn new(to: PublicIdentity, bytes: Vec<u8>, tag: Tag, origin: Option<PublicIdentity>) -> Self {
        let handshake = match GroupWire::decode(&bytes) {
            Ok(GroupWire::Message { message, .. }) => crate::is_handshake(&message),
            Ok(GroupWire::Forward { message, .. }) => crate::is_handshake(&message),
            Ok(
                GroupWire::Welcome { .. }
                | GroupWire::PassWelcome { .. }
                | GroupWire::Resync { .. }
                | GroupWire::Leave {
                    proposal: Some(_), ..
                },
            ) => true,
            _ => false,
        };
        Self {
            to,
            bytes,
            tag,
            origin,
            handshake,
        }
    }
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
    /// Someone who isn't a mutually approved contact invites us. A quiet
    /// one shouldn't notify ([`STRANGER_INVITE_QUIET`]).
    Invited { invite: Invite, quiet: bool },
    /// `by`: who added them, if known.
    MemberAdded {
        group: GroupId,
        member: PublicIdentity,
        by: Option<PublicIdentity>,
    },
    /// `by`: who removed them (themselves, if they left).
    MemberRemoved {
        group: GroupId,
        member: PublicIdentity,
        by: Option<PublicIdentity>,
    },
    /// A member's role was changed.
    RoleChanged {
        group: GroupId,
        member: PublicIdentity,
        role: MemberRole,
    },
    /// A change of ours never made it into the group.
    ChangeFailed { group: GroupId, reason: String },
    /// Roles wait for these members to update Threnody.
    UpgradePending {
        group: GroupId,
        outdated: Vec<PublicIdentity>,
    },
    /// This device fell out of step and asked to be brought back.
    OutOfStep { group: GroupId },
    /// The owner removed us.
    Left { group: GroupId },
    /// `ours`: sent by another device of our own account (shown as ours,
    /// not notified).
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
        ours: bool,
        /// It mentions us, or replies to one of our messages.
        mentioned: bool,
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
        forwarded: bool,
    },
    /// `from` changed its reactions on a message in `group`.
    Reacted {
        group: GroupId,
        from: PublicIdentity,
    },
    /// `from` voted in (or closed) a poll in `group`.
    Voted {
        group: GroupId,
        from: PublicIdentity,
    },
    /// `from` pinned (or unpinned) a message in `group`.
    Pinned {
        group: GroupId,
        from: PublicIdentity,
        pin: bool,
    },
    /// Signalling for the media layer on our leg with `from` in group
    /// call `call` (the node reports the same over sessions as
    /// `Event::GroupLegSignal`).
    CallLeg {
        call: u64,
        from: PublicIdentity,
        data: Vec<u8>,
    },
}

/// A group offered to one of our own devices on connecting, that it
/// neither joined nor said it stays out of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Offer {
    device: PublicIdentity,
    group: GroupId,
    /// How many times so far.
    times: u8,
    /// When last (ms).
    last_ms: u64,
}

impl Offer {
    /// Whether it may be offered again at `now` (ms).
    fn due(&self, now: u64) -> bool {
        self.times == 0
            || self.times < MAX_OFFERS && now.saturating_sub(self.last_ms) >= OFFER_RETRY_MS
    }
}

pub struct GroupNode {
    groups: Groups,
    invites: Vec<Invite>,
    /// Messages waiting for their member to connect, oldest first.
    held: Vec<Held>,
    /// Groups our other devices are in, with their owner (if the device
    /// said) whose invitation we accept.
    expected: Vec<(GroupId, Option<PublicIdentity>)>,
    /// Groups our own other devices were in once (and may have left since),
    /// said they stay out of, or were offered on connecting: one that
    /// leaves a group isn't put back. Those in `offers` are still offered.
    offered: Vec<(PublicIdentity, GroupId)>,
    /// Groups offered to our own other devices on connecting that they
    /// neither joined nor said they stay out of: how many times, and when
    /// last (ms). Offered again up to [`MAX_OFFERS`] times, the offer
    /// may have been lost.
    offers: Vec<Offer>,
    /// Groups we left that our own device offered us again, with the
    /// owner it named and when (ms): the invitation that follows is turned
    /// down quietly.
    refused: Vec<(GroupId, Option<PublicIdentity>, u64)>,
    /// Groups this device left or declined. Our other devices don't add
    /// it back, and an invitation to one needs the user's consent again.
    left: Vec<GroupId>,
    /// When a stranger's invitation last notified.
    stranger_invited: Option<std::time::Instant>,
    /// When each group's owner was last seen in it (ms), for succession.
    owner_seen: Vec<(GroupId, u64)>,
    home: Home,
    identity: Identity,
}

/// Our links in group call `call`, as sent in `Join` and `Update` (to
/// `everyone`, or to one member).
fn links(node: &Node, call: u64, everyone: bool) -> Vec<[u8; 32]> {
    node.group_call_links(call, everyone)
        .iter()
        .map(|p| *p.as_bytes())
        .collect()
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
        let offered = decode_offered(&load(OFFERED));
        let offers = match home.load_state(identity, OFFERS).ok().flatten() {
            Some(b) => decode_offers(&b),
            None => migrate_offers(&offered, &groups.list()),
        };
        Ok(Self {
            groups,
            invites: decode_invites(&load(INVITES)),
            held: decode_held(&load(HELD)),
            expected: decode_expected(&load(EXPECTED)),
            offered,
            offers,
            refused: decode_refused(&load(REFUSED)),
            left: decode_left(&load(LEFT)),
            stranger_invited: None,
            owner_seen: decode_owner_seen(&load(OWNER_SEEN)),
            home: home.reopen(),
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

    /// The group's roster (for a group without one: its creator alone).
    pub fn roster(&self, group: &GroupId) -> Result<Roster> {
        self.groups.roster(group)
    }

    /// Whether the group has a roster agreed by every member.
    pub fn has_roster(&self, group: &GroupId) -> bool {
        self.groups.has_roster(group).unwrap_or(false)
    }

    /// Our changes waiting for the coordinator.
    pub fn pending_changes(&self, group: &GroupId) -> usize {
        self.groups.pending_changes(group)
    }

    /// Members whose version doesn't support roles yet.
    pub fn outdated(&self, group: &GroupId) -> Vec<PublicIdentity> {
        self.groups.outdated(group).unwrap_or_default()
    }

    /// Whether this device coordinates the group.
    pub fn is_lead(&self, group: &GroupId) -> bool {
        self.groups.is_lead(group)
    }

    /// Whether we asked to be brought back in step and haven't been yet.
    pub fn out_of_step(&self, group: &GroupId) -> bool {
        self.groups.out_of_step(group)
    }

    /// Tests only: when the owner was last seen in `group`.
    #[doc(hidden)]
    pub fn set_owner_seen_for_testing(&mut self, group: &GroupId, ms: u64) {
        self.owner_seen.retain(|(g, _)| g != group);
        self.owner_seen.push((*group, ms));
    }

    /// Whether we are the group's successor and its owner has been silent
    /// for [`succession_silence`] (as of `now_ms`).
    pub fn can_claim(&self, node: &Node, group: &GroupId, now_ms: u64) -> bool {
        let me = node.identity();
        let Ok(roster) = self.groups.roster(group) else {
            return false;
        };
        let seen = self
            .owner_seen
            .iter()
            .find(|(g, _)| g == group)
            .map(|(_, t)| *t);
        roster.successors.contains(&me) && claim_allowed(seen, now_ms, succession_silence())
    }

    /// One row per account of the group's members, with what we may do
    /// to each.
    pub fn member_rows(&self, node: &Node, group: &GroupId) -> Result<Vec<MemberRow>> {
        let me = node.identity();
        let roster = self.groups.roster(group)?;
        let members = self.members(group)?;
        let mine: Vec<PublicIdentity> = members
            .iter()
            .copied()
            .filter(|d| *d == me || node.is_own_device(d))
            .collect();
        let mut rows: Vec<([u8; 32], Vec<PublicIdentity>)> = Vec::new();
        for d in &members {
            let key = node.reactor_of(d);
            match rows.iter_mut().find(|(k, _)| *k == key) {
                Some((_, ds)) => ds.push(*d),
                None => rows.push((key, vec![*d])),
            }
        }
        let mut out: Vec<MemberRow> = rows
            .into_iter()
            .map(|(_, devices)| {
                let role = devices
                    .iter()
                    .map(|d| roster.role(d))
                    .min_by_key(|r| r.to_wire())
                    .unwrap_or(MemberRole::Member);
                let is_me = devices.iter().any(|d| mine.contains(d));
                // Our own row stands for this device; others by a device
                // that holds their highest role.
                let device = if is_me {
                    me
                } else {
                    devices
                        .iter()
                        .copied()
                        .find(|d| *d == roster.lead)
                        .or_else(|| devices.iter().copied().find(|d| roster.role(d) == role))
                        .unwrap_or(devices[0])
                };
                MemberRow {
                    device,
                    successor: devices.iter().any(|d| roster.successors.contains(d)),
                    me: is_me,
                    coordinator: devices.contains(&roster.lead),
                    actions: member_actions(&roster, &me, &mine, &devices),
                    devices,
                    role,
                }
            })
            .collect();
        // Owners, then admins, then members; ourselves first among equals.
        out.sort_by_key(|r| (r.role.to_wire(), !r.me));
        Ok(out)
    }

    /// The banner the group should show here now.
    pub fn banner(&self, node: &Node, group: &GroupId, now_ms: u64) -> Option<Banner> {
        let roster = self.groups.roster(group).ok()?;
        let members = self.members(group).ok()?;
        let me = node.identity();
        let pending = u32::try_from(self.pending_changes(group)).unwrap_or(u32::MAX);
        let outdated = if self.has_roster(group)
            || !self
                .groups
                .list()
                .iter()
                .any(|(g, _, o, _)| g == group && *o == me)
        {
            Vec::new()
        } else if self.groups.pending_changes(group) > 0 {
            self.outdated(group)
        } else {
            Vec::new()
        };
        banner(
            self.out_of_step(group),
            self.can_claim(node, group, now_ms).then_some(roster.lead),
            // Ours wait for no one when we coordinate: only, at most, for
            // members to update (`outdated`).
            (pending > 0 && outdated.is_empty() && roster.lead != me)
                .then_some((pending, roster.lead)),
            &outdated,
            members.len() <= 1,
        )
    }

    /// How many messages are waiting for `member` to connect.
    pub fn held_for(&self, member: &PublicIdentity) -> usize {
        self.held.iter().filter(|h| h.to == *member).count()
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
                    self.apply(node, out, true)?;
                }
                Err(GroupError::AlreadyMember(_)) => {}
                Err(e) => return Err(e),
            }
        }
        if invited == 0 {
            return Err(GroupError::AlreadyMember(peer.fingerprint().to_string()));
        }
        // Our other devices join too, without waiting to reconnect.
        self.offer_to_siblings(node, group);
        Ok(invited)
    }

    /// Accepts the first invitation to `group` (the one [`Self::invites`]
    /// lists first).
    pub fn accept(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let from = self
            .invites
            .iter()
            .find(|i| i.group == *group)
            .ok_or(GroupError::UnknownGroup)?
            .from;
        self.accept_from(node, group, &from)
    }

    /// Accepts `from`'s invitation to `group`: only `from` gets a key
    /// package, and only its Welcome is taken.
    pub fn accept_from(
        &mut self,
        node: &Node,
        group: &GroupId,
        from: &PublicIdentity,
    ) -> Result<Vec<Update>> {
        let i = self
            .invites
            .iter()
            .position(|i| i.group == *group && i.from == *from)
            .ok_or(GroupError::UnknownGroup)?;
        let inv = self.invites.remove(i);
        self.save_invites();
        let out = self.groups.accept_invite(&inv.group, inv.from)?;
        if self.left.contains(group) {
            self.left.retain(|g| g != group);
            self.save_left();
        }
        self.apply(node, out, true)
    }

    pub fn decline(&mut self, group: &GroupId) {
        self.invites.retain(|i| i.group != *group);
        self.save_invites();
        self.note_left(*group);
    }

    /// Remembers that this device left (or declined) `group`.
    fn note_left(&mut self, group: GroupId) {
        self.left.retain(|g| *g != group);
        self.left.push(group);
        if let Some(extra) = self.left.len().checked_sub(MAX_LEFT) {
            self.left.drain(..extra);
        }
        self.save_left();
        if self.expected.iter().any(|(g, _)| *g == group) {
            self.expected.retain(|(g, _)| *g != group);
            self.save_expected();
        }
    }

    /// Declines `from`'s invitation to `group`, keeping any other
    /// inviter's to the same group.
    pub fn decline_from(&mut self, group: &GroupId, from: &PublicIdentity) {
        self.invites
            .retain(|i| !(i.group == *group && i.from == *from));
        self.save_invites();
        self.note_left(*group);
    }

    /// Removes every device of `peer`'s account that is in the group (we
    /// must be an owner or, for a member, an admin).
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
            updates.extend(self.apply(node, out, true)?);
        }
        Ok(updates)
    }

    /// Changes roles. In a group without a roster, the first change also
    /// makes our own other devices in it owners.
    pub fn change_roster(
        &mut self,
        node: &Node,
        group: &GroupId,
        changes: Vec<RosterChange>,
    ) -> Result<Vec<Update>> {
        let me = node.identity();
        let mut changes = changes;
        if !self.has_roster(group) {
            let ours: Vec<PublicIdentity> = self
                .members(group)?
                .into_iter()
                .filter(|d| *d != me && node.is_own_device(d))
                .collect();
            if !ours.is_empty() {
                changes.insert(0, RosterChange::AddOwners(ours));
            }
        }
        let out = self.groups.change_roster(group, &changes)?;
        self.apply(node, out, true)
    }

    /// `peer`'s devices in the group (just `peer` for an account we don't
    /// know), with `peer` first.
    fn devices_in(
        &self,
        node: &Node,
        group: &GroupId,
        peer: &PublicIdentity,
    ) -> Result<Vec<PublicIdentity>> {
        let members = self.members(group)?;
        let mut devices: Vec<PublicIdentity> = account_devices(node, peer)
            .into_iter()
            .filter(|d| members.contains(d) && d != peer)
            .collect();
        if members.contains(peer) {
            devices.insert(0, *peer);
        }
        if devices.is_empty() {
            return Err(GroupError::NotMember(peer.fingerprint().to_string()));
        }
        Ok(devices)
    }

    /// Owner: makes every device of `peer`'s account in the group an admin.
    pub fn promote(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let roster = self.roster(group)?;
        let devices: Vec<PublicIdentity> = self
            .devices_in(node, group, &peer)?
            .into_iter()
            .filter(|d| roster.role(d) == MemberRole::Member)
            .collect();
        if devices.is_empty() {
            return Err(GroupError::NotAllowed("already an admin"));
        }
        self.change_roster(node, group, vec![RosterChange::Promote(devices)])
    }

    /// Owner: `peer`'s account is no longer admin. For ourselves (an
    /// admin), steps down on this device; the coordinator follows on our
    /// other devices when it knows them.
    pub fn demote(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let me = node.identity();
        if peer == me || node.is_own_device(&peer) {
            return self.change_roster(node, group, vec![RosterChange::Demote(vec![me])]);
        }
        let roster = self.roster(group)?;
        let devices: Vec<PublicIdentity> = self
            .devices_in(node, group, &peer)?
            .into_iter()
            .filter(|d| roster.role(d) == MemberRole::Admin)
            .collect();
        if devices.is_empty() {
            return Err(GroupError::NotAllowed("only admins can be demoted"));
        }
        self.change_roster(node, group, vec![RosterChange::Demote(devices)])
    }

    /// Owner: hands the group to `peer`'s account (its devices in the group
    /// own it; `peer` coordinates). Our devices become admins.
    pub fn transfer(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let devices = self.devices_in(node, group, &peer)?;
        self.change_roster(node, group, vec![RosterChange::Transfer(devices)])
    }

    /// Owner: hands the group to `peer`'s account and leaves it. Only once
    /// the handover can go in at once (or, from an owner device that isn't
    /// the coordinator, ahead of our leaving): in a group still waiting for
    /// members to update, nothing changes.
    pub fn transfer_and_leave(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: PublicIdentity,
    ) -> Result<Vec<Update>> {
        let me = node.identity();
        if !self.has_roster(group) {
            return Err(GroupError::NotAllowed(
                "everyone must update Threnody before you can hand the group over",
            ));
        }
        let mut updates = self.transfer(node, group, peer)?;
        if self.roster(group)?.lead == me {
            return Err(GroupError::NotAllowed(
                "the handover hasn't gone through yet: try leaving again later",
            ));
        }
        updates.extend(self.leave(node, group)?);
        Ok(updates)
    }

    /// Owner: makes `peer`'s account (admins all) the successor, who may
    /// take over should we fall silent; `None` clears it.
    pub fn set_successor(
        &mut self,
        node: &Node,
        group: &GroupId,
        peer: Option<PublicIdentity>,
    ) -> Result<Vec<Update>> {
        let devices = match peer {
            None => Vec::new(),
            Some(p) => {
                let roster = self.roster(group)?;
                let d: Vec<PublicIdentity> = self
                    .devices_in(node, group, &p)?
                    .into_iter()
                    .filter(|d| roster.role(d) == MemberRole::Admin)
                    .collect();
                if d.is_empty() {
                    return Err(GroupError::NotAllowed("make them an admin first"));
                }
                d
            }
        };
        self.change_roster(node, group, vec![RosterChange::Successor(devices)])
    }

    /// An owner device makes `device` (another owner device, or itself)
    /// the coordinator, through the current one.
    pub fn use_as_coordinator(
        &mut self,
        node: &Node,
        group: &GroupId,
        device: PublicIdentity,
    ) -> Result<Vec<Update>> {
        self.change_roster(node, group, vec![RosterChange::Lead(device)])
    }

    /// An owner device takes over as coordinator now, without the current
    /// one, applying our pending changes.
    pub fn take_over(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let out = self.groups.take_over(group)?;
        self.apply(node, out, true)
    }

    /// Takes over from a lost coordinating device of ours and removes it.
    pub fn remove_coordinator(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let lead = self.roster(group)?.lead;
        if !node.is_own_device(&lead) {
            return Err(GroupError::NotAllowed("not one of our devices"));
        }
        let out = self.groups.take_over_removing(group, &[lead])?;
        self.apply(node, out, true)
    }

    /// The successor takes ownership (the apps offer it after
    /// [`succession_silence`], see [`Self::can_claim`]).
    pub fn claim(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let out = self.groups.claim(group)?;
        self.apply(node, out, true)
    }

    /// Leaves a group: asks the coordinator to remove us; the group is gone
    /// here at once. The last owner device must hand the group over or
    /// delete it instead, unless nobody else is in it.
    pub fn leave(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let me = node.identity();
        let roster = self.roster(group)?;
        let members = self.members(group)?;
        let other_owner = roster
            .owners
            .iter()
            .any(|o| *o != me && members.contains(o));
        let out = if roster.role(&me) == MemberRole::Owner && !other_owner {
            if members.iter().any(|m| *m != me) {
                return Err(GroupError::NotAllowed(
                    "transfer ownership first, or delete the group",
                ));
            }
            self.groups.disband(group)?
        } else {
            self.groups.leave(group)?
        };
        self.gone(group);
        self.apply(node, out, true)
    }

    /// Owner: deletes the group for everyone.
    pub fn delete(&mut self, node: &Node, group: &GroupId) -> Result<Vec<Update>> {
        let out = self.groups.disband(group)?;
        self.gone(group);
        self.apply(node, out, true)
    }

    /// We left `group` (or deleted it): copies held for its members are
    /// moot, and our other devices don't add this one back.
    fn gone(&mut self, group: &GroupId) {
        self.held
            .retain(|h| GroupWire::decode(&h.bytes).map_or(true, |w| w.group() != group));
        self.save_held();
        self.owner_seen.retain(|(g, _)| g != group);
        self.save_owner_seen();
        self.note_left(*group);
    }

    /// After a start: the coordinator commits what it stored but hadn't
    /// committed, and members propose upgrades that are due.
    pub fn resume_all(&mut self, node: &Node) -> Vec<Update> {
        let mut updates = Vec::new();
        for (group, ..) in self.groups.list() {
            if let Ok(out) = self.groups.resume(&group)
                && (!out.send.is_empty() || !out.events.is_empty())
                && let Ok(u) = self.apply(node, out, true)
            {
                updates.extend(u);
            }
        }
        updates
    }

    /// Sends text to every other member and records it in the group's
    /// history, to be marked as members acknowledge their copies.
    pub fn send_text(&mut self, node: &Node, group: &GroupId, text: &str) -> Result<()> {
        self.send_reply(node, group, text, None)
    }

    /// [`Self::send_text`], replying to (quoting) an earlier message.
    pub fn send_reply(
        &mut self,
        node: &Node,
        group: &GroupId,
        text: &str,
        quote: Option<Quote>,
    ) -> Result<()> {
        self.send_message(node, group, text, quote, Vec::new())
    }

    /// [`Self::send_text`], with what it quotes and who it mentions.
    pub fn send_message(
        &mut self,
        node: &Node,
        group: &GroupId,
        text: &str,
        quote: Option<Quote>,
        mentions: Vec<Mention>,
    ) -> Result<()> {
        self.send_text_with(
            node,
            group,
            text,
            Extras {
                quote,
                mentions,
                forwarded: false,
            },
        )
    }

    /// [`Self::send_text`], with what it quotes, who it mentions, and
    /// whether it's forwarded.
    pub fn send_text_with(
        &mut self,
        node: &Node,
        group: &GroupId,
        text: &str,
        extras: Extras,
    ) -> Result<()> {
        let id = threnody_net::history::local_id();
        self.send_text_with_id(node, group, text, extras, id)
    }

    /// [`Self::send_text_with`], recorded (and sent) under `id`: a
    /// scheduled message keeps the id it was scheduled with, so it is
    /// never sent twice.
    pub fn send_text_with_id(
        &mut self,
        node: &Node,
        group: &GroupId,
        text: &str,
        extras: Extras,
        id: u64,
    ) -> Result<()> {
        let content = Content::Text {
            text: text.to_owned(),
            id,
            quote: extras.quote,
            mentions: Mention::valid(extras.mentions, text),
            forwarded: extras.forwarded,
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
        self.apply(node, out, true)?;
        Ok(true)
    }

    /// Asks the group `question`, with 2 to 12 `options`; `multi` lets
    /// members choose several.
    pub fn send_poll(
        &mut self,
        node: &Node,
        group: &GroupId,
        question: &str,
        options: Vec<String>,
        multi: bool,
    ) -> Result<()> {
        let question: String = question.trim().chars().take(MAX_POLL_QUESTION).collect();
        let poll = Poll::new(options, multi)
            .filter(|_| !question.is_empty())
            .ok_or(GroupError::Unexpected(
                "a poll needs a question and 2 to 12 options",
            ))?;
        let id = threnody_net::history::local_id();
        let content = Content::Poll {
            id,
            question,
            options: poll.options,
            multi,
        };
        self.send(node, group, &content, None, id)
    }

    /// Sets our vote in poll `id` (no choices: takes it back), for every
    /// member. Returns false if that changed nothing (or the poll's closed).
    pub fn vote(&mut self, node: &Node, group: &GroupId, id: u64, choices: &[u32]) -> Result<bool> {
        let me = node.reactor_of(&node.identity());
        if !node.vote_in(ConversationId::Group(*group), id, me, choices) {
            return Ok(false);
        }
        let content = Content::Vote {
            id,
            choices: choices.to_vec(),
        };
        let out = self.groups.send(group, &content)?;
        self.apply(node, out, true)?;
        Ok(true)
    }

    /// Closes our poll `id` for every member: it takes no more votes.
    pub fn close_poll(&mut self, node: &Node, group: &GroupId, id: u64) -> Result<bool> {
        let ours = |d: &[u8; 32]| {
            *d == *node.identity().as_bytes()
                || PublicIdentity::from_bytes(d).is_ok_and(|p| node.is_own_device(&p))
        };
        if !node.close_poll_in(ConversationId::Group(*group), id, ours) {
            return Ok(false);
        }
        let out = self.groups.send(group, &Content::ClosePoll { id })?;
        self.apply(node, out, true)?;
        Ok(true)
    }

    /// Starts a group call in `group`, ringing every other member.
    /// Returns its id.
    pub fn start_call(&mut self, node: &Node, group: &GroupId, video: bool) -> Result<u64> {
        self.groups
            .list()
            .iter()
            .find(|(g, ..)| g == group)
            .ok_or(GroupError::UnknownGroup)?;
        let call = loop {
            let id = u64::from_be_bytes(threnody_core::crypto::random_bytes());
            if id != 0 {
                break id;
            }
        };
        let secret = threnody_core::crypto::random_bytes::<32>();
        node.start_group_call(*group, call, secret, video)
            .map_err(|e| GroupError::Call(e.to_string()))?;
        self.send_call(
            node,
            group,
            &GroupCallMsg::Start {
                call,
                video,
                secret,
            },
            None,
        )?;
        Ok(call)
    }

    /// Joins the group call ringing here.
    pub fn join_call(&mut self, node: &Node, call: u64, video: bool) -> Result<()> {
        let group = node
            .group_call()
            .filter(|c| c.call == call)
            .ok_or(GroupError::Unexpected("no such call"))?
            .group;
        node.join_group_call(call, video)
            .map_err(|e| GroupError::Call(e.to_string()))?;
        let links = links(node, call, true);
        self.send_call(
            node,
            &group,
            &GroupCallMsg::Join { call, video, links },
            None,
        )
    }

    /// Leaves the group call (or stops it ringing here).
    pub fn leave_call(&mut self, node: &Node, call: u64) {
        let Some(c) = node.group_call().filter(|c| c.call == call) else {
            return;
        };
        node.leave_group_call(call);
        if c.joined {
            let _ = self.send_call(node, &c.group, &GroupCallMsg::Leave { call }, None);
        }
    }

    /// Turns our video in the group call on or off.
    pub fn set_call_video(&mut self, node: &Node, call: u64, video: bool) -> Result<()> {
        let group = node
            .group_call()
            .filter(|c| c.call == call && c.joined)
            .ok_or(GroupError::Unexpected("no such call"))?
            .group;
        node.set_group_call_video_flag(call, video);
        let links = links(node, call, true);
        self.send_call(
            node,
            &group,
            &GroupCallMsg::Update { call, video, links },
            None,
        )
    }

    /// Tells the call's participants which of them we have sessions with
    /// now (on `Event::GroupCallLinks`), so they can relay through us.
    pub fn announce_call_links(&mut self, node: &Node, call: u64) -> Result<()> {
        let Some(c) = node.group_call().filter(|c| c.call == call && c.joined) else {
            return Ok(());
        };
        let links = links(node, call, true);
        let update = GroupCallMsg::Update {
            call,
            video: c.video,
            links,
        };
        self.send_call(node, &c.group, &update, None)
    }

    /// Sends the media layer's signalling for our leg with `to`: over our
    /// session with it if there is one, else in a group message to it.
    pub fn send_call_leg(
        &mut self,
        node: &Node,
        call: u64,
        to: &PublicIdentity,
        data: Vec<u8>,
    ) -> Result<()> {
        if node.send_leg_signal(call, to, data.clone()) {
            return Ok(());
        }
        let group = node
            .group_call()
            .filter(|c| c.call == call && c.joined)
            .ok_or(GroupError::Unexpected("no such call"))?
            .group;
        self.send_call(
            node,
            &group,
            &GroupCallMsg::Leg {
                call,
                to: *to.as_bytes(),
                data,
            },
            Some(to),
        )
    }

    /// Sends call signalling in a group message: to every member, or to
    /// `only` one. Live sessions only, directly or through a member that
    /// can reach it: a call doesn't wait, so nothing is held or sealed
    /// for later.
    fn send_call(
        &mut self,
        node: &Node,
        group: &GroupId,
        msg: &GroupCallMsg,
        only: Option<&PublicIdentity>,
    ) -> Result<()> {
        let bytes = msg.encode().map_err(GroupError::from)?;
        let out = self.groups.send(group, &Content::Call(bytes))?;
        let _ = self.save();
        for o in out.send {
            if only.is_some_and(|p| *p != o.to) {
                continue;
            }
            let GroupWire::Message {
                group,
                message,
                proposals,
                sync,
            } = o.wire
            else {
                continue;
            };
            let direct = GroupWire::Message {
                group,
                message: message.clone(),
                proposals: proposals.clone(),
                sync,
            };
            if let Ok(b) = direct.encode()
                && node.send(&o.to, AppMessage::Group(b)).is_ok()
            {
                continue;
            }
            if let Some(via) = self.forwarder(node, &group, &o.to, &[]) {
                let fwd = GroupWire::Forward {
                    group,
                    to: *o.to.as_bytes(),
                    message,
                    reference: 0,
                    proposals,
                    sync,
                };
                if let Ok(b) = fwd.encode() {
                    let _ = node.send(&via, AppMessage::Group(b));
                }
            }
        }
        Ok(())
    }

    /// Group call signalling from member `from` of `group`.
    fn on_call(
        &mut self,
        node: &Node,
        group: GroupId,
        from: PublicIdentity,
        msg: &[u8],
    ) -> Option<Update> {
        let msg = GroupCallMsg::decode(msg).ok()?;
        let call = msg.call();
        // Our own other devices' calls don't ring here.
        let ours = node.is_own_device(&from);
        let devices = |links: Vec<[u8; 32]>| -> Vec<PublicIdentity> {
            links
                .iter()
                .filter_map(|l| PublicIdentity::from_bytes(l).ok())
                .collect()
        };
        match msg {
            GroupCallMsg::Start { video, secret, .. } if !ours => {
                node.group_call_rang(group, call, from, secret, video);
            }
            GroupCallMsg::Start { .. } => {}
            GroupCallMsg::Join { video, links, .. } => {
                let new = node.group_call_joined(call, from, video, devices(links));
                // A newcomer may have missed that we're in: say so, to it.
                if let Some(c) = node
                    .group_call()
                    .filter(|c| new && c.call == call && c.joined)
                {
                    let join = GroupCallMsg::Join {
                        call,
                        video: c.video,
                        links: self::links(node, call, false),
                    };
                    let _ = self.send_call(node, &group, &join, Some(&from));
                }
            }
            GroupCallMsg::Leave { .. } => node.group_call_left(call, from),
            GroupCallMsg::Update { video, links, .. } => {
                node.group_call_update(call, from, video, devices(links));
            }
            GroupCallMsg::Leg { to, data, .. } => {
                if to != *node.identity().as_bytes() {
                    return None;
                }
                return Some(Update::CallLeg { call, from, data });
            }
        }
        None
    }

    /// Sends a list to every member (see `Node::edit_list`).
    pub fn send_list(&mut self, node: &Node, group: &GroupId, list: Vec<u8>) -> Result<()> {
        let out = self.groups.send(group, &Content::List(list))?;
        self.apply(node, out, true)?;
        Ok(())
    }

    /// Pins (or unpins) message `id` in `group` for every member. Only the
    /// owner and admins may; returns false if there's no such message.
    pub fn pin(&mut self, node: &Node, group: &GroupId, id: u64, pin: bool) -> Result<bool> {
        if !self.may_pin(group, &node.identity()) {
            return Err(GroupError::NotOwner);
        }
        let conv = ConversationId::Group(*group);
        let known = node
            .history(conv)
            .is_ok_and(|h| id != 0 && h.entries().iter().any(|e| e.message_id() == id));
        if !known {
            return Ok(false);
        }
        node.pin_in(conv, id, pin);
        let out = self.groups.send(group, &Content::Pin { id, pin })?;
        self.apply(node, out, true)?;
        Ok(true)
    }

    /// Whether `who` may pin messages in `group`: its owners and admins,
    /// as the roster every member holds says.
    pub fn may_pin(&self, group: &GroupId, who: &PublicIdentity) -> bool {
        self.members(group).is_ok_and(|m| m.contains(who))
            && self
                .groups
                .role_of(group, who)
                .is_ok_and(MemberRole::can_manage_members)
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
            forwarded: file.forwarded,
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
            forwarded: file.forwarded,
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
        self.apply_tagged(node, out, true, tag(local_id, group), None)?;
        let now = threnody_core::now_ms();
        node.append(
            ConversationId::Group(*group),
            Entry {
                at_ms: now,
                outgoing: true,
                device: *node.identity().as_bytes(),
                text: match content {
                    Content::Text { text: t, .. }
                    | Content::File { caption: t, .. }
                    | Content::Poll { question: t, .. } => t.clone(),
                    _ => String::new(),
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
                quote: match content {
                    Content::Text { quote, .. } => quote.clone(),
                    _ => None,
                },
                mentions: match content {
                    Content::Text { mentions, .. } => mentions.clone(),
                    _ => Vec::new(),
                },
                poll: match content {
                    Content::Poll { options, multi, .. } => Poll::new(options.clone(), *multi),
                    _ => None,
                },
                forwarded: matches!(
                    content,
                    Content::Text {
                        forwarded: true,
                        ..
                    }
                ),
                list: None,
                ticked_ms: 0,
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
                quote: None,
                mentions: Vec::new(),
                poll: None,
                forwarded: false,
                list: None,
                ticked_ms: 0,
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
                self.apply_tagged(node, out, false, t, Some(from))
            }
            GroupWire::AddDevice {
                group,
                device,
                owner,
            } => self.add_device(node, from, group, device, owner),
            // Our own device, offered a group it left, stays out.
            GroupWire::Leave {
                group,
                proposal: None,
            } if node.is_own_device(&from)
                && self.members(&group).is_ok_and(|m| !m.contains(&from)) =>
            {
                self.settle(from, group);
                Ok(Vec::new())
            }
            wire => {
                let out = self.groups.handle(from, wire)?;
                self.apply(node, out, true)
            }
        }
    }

    /// `from` asks us to add `device`, another device of its account, to
    /// `group`; or, from our own device about us, says to join it when
    /// `owner` invites us.
    fn add_device(
        &mut self,
        node: &Node,
        from: PublicIdentity,
        group: GroupId,
        device: [u8; 32],
        owner: Option<[u8; 32]>,
    ) -> Result<Vec<Update>> {
        let device = PublicIdentity::from_bytes(&device)?;
        if device == node.identity() {
            if !node.is_own_device(&from) {
                return Ok(Vec::new());
            }
            let owner = owner.map(|o| PublicIdentity::from_bytes(&o)).transpose()?;
            if self.left.contains(&group) {
                // We stay out: say so, so our device stops offering it, and
                // turn down the invitation it set off without asking the
                // user again.
                let now = threnody_core::now_ms();
                self.refused
                    .retain(|(g, _, at)| *g != group && now.saturating_sub(*at) < REFUSED_FOR_MS);
                self.refused.push((group, owner, now));
                self.save_refused();
                let held = self.held.len();
                let stay_out = GroupWire::Leave {
                    group,
                    proposal: None,
                };
                self.deliver(node, from, stay_out, false, Tag::NONE);
                if self.held.len() != held {
                    self.save_held();
                }
                return Ok(Vec::new());
            }
            // Only the owner's invitation is taken on our device's word.
            // From an older device that doesn't name it, we still ask.
            self.expected.retain(|(g, _)| *g != group);
            self.expected.push((group, owner));
            self.save_expected();
            // The invitation may already be waiting.
            if let Some(owner) = owner
                && self
                    .invites
                    .iter()
                    .any(|i| i.group == group && i.from == owner)
            {
                return self.accept_from(node, &group, &owner);
            }
            return Ok(Vec::new());
        }
        // Only for a member, about a device of the same account.
        let members = self.members(&group)?;
        let same_account = node
            .account_of(&from)
            .is_some_and(|a| a.state().has(&from) && a.state().has(&device));
        if !members.contains(&from) || members.contains(&device) || !same_account {
            return Ok(Vec::new());
        }
        let out = self.groups.invite(&group, device)?;
        self.apply(node, out, true)
    }

    /// Our own device `sibling` is connected (or just linked): it joins
    /// every group we're in that it isn't (`only`: just that one). We add
    /// it ourselves if we may (an owner or admin), otherwise ask the
    /// coordinator; and tell it to accept the invitation.
    ///
    /// Once it has been in the group, or says it stays out, it is never
    /// offered that group again. Until then, an offer may have been lost:
    /// the device that was just linked may be a process that runs no
    /// groups (`threnody link`). So the offer made on linking doesn't
    /// count, and those made otherwise (`count`) are repeated up to
    /// [`MAX_OFFERS`] times, [`OFFER_RETRY_MS`] apart: an older device that
    /// left the group ignores them, and its user is asked each time.
    fn add_sibling(
        &mut self,
        node: &Node,
        sibling: &PublicIdentity,
        only: Option<GroupId>,
        count: bool,
    ) {
        let me = node.identity();
        let now = threnody_core::now_ms();
        let mut counted = false;
        let mut todo = Vec::new();
        for (group, _, owner, members, roles) in self.groups.list_with_roles() {
            if only.is_some_and(|o| o != group) {
                continue;
            }
            if members.contains(sibling) {
                // It's in: should it leave, it stays out.
                self.settle(*sibling, group);
                continue;
            }
            let pair = (*sibling, group);
            let offer = self
                .offers
                .iter()
                .position(|o| o.device == *sibling && o.group == group);
            if offer.is_none() && self.offered.contains(&pair) {
                continue;
            }
            if count {
                match offer {
                    Some(i) if !self.offers[i].due(now) => continue,
                    Some(i) => {
                        self.offers[i].times += 1;
                        self.offers[i].last_ms = now;
                    }
                    None => self.offers.push(Offer {
                        device: *sibling,
                        group,
                        times: 1,
                        last_ms: now,
                    }),
                }
                if !self.offered.contains(&pair) {
                    self.offered.push(pair);
                }
                counted = true;
            }
            todo.push((group, owner, members, roles));
        }
        if counted {
            self.save_offered();
            self.save_offers();
        }
        if todo.is_empty() {
            return;
        }
        for (group, owner, members, roles) in todo {
            let hint = |owner| GroupWire::AddDevice {
                group,
                device: *sibling.as_bytes(),
                owner,
            };
            self.deliver(
                node,
                *sibling,
                hint(Some(*owner.as_bytes())),
                false,
                Tag::NONE,
            );
            let may_add = members
                .iter()
                .position(|m| *m == me)
                .and_then(|i| roles.get(i))
                .is_some_and(|r| r.can_manage_members());
            if may_add {
                if let Ok(out) = self.groups.invite(&group, *sibling) {
                    let _ = self.apply(node, out, true);
                }
            } else {
                self.deliver(node, owner, hint(None), true, Tag::NONE);
            }
        }
        self.save_held();
    }

    /// Our own `device` is in `group`, or says it stays out: should it
    /// leave, or once it did, it isn't offered the group again.
    fn settle(&mut self, device: PublicIdentity, group: GroupId) {
        if !self.offered.contains(&(device, group)) {
            self.offered.push((device, group));
            self.save_offered();
        }
        let before = self.offers.len();
        self.offers
            .retain(|o| o.device != device || o.group != group);
        if self.offers.len() != before {
            self.save_offers();
        }
    }

    /// Our own devices connected now join `group`, which we just started
    /// adding people to.
    fn offer_to_siblings(&mut self, node: &Node, group: &GroupId) {
        let mut siblings: Vec<PublicIdentity> = node
            .sessions()
            .iter()
            .map(|s| s.peer)
            .filter(|p| node.is_own_device(p))
            .collect();
        siblings.sort_by_key(|p| *p.as_bytes());
        siblings.dedup();
        for s in siblings {
            self.add_sibling(node, &s, Some(*group), true);
        }
    }

    /// Devices were removed from their accounts (`Event::AccountChanged`):
    /// they lose their roles and their membership, so later epochs are
    /// unreadable to them, and nothing held for them is sent. The
    /// coordinator removes them; an owner device or admin that may
    /// proposes it; if the device removed was our own coordinating device,
    /// the remaining owner device of ours with the lowest identity takes
    /// over and removes it. Always on: a stolen phone keeps no power.
    pub fn revoked(&mut self, node: &Node, removed: &[PublicIdentity]) -> Vec<Update> {
        let removed: Vec<PublicIdentity> = removed
            .iter()
            .copied()
            .filter(|d| node.is_revoked(d))
            .collect();
        if removed.is_empty() {
            return Vec::new();
        }
        let held = self.held.len();
        self.held.retain(|h| !removed.contains(&h.to));
        if self.held.len() != held {
            self.save_held();
        }
        let me = node.identity();
        let mut updates = Vec::new();
        for (group, _, lead, members) in self.groups.list() {
            let gone: Vec<PublicIdentity> = members
                .iter()
                .copied()
                .filter(|m| removed.contains(m))
                .collect();
            if gone.is_empty() {
                continue;
            }
            let Ok(roster) = self.groups.roster(&group) else {
                continue;
            };
            let out = if gone.contains(&lead) && roster.role(&me) == MemberRole::Owner {
                // The coordinating device of our account was removed (it's
                // no longer "ours" now): one of us takes over, the same one
                // on every device.
                let next = roster
                    .owners
                    .iter()
                    .copied()
                    .filter(|o| {
                        !removed.contains(o)
                            && members.contains(o)
                            && (*o == me || node.is_own_device(o))
                    })
                    .min_by_key(|o| *o.as_bytes());
                if next != Some(me) {
                    continue;
                }
                self.groups.take_over_removing(&group, &gone)
            } else {
                let mut out = Output::default();
                for d in &gone {
                    if *d == lead {
                        continue;
                    }
                    if let Ok(o) = self.groups.remove(&group, d) {
                        out.send.extend(o.send);
                        out.events.extend(o.events);
                    }
                }
                Ok(out)
            };
            if let Ok(out) = out
                && let Ok(u) = self.apply(node, out, true)
            {
                updates.extend(u);
            }
        }
        updates
    }

    /// Adds our newly linked `device` to our groups (`Event::DeviceLinked`).
    pub fn linked(&mut self, node: &Node, device: &PublicIdentity) {
        if node.is_own_device(device) {
            // Not counted: the process being linked may run no groups,
            // and the device is offered them again when it connects.
            self.add_sibling(node, device, None, false);
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
    /// Our own devices also join the groups we're in.
    pub fn connected(&mut self, node: &Node, peer: &PublicIdentity) {
        if node.is_own_device(peer) {
            self.add_sibling(node, peer, None, true);
        }
        // An owner showing up resets the succession clock; our coordinator
        // showing up takes what waited for it.
        for (group, ..) in self.groups.list() {
            let Ok(roster) = self.groups.roster(&group) else {
                continue;
            };
            if roster.owners.contains(peer) {
                self.stamp_owner_seen(&group);
            }
            if roster.lead == *peer
                && let Ok(out) = self.groups.resume(&group)
                && !out.send.is_empty()
            {
                let _ = self.apply(node, out, true);
            }
        }
        // Messages from a branch that outranks ours waited long enough.
        let out = self.groups.check_forks();
        if !out.send.is_empty() {
            let _ = self.apply(node, out, true);
        }
        if !self.held.iter().any(|h| h.to == *peer) {
            return;
        }
        let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|h| h.to == *peer);
        self.held = rest;
        for h in mine {
            if node
                .send_tagged(&h.to, AppMessage::Group(h.bytes.clone()), h.tag)
                .is_err()
            {
                self.held.push(h);
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

    fn save_expected(&self) {
        let _ = self
            .home
            .save_state(&self.identity, EXPECTED, &encode_expected(&self.expected));
    }

    fn save_offered(&self) {
        let mut out = vec![VERSION];
        for (d, g) in &self.offered {
            out.extend_from_slice(d.as_bytes());
            out.extend_from_slice(g);
        }
        let _ = self.home.save_state(&self.identity, OFFERED, &out);
    }

    fn save_offers(&self) {
        let _ = self
            .home
            .save_state(&self.identity, OFFERS, &encode_offers(&self.offers));
    }

    fn save_refused(&self) {
        let _ = self
            .home
            .save_state(&self.identity, REFUSED, &encode_refused(&self.refused));
    }

    fn save_left(&self) {
        let mut out = vec![VERSION];
        for g in &self.left {
            out.extend_from_slice(g);
        }
        let _ = self.home.save_state(&self.identity, LEFT, &out);
    }

    fn save_owner_seen(&self) {
        let _ = self.home.save_state(
            &self.identity,
            OWNER_SEEN,
            &encode_owner_seen(&self.owner_seen),
        );
    }

    fn save_held(&self) {
        let _ = self
            .home
            .save_state(&self.identity, HELD, &encode_held(&self.held));
    }

    /// Persists, delivers `out`, and turns its events into updates.
    fn apply(&mut self, node: &Node, out: Output, may_forward: bool) -> Result<Vec<Update>> {
        self.apply_tagged(node, out, may_forward, Tag::NONE, None)
    }

    /// [`Self::apply`], sending the copies with delivery tag `t`, as
    /// `origin`'s if we forward them for another member.
    fn apply_tagged(
        &mut self,
        node: &Node,
        out: Output,
        may_forward: bool,
        t: Tag,
        origin: Option<PublicIdentity>,
    ) -> Result<Vec<Update>> {
        // Persist before anything leaves: a crash must not lose an epoch
        // that peers have already moved to. If it can't be saved, nothing
        // leaves, and we go back to what was saved, where peers still are.
        if let Err(e) = self.save()
            && !out.send.is_empty()
        {
            self.reload();
            return Err(e);
        }
        let held = self.held.len();
        // Whoever this welcomes doesn't know the group until the Welcome
        // is in: asked to forward anything before then, it would drop it.
        let joining: Vec<PublicIdentity> = out
            .send
            .iter()
            .filter(|o| matches!(o.wire, GroupWire::Welcome { .. }))
            .map(|o| o.to)
            .collect();
        for o in out.send {
            self.deliver_avoiding(node, o.to, o.wire, may_forward, t, &joining, origin);
        }
        if self.held.len() != held {
            self.save_held();
        }
        let mut updates = Vec::new();
        for e in out.events {
            self.event(node, e, &mut updates);
        }
        Ok(updates)
    }

    /// Goes back to the groups as last saved.
    fn reload(&mut self) {
        let saved = match self.home.load_state(&self.identity, STATE) {
            Ok(Some(b)) => Groups::restore(&self.identity, &b),
            Ok(None) => Ok(Groups::new(&self.identity)),
            Err(e) => Err(e.into()),
        };
        if let Ok(groups) = saved {
            self.groups = groups;
        }
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
        self.deliver_avoiding(node, to, wire, may_forward, t, &[], None);
    }

    /// [`Self::deliver`], never forwarding through those in `avoid`.
    #[allow(clippy::too_many_arguments)]
    fn deliver_avoiding(
        &mut self,
        node: &Node,
        to: PublicIdentity,
        wire: GroupWire,
        may_forward: bool,
        t: Tag,
        avoid: &[PublicIdentity],
        origin: Option<PublicIdentity>,
    ) {
        // A device removed from its account gets nothing more, by any path.
        if node.is_revoked(&to) {
            return;
        }
        let Ok(bytes) = wire.encode() else { return };
        let msg = AppMessage::Group(bytes.clone());
        if node.send_tagged(&to, msg.clone(), t).is_ok() {
            return;
        }
        // Should every mailbox decline it, the node keeps it for a session.
        if node.can_send_offline(&to) && node.send_offline_or_hold(&to, &msg, t).is_ok() {
            return;
        }
        if let (
            true,
            GroupWire::Message {
                group,
                message,
                proposals,
                sync,
            },
        ) = (may_forward, &wire)
            && let Some(via) = self.forwarder(node, group, &to, avoid)
        {
            // Our own message asks for a receipt; one we forward doesn't.
            let fwd = GroupWire::Forward {
                group: *group,
                to: *to.as_bytes(),
                message: message.clone(),
                reference: if t.relay_for.is_none() { t.local_id } else { 0 },
                proposals: proposals.clone(),
                sync: *sync,
            };
            if let Ok(b) = fwd.encode()
                && node.send(&via, AppMessage::Group(b)).is_ok()
            {
                return;
            }
        }
        // Hold it for when they connect, and try to make that happen.
        hold(&mut self.held, Held::new(to, bytes, t, origin));
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let node = node.clone();
            rt.spawn(async move {
                let _ = node.connect_relayed(to.fingerprint()).await;
            });
        }
    }

    /// A member other than `to` (and those in `avoid`) with a live
    /// session: the coordinator first, then the owner's other devices,
    /// then admins, then anyone.
    fn forwarder(
        &self,
        node: &Node,
        group: &GroupId,
        to: &PublicIdentity,
        avoid: &[PublicIdentity],
    ) -> Option<PublicIdentity> {
        let (_, _, lead, members) = self.groups.list().into_iter().find(|(g, ..)| g == group)?;
        let roster = self.groups.roster(group).ok()?;
        let me = node.identity();
        let live: Vec<PublicIdentity> = node.sessions().iter().map(|s| s.peer).collect();
        let candidates = members
            .into_iter()
            .filter(|m| m != to && *m != me && live.contains(m) && !avoid.contains(m));
        let mut candidates: Vec<_> = candidates.collect();
        candidates.sort_by_key(|m| (*m != lead, roster.role(m).to_wire()));
        candidates.into_iter().next()
    }

    /// The owner showed signs of life in `group` (`who` did something):
    /// the successor's clock starts over.
    fn seen_by(&mut self, group: &GroupId, who: Option<&PublicIdentity>) {
        if let Some(w) = who
            && self
                .groups
                .roster(group)
                .is_ok_and(|r| r.owners.contains(w))
        {
            self.stamp_owner_seen(group);
        }
    }

    fn stamp_owner_seen(&mut self, group: &GroupId) {
        let now = threnody_core::now_ms();
        match self.owner_seen.iter_mut().find(|(g, _)| g == group) {
            Some((_, t)) if now.saturating_sub(*t) < 60_000 => return,
            Some((_, t)) => *t = now,
            None => self.owner_seen.push((*group, now)),
        }
        self.save_owner_seen();
    }

    /// As the coordinator: a device `by` added is an owner if it's one of
    /// ours, or an admin if `by` is an admin and we can tell it's `by`'s
    /// own device (members can't, so it's ours to check).
    fn sibling_role(
        &mut self,
        node: &Node,
        group: &GroupId,
        device: &PublicIdentity,
        by: &PublicIdentity,
        updates: &mut Vec<Update>,
    ) {
        let me = node.identity();
        if !self.has_roster(group) || !self.groups.is_lead(group) {
            return;
        }
        let Ok(roster) = self.groups.roster(group) else {
            return;
        };
        let change = if node.is_own_device(device) && roster.role(device) != MemberRole::Owner {
            RosterChange::AddOwners(vec![*device])
        } else if roster.role(by) == MemberRole::Admin
            && *by != me
            && roster.role(device) == MemberRole::Member
            && node
                .account_of(by)
                .is_some_and(|a| a.state().has(by) && a.state().has(device))
        {
            RosterChange::Promote(vec![*device])
        } else {
            return;
        };
        if let Ok(out) = self.groups.change_roster(group, &[change])
            && let Ok(u) = self.apply(node, out, true)
        {
            updates.extend(u);
        }
    }

    /// As the coordinator: an admin device stepped down, so its account's
    /// other admin devices (if we know them) do too.
    fn mirror_step_down(
        &mut self,
        node: &Node,
        group: &GroupId,
        device: &PublicIdentity,
        updates: &mut Vec<Update>,
    ) {
        if !self.groups.is_lead(group) {
            return;
        }
        let Ok(roster) = self.groups.roster(group) else {
            return;
        };
        let Some(account) = node.account_of(device) else {
            return;
        };
        if !account.state().has(device) {
            return;
        }
        let others: Vec<PublicIdentity> = roster
            .admins
            .iter()
            .copied()
            .filter(|a| a != device && account.state().has(a))
            .collect();
        if others.is_empty() {
            return;
        }
        if let Ok(out) = self
            .groups
            .change_roster(group, &[RosterChange::Demote(others)])
            && let Ok(u) = self.apply(node, out, true)
        {
            updates.extend(u);
        }
    }

    fn event(&mut self, node: &Node, e: GroupEvent, updates: &mut Vec<Update>) {
        // What our reaction to it brings, shown after it.
        let mut extra = Vec::new();
        let update = match e {
            GroupEvent::Joined { group, name, owner } => {
                if self.expected.iter().any(|(g, _)| *g == group) {
                    self.expected.retain(|(g, _)| *g != group);
                    self.save_expected();
                }
                Update::Joined { group, name, owner }
            }
            GroupEvent::InviteRequested { group, name, peer } => {
                // Group traffic skips message requests, and can still come
                // from someone blocked (through a mailbox).
                if node.contacts().get(&peer).is_some_and(|c| c.blocked) {
                    return;
                }
                // Our own device offered us a group we left, and we said
                // we stay out: this is the invitation that set off, from
                // our device or the owner it named.
                let now = threnody_core::now_ms();
                let before = self.refused.len();
                self.refused
                    .retain(|(.., at)| now.saturating_sub(*at) < REFUSED_FOR_MS);
                let set_off = self.refused.iter().position(|(g, owner, _)| {
                    *g == group && (*owner == Some(peer) || node.is_own_device(&peer))
                });
                if let (true, Some(i)) = (self.left.contains(&group), set_off) {
                    self.refused.remove(i);
                    self.save_refused();
                    return;
                }
                if self.refused.len() != before {
                    self.save_refused();
                }
                // Mutually approved contacts are trusted to add us, and so
                // is the owner of a group our own device says to join;
                // anyone else needs explicit consent, and so does a group
                // we left.
                // And, when we asked to be brought back in step, an owner or
                // successor device of the group re-adding us.
                let repair = self.groups.out_of_step(&group)
                    && self
                        .groups
                        .roster(&group)
                        .is_ok_and(|r| r.owners.contains(&peer) || r.successors.contains(&peer));
                let trusted = repair
                    || !self.left.contains(&group)
                        && (node
                            .contacts()
                            .get(&peer)
                            .is_some_and(|c| c.mutually_approved())
                            || node.is_own_device(&peer)
                            || self.expected.contains(&(group, Some(peer))));
                // If our key package can't be kept and sent, the
                // invitation waits for consent like any other.
                if trusted
                    && let Ok(out) = self.groups.accept_invite(&group, peer)
                    && let Ok(u) = self.apply(node, out, true)
                {
                    updates.extend(u);
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
                // Anyone can ask: keep only the latest few of each, and
                // past the limit drop strangers' first, so a crowd of new
                // identities can't push out what someone we know sent.
                let theirs = self.invites.iter().filter(|i| i.from == peer).count();
                let mut extra = theirs.saturating_sub(MAX_INVITES_PER_PEER);
                self.invites.retain(|i| {
                    let drop = extra > 0 && i.from == peer;
                    extra -= usize::from(drop);
                    !drop
                });
                while self.invites.len() > MAX_INVITES {
                    let i = self
                        .invites
                        .iter()
                        .position(|i| !vouched(node, &self.expected, i))
                        .unwrap_or(0);
                    self.invites.remove(i);
                }
                self.save_invites();
                // Nor notify for each of them.
                let stranger = !vouched(node, &self.expected, &inv);
                let quiet = stranger
                    && self
                        .stranger_invited
                        .is_some_and(|t| t.elapsed() < STRANGER_INVITE_QUIET);
                if stranger && !quiet {
                    self.stranger_invited = Some(std::time::Instant::now());
                }
                Update::Invited { invite: inv, quiet }
            }
            GroupEvent::MemberAdded { group, member, by } => {
                self.seen_by(&group, by.as_ref());
                // One of our devices is in: should it leave, it stays out.
                if node.is_own_device(&member) {
                    self.settle(member, group);
                }
                if let Some(by) = by {
                    self.sibling_role(node, &group, &member, &by, &mut extra);
                }
                Update::MemberAdded { group, member, by }
            }
            GroupEvent::MemberRemoved { group, member, by } => {
                self.seen_by(&group, by.as_ref());
                // One of our devices left (or was removed): it stays out.
                if node.is_own_device(&member) && !self.offered.contains(&(member, group)) {
                    self.offered.push((member, group));
                    self.save_offered();
                }
                Update::MemberRemoved { group, member, by }
            }
            GroupEvent::Left { group } => {
                if self.owner_seen.iter().any(|(g, _)| *g == group) {
                    self.owner_seen.retain(|(g, _)| *g != group);
                    self.save_owner_seen();
                }
                Update::Left { group }
            }
            GroupEvent::Committed {
                group,
                by,
                successor,
            } => {
                // Any commit of the owner's, a role change say, is a sign
                // of life; becoming successor starts the clock afresh.
                if successor {
                    self.stamp_owner_seen(&group);
                } else {
                    self.seen_by(&group, Some(&by));
                }
                return;
            }
            GroupEvent::RoleChanged {
                group,
                member,
                role,
            } => {
                if role == MemberRole::Member {
                    self.mirror_step_down(node, &group, &member, &mut extra);
                }
                Update::RoleChanged {
                    group,
                    member,
                    role,
                }
            }
            GroupEvent::ChangeFailed { group, reason } => Update::ChangeFailed {
                group,
                reason: reason.to_owned(),
            },
            GroupEvent::UpgradePending { group, outdated } => {
                Update::UpgradePending { group, outdated }
            }
            GroupEvent::OutOfStep { group } => Update::OutOfStep { group },
            GroupEvent::Text {
                group,
                from,
                text,
                id,
                quote,
                mentions,
                forwarded,
            } => {
                self.seen_by(&group, Some(&from));
                // What our other devices send to the group is ours too.
                let ours = node.is_own_device(&from);
                let timer = node.effective_timer(ConversationId::Group(group));
                let me = node.reactor_of(&node.identity());
                let mine = |d: &[u8; 32]| {
                    *d == *node.identity().as_bytes()
                        || PublicIdentity::from_bytes(d).is_ok_and(|p| node.is_own_device(&p))
                };
                let mentioned = !ours
                    && (mentions.iter().any(|m| m.who == me || mine(&m.who))
                        || quote.as_ref().is_some_and(|q| mine(&q.author)));
                node.record_with_id(
                    ConversationId::Group(group),
                    *from.as_bytes(),
                    ours,
                    &text,
                    false,
                    timer,
                    id,
                    Extras {
                        quote,
                        mentions,
                        forwarded,
                    },
                );
                Update::Text {
                    group,
                    from,
                    text,
                    ours,
                    mentioned,
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
                forwarded,
            } => {
                self.seen_by(&group, Some(&from));
                Update::File {
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
                    forwarded,
                }
            }
            GroupEvent::React {
                group,
                from,
                id,
                emoji,
                add,
            } => {
                self.seen_by(&group, Some(&from));
                let who = node.reactor_of(&from);
                node.apply_reaction(ConversationId::Group(group), who, id, &emoji, add);
                Update::Reacted { group, from }
            }
            GroupEvent::Poll {
                group,
                from,
                id,
                question,
                options,
                multi,
            } => {
                let Some(poll) = Poll::new(options, multi) else {
                    return;
                };
                let conv = ConversationId::Group(group);
                let ours = node.is_own_device(&from);
                let now = threnody_core::now_ms();
                node.append(
                    conv,
                    Entry {
                        at_ms: now,
                        outgoing: ours,
                        device: *from.as_bytes(),
                        text: question.clone(),
                        offline: false,
                        expires_at_ms: node
                            .effective_timer(conv)
                            .map(|s| now + u64::from(s) * 1000),
                        file: None,
                        local_id: 0,
                        delivered: false,
                        recipients: 0,
                        delivered_to: Vec::new(),
                        remote_id: id,
                        edited_ms: 0,
                        read_ms: 0,
                        reactions: Vec::new(),
                        quote: None,
                        mentions: Vec::new(),
                        poll: Some(poll),
                        forwarded: false,
                        list: None,
                        ticked_ms: 0,
                    },
                );
                Update::Text {
                    group,
                    from,
                    text: format!("📊 {question}"),
                    ours,
                    mentioned: false,
                }
            }
            GroupEvent::Vote {
                group,
                from,
                id,
                choices,
            } => {
                self.seen_by(&group, Some(&from));
                let who = node.reactor_of(&from);
                if !node.vote_in(ConversationId::Group(group), id, who, &choices) {
                    return;
                }
                Update::Voted { group, from }
            }
            GroupEvent::Call { group, from, msg } => {
                if let Some(u) = self.on_call(node, group, from, &msg) {
                    updates.push(u);
                }
                return;
            }
            GroupEvent::List { group, from, list } => {
                // The node keeps it and tells the app (`Event::ListChanged`).
                if let Some((bytes, groups)) =
                    node.on_shared_list(ConversationId::Group(group), from, &list)
                {
                    for g in groups {
                        let _ = self.send_list(node, &g, bytes.clone());
                    }
                }
                return;
            }
            GroupEvent::ClosePoll { group, from, id } => {
                // Only its author (any device of their account) closes a poll.
                let who = node.reactor_of(&from);
                let author = |d: &[u8; 32]| {
                    *d == *from.as_bytes()
                        || PublicIdentity::from_bytes(d).is_ok_and(|p| node.reactor_of(&p) == who)
                };
                if !node.close_poll_in(ConversationId::Group(group), id, author) {
                    return;
                }
                Update::Voted { group, from }
            }
            GroupEvent::Pin {
                group,
                from,
                id,
                pin,
            } => {
                self.seen_by(&group, Some(&from));
                // Only the owner's and admins' pins count.
                if !self.may_pin(&group, &from)
                    || !node.pin_in(ConversationId::Group(group), id, pin)
                {
                    return;
                }
                Update::Pinned { group, from, pin }
            }
        };
        updates.push(update);
        updates.extend(extra);
    }
}

/// Adds `new` to `held`, then drops what is past [`MAX_HELD_PER_MEMBER`]
/// or [`MAX_HELD_BYTES_PER_MEMBER`] for its member, and past
/// [`MAX_HELD_BYTES`] in all. What goes is chosen by [`victim`], so a
/// member that floods us through `Forward` loses its own messages, and
/// the commits a member needs outlast the files sent to it.
fn hold(held: &mut Vec<Held>, new: Held) {
    let to = new.to;
    held.push(new);
    let mine = |h: &Held| h.to == to;
    let len = |h: &Held| h.bytes.len();
    trim(held, &mine, &|_| 1, MAX_HELD_PER_MEMBER);
    trim(held, &mine, &len, MAX_HELD_BYTES_PER_MEMBER);
    trim(held, &|_| true, &len, MAX_HELD_BYTES);
}

/// Drops [`victim`]s until what `scope` picks weighs at most `cap`.
fn trim(
    held: &mut Vec<Held>,
    scope: &dyn Fn(&Held) -> bool,
    weight: &dyn Fn(&Held) -> usize,
    cap: usize,
) {
    while held.iter().filter(|h| scope(h)).map(weight).sum::<usize>() > cap {
        let Some(i) = victim(held, scope, weight) else {
            break;
        };
        held.remove(i);
    }
}

/// Which of the held messages `scope` picks to drop: of those whose
/// origin has the most `weight` among them, the oldest that isn't a
/// handshake, or failing that the oldest handshake.
fn victim(
    held: &[Held],
    scope: &dyn Fn(&Held) -> bool,
    weight: &dyn Fn(&Held) -> usize,
) -> Option<usize> {
    let mut origins: Vec<(Option<PublicIdentity>, usize)> = Vec::new();
    for h in held.iter().filter(|h| scope(h)) {
        match origins.iter_mut().find(|(o, _)| *o == h.origin) {
            Some((_, w)) => *w += weight(h),
            None => origins.push((h.origin, weight(h))),
        }
    }
    // On a tie, someone else's before our own.
    let (origin, _) = origins.into_iter().max_by_key(|(o, w)| (*w, o.is_some()))?;
    let theirs = |h: &&Held| scope(h) && h.origin == origin;
    held.iter()
        .position(|h| theirs(&h) && !h.handshake)
        .or_else(|| held.iter().position(|h| theirs(&h)))
}

/// Whether `invite` comes from someone we accepted, or for a group our own
/// device is in from its owner (or anyone, if the device didn't say).
fn vouched(node: &Node, expected: &[(GroupId, Option<PublicIdentity>)], invite: &Invite) -> bool {
    node.contacts()
        .get(&invite.from)
        .is_some_and(|c| c.accepted)
        || expected
            .iter()
            .any(|(g, o)| *g == invite.group && o.is_none_or(|o| o == invite.from))
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
//   held    = HELD_VERSION || * ( member (32) | delivery tag (Tag::LEN) | origin (32, zeros if ours) | len (u32 LE) | GroupWire bytes )
//             (version 3 had no origin)
//   expected = EXPECTED_VERSION || * ( group (16) | owner (32, zeros if not known) )
//   owner-seen = VERSION || * ( group (16) | ms (u64 LE) )

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

fn encode_expected(expected: &[(GroupId, Option<PublicIdentity>)]) -> Vec<u8> {
    let mut out = vec![EXPECTED_VERSION];
    for (g, owner) in expected {
        out.extend_from_slice(g);
        out.extend_from_slice(owner.as_ref().map_or(&[0; 32], |o| o.as_bytes()));
    }
    out
}

fn decode_expected(b: &[u8]) -> Vec<(GroupId, Option<PublicIdentity>)> {
    match b.split_first() {
        Some((&EXPECTED_VERSION, rest)) => rest
            .as_chunks::<48>()
            .0
            .iter()
            .map(|c| {
                let (group, owner) = c.split_at(16);
                let owner = <&[u8; 32]>::try_from(owner)
                    .ok()
                    .filter(|o| **o != [0; 32])
                    .and_then(|o| PublicIdentity::from_bytes(o).ok());
                (group.try_into().unwrap_or_default(), owner)
            })
            .collect(),
        // Saved before owners were: those groups' invitations need consent.
        Some((&VERSION, rest)) => rest
            .as_chunks::<16>()
            .0
            .iter()
            .map(|g| (*g, None))
            .collect(),
        _ => Vec::new(),
    }
}

fn encode_owner_seen(seen: &[(GroupId, u64)]) -> Vec<u8> {
    let mut out = vec![VERSION];
    for (g, t) in seen {
        out.extend_from_slice(g);
        out.extend_from_slice(&t.to_le_bytes());
    }
    out
}

fn decode_owner_seen(b: &[u8]) -> Vec<(GroupId, u64)> {
    match b.split_first() {
        Some((&VERSION, rest)) => rest
            .as_chunks::<24>()
            .0
            .iter()
            .map(|c| {
                let (g, t) = c.split_at(16);
                (
                    g.try_into().unwrap_or_default(),
                    u64::from_le_bytes(t.try_into().unwrap_or_default()),
                )
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn decode_left(b: &[u8]) -> Vec<GroupId> {
    match b.split_first() {
        Some((&VERSION, rest)) => rest.as_chunks::<16>().0.to_vec(),
        _ => Vec::new(),
    }
}

fn decode_offered(b: &[u8]) -> Vec<(PublicIdentity, GroupId)> {
    let Some((&VERSION, rest)) = b.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<48>()
        .0
        .iter()
        .filter_map(|c| {
            let d = PublicIdentity::from_bytes(c[..32].try_into().ok()?).ok()?;
            Some((d, c[32..].try_into().ok()?))
        })
        .collect()
}

/// `group-offers` for a home that has none yet: up to 0.19.0,
/// `group-offered` also listed devices offered a group that never got the
/// offer (they were linked by a process that runs no groups), with no way
/// to tell them from those that left it. Each listed device not in the
/// group now is offered it once more: a stuck one joins, one that left
/// says it stays out (or, if older, asks its user once).
#[allow(clippy::type_complexity)]
fn migrate_offers(
    offered: &[(PublicIdentity, GroupId)],
    groups: &[(GroupId, String, PublicIdentity, Vec<PublicIdentity>)],
) -> Vec<Offer> {
    offered
        .iter()
        .filter(|(d, g)| {
            groups
                .iter()
                .any(|(x, .., members)| x == g && !members.contains(d))
        })
        .map(|&(device, group)| Offer {
            device,
            group,
            times: MAX_OFFERS - 1,
            last_ms: 0,
        })
        .collect()
}

fn encode_offers(offers: &[Offer]) -> Vec<u8> {
    let mut out = vec![VERSION];
    for o in offers {
        out.extend_from_slice(o.device.as_bytes());
        out.extend_from_slice(&o.group);
        out.push(o.times);
        out.extend_from_slice(&o.last_ms.to_le_bytes());
    }
    out
}

fn decode_offers(b: &[u8]) -> Vec<Offer> {
    let Some((&VERSION, rest)) = b.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<57>()
        .0
        .iter()
        .filter_map(|c| {
            Some(Offer {
                device: PublicIdentity::from_bytes(c[..32].try_into().ok()?).ok()?,
                group: c[32..48].try_into().ok()?,
                times: c[48],
                last_ms: u64::from_le_bytes(c[49..].try_into().ok()?),
            })
        })
        .collect()
}

fn encode_refused(refused: &[(GroupId, Option<PublicIdentity>, u64)]) -> Vec<u8> {
    let mut out = vec![VERSION];
    for (g, owner, at) in refused {
        out.extend_from_slice(g);
        out.push(u8::from(owner.is_some()));
        out.extend_from_slice(owner.as_ref().map_or(&[0; 32], |o| o.as_bytes()));
        out.extend_from_slice(&at.to_le_bytes());
    }
    out
}

fn decode_refused(b: &[u8]) -> Vec<(GroupId, Option<PublicIdentity>, u64)> {
    let Some((&VERSION, rest)) = b.split_first() else {
        return Vec::new();
    };
    rest.as_chunks::<57>()
        .0
        .iter()
        .filter_map(|c| {
            let owner = match c[16] {
                0 => None,
                _ => Some(PublicIdentity::from_bytes(c[17..49].try_into().ok()?).ok()?),
            };
            Some((
                c[..16].try_into().ok()?,
                owner,
                u64::from_le_bytes(c[49..].try_into().ok()?),
            ))
        })
        .collect()
}

fn encode_held(held: &[Held]) -> Vec<u8> {
    let mut out = vec![HELD_VERSION];
    for h in held {
        out.extend_from_slice(h.to.as_bytes());
        h.tag.encode(&mut out);
        out.extend_from_slice(&h.origin.map(|o| *o.as_bytes()).unwrap_or_default());
        out.extend_from_slice(
            &u32::try_from(h.bytes.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        out.extend_from_slice(&h.bytes);
    }
    out
}

fn decode_held(b: &[u8]) -> Vec<Held> {
    let mut out = Vec::new();
    let origins = match b.first() {
        Some(&HELD_VERSION) => true,
        Some(&HELD_VERSION_NO_ORIGIN) => false,
        _ => return out,
    };
    let mut rest = &b[1..];
    let n = 32 + Tag::LEN;
    let o = if origins { n + 32 } else { n };
    while let Some((head, r)) = rest.split_at_checked(o + 4) {
        let len = u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]) as usize;
        let Some((msg, r)) = r.split_at_checked(len) else {
            break;
        };
        rest = r;
        let peer: [u8; 32] = head[..32].try_into().unwrap_or_default();
        let origin: [u8; 32] = head[n..o].try_into().unwrap_or_default();
        let origin = (origin != [0; 32])
            .then(|| PublicIdentity::from_bytes(&origin).ok())
            .flatten();
        if let Ok(p) = PublicIdentity::from_bytes(&peer) {
            out.push(Held::new(
                p,
                msg.to_vec(),
                Tag::decode(&head[32..n]),
                origin,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(to: PublicIdentity, bytes: Vec<u8>, origin: Option<PublicIdentity>) -> Held {
        Held {
            to,
            bytes,
            tag: Tag::NONE,
            origin,
            handshake: false,
        }
    }

    #[test]
    fn what_is_held_is_bounded_oldest_first() {
        let (a, b) = (Identity::generate().public(), Identity::generate().public());
        let mut q = Vec::new();
        let mib = |n: u8| vec![n; 1 << 20];
        // By bytes for one member: the newest 32 MiB stay.
        for n in 0..40 {
            hold(&mut q, held(a, mib(n), None));
        }
        hold(&mut q, held(b, mib(0), None));
        let bytes = |q: &[Held], p| {
            q.iter()
                .filter(|h| h.to == p)
                .map(|h| h.bytes.len())
                .sum::<usize>()
        };
        assert_eq!(bytes(&q, a), MAX_HELD_BYTES_PER_MEMBER);
        assert_eq!(q[0].bytes[0], 8, "the oldest went first");
        // By count, as before.
        for _ in 0..MAX_HELD_PER_MEMBER + 5 {
            hold(&mut q, held(b, vec![1], None));
        }
        assert_eq!(q.iter().filter(|h| h.to == b).count(), MAX_HELD_PER_MEMBER);
        // And in all, across members.
        let mut q = Vec::new();
        for i in 0..10 {
            let p = Identity::generate().public();
            for _ in 0..20 {
                hold(&mut q, held(p, mib(i), None));
            }
        }
        let total: usize = q.iter().map(|h| h.bytes.len()).sum();
        assert_eq!(total, MAX_HELD_BYTES);
        assert_eq!(q.last().unwrap().bytes[0], 9, "the newest stay");
    }

    #[test]
    fn commits_and_others_messages_outlast_a_flood() {
        let (a, b, m) = (
            Identity::generate().public(),
            Identity::generate().public(),
            Identity::generate().public(),
        );
        let mib = |n: u8| vec![n; 1 << 20];
        let commit = |to, origin| Held {
            handshake: true,
            ..held(to, vec![7; 100], origin)
        };
        // The commit adding `a`, then more files for `a` than we keep, and
        // more small messages than we keep: the commit stays.
        let mut q = vec![commit(a, None)];
        for n in 0..40 {
            hold(&mut q, held(a, mib(n), None));
        }
        for _ in 0..MAX_HELD_PER_MEMBER + 5 {
            hold(&mut q, held(a, vec![1], None));
        }
        assert_eq!(q[0], commit(a, None));
        // A commit the owner had us forward to `b`, our own message to `b`,
        // then Mallory forwarding more than we keep in all: only hers go.
        let mut q = vec![commit(b, Some(a)), held(b, mib(1), None)];
        for _ in 0..20 {
            let p = Identity::generate().public();
            for n in 0..8 {
                hold(&mut q, held(p, mib(n), Some(m)));
            }
        }
        assert_eq!(q[..2], [commit(b, Some(a)), held(b, mib(1), None)]);
        assert!(q.iter().map(|h| h.bytes.len()).sum::<usize>() <= MAX_HELD_BYTES);
        // Likewise when she sends it all to `b`.
        let mut q = vec![commit(b, Some(a)), held(b, mib(1), None)];
        for n in 0..40 {
            hold(
                &mut q,
                Held {
                    handshake: true,
                    ..held(b, mib(n), Some(m))
                },
            );
        }
        assert_eq!(q[..2], [commit(b, Some(a)), held(b, mib(1), None)]);
    }

    #[test]
    fn commits_and_welcomes_count_as_handshakes() {
        let (a, b) = (Identity::generate(), Identity::generate());
        let (pa, pb) = (a.public(), b.public());
        let (mut ga, mut gb) = (Groups::new(&a), Groups::new(&b));
        let g = ga.create("team").unwrap();
        ga.invite(&g, pb).unwrap();
        let kp = gb.accept_invite(&g, pa).unwrap().send.remove(0).wire;
        let welcome = ga.handle(pb, kp).unwrap().send.remove(0).wire;
        gb.handle(pa, welcome.clone()).unwrap();
        let text = ga.send_text(&g, "hi").unwrap().send.remove(0).wire;
        let commit = ga.remove(&g, &pb).unwrap().send.remove(0).wire;
        let handshake =
            |w: GroupWire| Held::new(pb, w.encode().unwrap(), Tag::NONE, None).handshake;
        assert!(handshake(welcome) && handshake(commit) && !handshake(text));
    }

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
        let o = Identity::generate().public();
        let q = vec![
            Held {
                tag: t,
                ..held(p, vec![1, 2, 3], Some(o))
            },
            held(p, vec![], None),
        ];
        let b = encode_held(&q);
        assert_eq!(decode_held(&b), q);
        assert_eq!(decode_held(&b[..b.len() - 2]), q[..1]);
        // As saved before origins were.
        let old = [
            &[HELD_VERSION_NO_ORIGIN][..],
            p.as_bytes(),
            &[0; Tag::LEN],
            &1u32.to_le_bytes(),
            &[5],
        ]
        .concat();
        assert_eq!(decode_held(&old), [held(p, vec![5], None)]);
        for d in [&[][..], &[9, 0, 0]] {
            assert!(decode_invites(d).is_empty() && decode_held(d).is_empty());
        }
        // Expected groups, with and without an owner, and as first saved.
        let expected = vec![([1; 16], Some(p)), ([2; 16], None)];
        assert_eq!(decode_expected(&encode_expected(&expected)), expected);
        assert_eq!(
            decode_expected(&[&[VERSION][..], &[3; 16]].concat()),
            [([3; 16], None)]
        );
        // When each group's owner was last seen.
        let seen = vec![([1; 16], 5), ([2; 16], u64::MAX)];
        let b = encode_owner_seen(&seen);
        assert_eq!(decode_owner_seen(&b), seen);
        assert_eq!(decode_owner_seen(&b[..b.len() - 1]), seen[..1]);
        assert!(decode_owner_seen(&[9, 1]).is_empty());
        // Offered groups, in the layout 0.19.0 reads too.
        let q = Identity::generate().public();
        let file = [
            &[VERSION][..],
            q.as_bytes(),
            &[1; 16],
            q.as_bytes(),
            &[2; 16],
        ]
        .concat();
        let offered = decode_offered(&file);
        assert_eq!(offered, [(q, [1; 16]), (q, [2; 16])]);
        assert!(decode_offered(&[&[9][..], &file[1..]].concat()).is_empty());
        // A home from 0.19.0 or earlier: a device offered a group it isn't
        // in (group 2, say) is offered it once more; one in it, or offered
        // a group we're no longer in, isn't.
        let groups = vec![
            ([1; 16], "a".to_owned(), p, vec![p, q]),
            ([2; 16], "b".to_owned(), p, vec![p]),
        ];
        let migrated = migrate_offers(&[offered[0], offered[1], (q, [3; 16])], &groups);
        assert_eq!(
            migrated,
            [Offer {
                device: q,
                group: [2; 16],
                times: MAX_OFFERS - 1,
                last_ms: 0
            }]
        );
        assert!(migrated[0].due(threnody_core::now_ms()));
        // Offers in progress.
        let offers = vec![
            migrated[0],
            Offer {
                device: p,
                group: [4; 16],
                times: 1,
                last_ms: u64::MAX - 1,
            },
        ];
        let b = encode_offers(&offers);
        assert_eq!(decode_offers(&b), offers);
        assert_eq!(decode_offers(&b[..b.len() - 1]), offers[..1]);
        assert!(decode_offers(&[9]).is_empty());
        // Invitations to turn down, with and without the owner.
        let refused = vec![([1; 16], Some(q), 7), ([2; 16], None, u64::MAX)];
        let b = encode_refused(&refused);
        assert_eq!(decode_refused(&b), refused);
        assert_eq!(decode_refused(&b[..b.len() - 1]), refused[..1]);
        assert!(decode_refused(&[9]).is_empty());
    }

    #[test]
    fn a_sibling_is_offered_a_group_a_bounded_number_of_times() {
        let p = Identity::generate().public();
        let mut o = Offer {
            device: p,
            group: [1; 16],
            times: 0,
            last_ms: 0,
        };
        assert!(o.due(0));
        o.times = 1;
        o.last_ms = 1_000;
        assert!(!o.due(1_000 + OFFER_RETRY_MS - 1));
        assert!(o.due(1_000 + OFFER_RETRY_MS));
        o.times = MAX_OFFERS;
        assert!(!o.due(u64::MAX));
    }

    #[test]
    fn claim_allowed_after_fourteen_days() {
        let day = 24 * 60 * 60 * 1000;
        assert!(!claim_allowed(None, 100 * day, SUCCESSION_SILENCE_MS));
        assert!(!claim_allowed(Some(day), 14 * day, SUCCESSION_SILENCE_MS));
        assert!(claim_allowed(Some(day), 15 * day, SUCCESSION_SILENCE_MS));
        assert!(!claim_allowed(
            Some(20 * day),
            15 * day,
            SUCCESSION_SILENCE_MS
        ));
    }

    #[test]
    fn member_actions_follow_roles() {
        use MemberAction::*;
        let mut p: Vec<PublicIdentity> = (0..6).map(|_| Identity::generate().public()).collect();
        p.sort_by_key(|x| *x.as_bytes());
        let (o1, o2, a1, a2, m, s) = (p[0], p[1], p[2], p[3], p[4], p[5]);
        let mut r = Roster::legacy(o1);
        r = r
            .apply(&[
                RosterChange::AddOwners(vec![o2]),
                RosterChange::Promote(vec![a1, a2, s]),
                RosterChange::Successor(vec![s]),
            ])
            .unwrap();
        let ours = [o1, o2];
        // The coordinator, owner.
        assert_eq!(
            member_actions(&r, &o1, &ours, &[m]),
            [MakeAdmin, MakeOwner, Remove]
        );
        assert_eq!(
            member_actions(&r, &o1, &ours, &[a1]),
            [RemoveAdmin, MakeSuccessor, MakeOwner, Remove]
        );
        assert_eq!(
            member_actions(&r, &o1, &ours, &[s]),
            [RemoveAdmin, NoLongerSuccessor, MakeOwner, Remove]
        );
        assert!(member_actions(&r, &o1, &ours, &[o1, o2]).is_empty());
        // Our other owner device, not coordinating.
        assert_eq!(
            member_actions(&r, &o2, &ours, &[o1, o2]),
            [UseAsCoordinator, RemoveThisDevice]
        );
        // An admin.
        assert_eq!(member_actions(&r, &a1, &[a1], &[m]), [Remove]);
        assert!(member_actions(&r, &a1, &[a1], &[a2]).is_empty());
        assert!(member_actions(&r, &a1, &[a1], &[o1, o2]).is_empty());
        assert_eq!(member_actions(&r, &a1, &[a1], &[a1]), [StepDown]);
        // A member.
        for row in [&[o1, o2][..], &[a1], &[m], &[s]] {
            assert!(member_actions(&r, &m, &[m], row).is_empty());
        }
    }

    #[test]
    fn banner_priority() {
        let p = Identity::generate().public();
        let all = banner(true, Some(p), Some((2, p)), &[p], true);
        assert_eq!(all, Some(Banner::OutOfStep));
        assert_eq!(
            banner(false, Some(p), Some((2, p)), &[p], true),
            Some(Banner::CanClaim { owner: p })
        );
        assert_eq!(
            banner(false, None, Some((2, p)), &[p], true),
            Some(Banner::PendingChanges {
                count: 2,
                coordinator: p
            })
        );
        assert_eq!(
            banner(false, None, Some((0, p)), &[p], true),
            Some(Banner::UpgradePending { outdated: vec![p] })
        );
        assert_eq!(banner(false, None, None, &[], true), Some(Banner::OnlyYou));
        assert_eq!(banner(false, None, None, &[], false), None);
    }
}
