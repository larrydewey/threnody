//! MLS groups for Threnody (spec §6.2) on openmls.
//!
//! - Ciphersuite `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519`: X-Wing
//!   HPKE, so group key agreement is post-quantum hybrid like the 1:1
//!   protocol; Ed25519 signatures with each device's Threnody identity key.
//! - Every member's credential must be its own signature key, so an MLS
//!   member *is* a Threnody identity; anything else is rejected.
//! - There is no server. Group messages travel as [`GroupWire`] inside the
//!   1:1 ratchets, fanned out by the sender to every other member.
//! - Membership policy (spec §6.2 "application layer"): one device commits,
//!   the *coordinator*. In a group created before roles, that is its
//!   creator. Once its owner first changes a role, a [`Roster`] in the MLS
//!   GroupContext names the owner's devices, the admins, an optional
//!   successor and the coordinator (one of the owner's devices). Admins and
//!   the owner's other devices express changes as MLS proposals to the
//!   coordinator, which commits them at once; every member checks every
//!   commit against the roster before it. Concurrent changes therefore
//!   never fork the group without a delivery service; only an explicit
//!   takeover or claim commits otherwise, bumping the roster's term, and a
//!   member left on a losing branch asks to be re-added.
//!
//! The manager is sans-IO: callers feed it messages and deliver what it
//! returns. See `docs/appendix-f-groups.md`.

#[cfg(feature = "node")]
pub mod node;
pub mod roster;
pub mod wire;

use std::collections::HashMap;

use openmls::ciphersuite::hash_ref::ProposalRef;
use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::{Signer, SignerError};
use openmls_traits::storage::StorageProvider as _;
use threnody_core::crypto::random_bytes;
use threnody_core::message::{Clip, Mention, Quote};
use threnody_core::{Identity, PublicIdentity};

use roster::{
    CommitProposal, ProposalKind, ROSTER_EXTENSION, authorize_proposal, check_commit,
    proposed_roster, roster_extensions, roster_from,
};
pub use roster::{Roster, RosterChange};
pub use wire::{Content, GroupId, GroupWire, SyncHint};

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519;
const STATE_VERSION: u8 = 1;
/// How many past epochs' application messages can still be decrypted.
const MAX_PAST_EPOCHS: usize = 5;
/// Group names are display labels; keep them short.
pub const MAX_NAME: usize = 64;
/// Key packages kept for invitations we accepted that no Welcome has
/// answered yet; past this the oldest are deleted.
pub const MAX_KEY_PACKAGES: usize = 16;
/// Times a change of ours is proposed again after commits that didn't
/// include it, before we give up on it.
pub const MAX_INTENT_ATTEMPTS: u8 = 8;
/// Proposals the coordinator keeps for one epoch, in all and from one
/// proposer; past these it drops them.
pub const MAX_STORED_PROPOSALS: usize = 32;
pub const MAX_PROPOSALS_PER_PROPOSER: usize = 8;
/// Messages for a later epoch than ours, kept until the commit that gets
/// us there: per group, by count and bytes. The oldest go first.
pub const MAX_FUTURE: usize = 64;
pub const MAX_FUTURE_BYTES: usize = 2 << 20;

type Pid = PublicIdentity;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("no such group")]
    UnknownGroup,
    #[error("only the owner or an admin can do that")]
    NotOwner,
    #[error("{0} is not a member of this group")]
    NotMember(String),
    #[error("{0} is already a member of this group")]
    AlreadyMember(String),
    #[error("credential is not a Threnody identity")]
    BadCredential,
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    /// A rule of the group's roles forbids it ("admins can't remove
    /// admins", "transfer ownership first", ...).
    #[error("{0}")]
    NotAllowed(&'static str),
    /// These invitees (fingerprints) run a version of Threnody without
    /// group roles, which a group with admins needs.
    #[error("{} must update Threnody to join a group with admins", .0.join(", "))]
    NeedsUpdate(Vec<String>),
    #[error("mls: {0}")]
    Mls(String),
    #[error(transparent)]
    Core(#[from] threnody_core::Error),
    /// The file couldn't be prepared to send (too large, or a damaged
    /// image whose metadata couldn't be removed).
    #[error("{0}")]
    File(String),
    /// A group call couldn't start or be joined (already in a call, say).
    #[error("{0}")]
    Call(String),
}

pub(crate) fn mls<E: std::fmt::Debug>(e: E) -> GroupError {
    GroupError::Mls(format!("{e:?}"))
}

pub type Result<T> = core::result::Result<T, GroupError>;

/// Signs with the device's Threnody identity key; the seed never leaves
/// [`Identity`].
struct IdentitySigner(Identity);

impl Signer for IdentitySigner {
    fn sign(&self, payload: &[u8]) -> core::result::Result<Vec<u8>, SignerError> {
        Ok(self.0.sign(payload).to_vec())
    }

    fn signature_scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// A message to deliver to one peer over its 1:1 session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    pub to: PublicIdentity,
    pub wire: GroupWire,
}

/// Something the user interface should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupEvent {
    Joined {
        group: GroupId,
        name: String,
        owner: PublicIdentity,
    },
    /// `peer` asked for a key package to add us to `name`. Answer with
    /// [`Groups::accept_invite`] to consent.
    InviteRequested {
        group: GroupId,
        name: String,
        peer: PublicIdentity,
    },
    /// `by`: whoever added them (`None` if not known).
    MemberAdded {
        group: GroupId,
        member: PublicIdentity,
        by: Option<PublicIdentity>,
    },
    /// `by`: whoever removed them (themselves, if they left).
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
    ChangeFailed {
        group: GroupId,
        reason: &'static str,
    },
    /// Roles wait for these members to update Threnody.
    UpgradePending {
        group: GroupId,
        outdated: Vec<PublicIdentity>,
    },
    /// This device fell out of step with the group and asked to be
    /// brought back.
    OutOfStep { group: GroupId },
    /// We were removed, or the group was otherwise closed for us.
    Left { group: GroupId },
    /// `id` is the sender's id for the message (0 from older members).
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
        id: u64,
        /// The message it replies to.
        quote: Option<Quote>,
        /// Who it mentions.
        mentions: Vec<Mention>,
        forwarded: bool,
    },
    File {
        group: GroupId,
        from: PublicIdentity,
        name: String,
        data: Vec<u8>,
        sensitive: bool,
        caption: String,
        album: u64,
        id: u64,
        clip: Option<Clip>,
        forwarded: bool,
    },
    /// `from` added (or took away) its `emoji` on message `id`.
    React {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
        emoji: String,
        add: bool,
    },
    /// `from` pinned (or unpinned) message `id`.
    Pin {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
        pin: bool,
    },
    /// `from` asks the group a question.
    Poll {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
        question: String,
        options: Vec<String>,
        multi: bool,
    },
    /// `from` voted in poll `id` (no choices: took its vote back).
    Vote {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
        choices: Vec<u32>,
    },
    /// `from` closed its poll `id`.
    ClosePoll {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
    },
    /// `from` shared (or changed) a list in the group.
    List {
        group: GroupId,
        from: PublicIdentity,
        list: Vec<u8>,
    },
    /// Group call signalling (`threnody_core::call::GroupCallMsg`).
    Call {
        group: GroupId,
        from: PublicIdentity,
        msg: Vec<u8>,
    },
}

/// Results of handling one input.
#[derive(Debug, Default)]
pub struct Output {
    pub send: Vec<Outgoing>,
    pub events: Vec<GroupEvent>,
}

impl Output {
    fn extend(&mut self, other: Output) {
        self.send.extend(other.send);
        self.events.extend(other.events);
    }
}

/// Member role in a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
}

impl MemberRole {
    /// Owners and admins invite and remove members, and pin.
    pub fn can_manage_members(self) -> bool {
        matches!(self, MemberRole::Owner | MemberRole::Admin)
    }

    /// Only owners promote, demote and hand over the group.
    pub fn can_manage_roles(self) -> bool {
        matches!(self, MemberRole::Owner)
    }

    /// Wire format: 0=Owner, 1=Admin, 2=Member
    pub fn to_wire(self) -> u8 {
        match self {
            MemberRole::Owner => 0,
            MemberRole::Admin => 1,
            MemberRole::Member => 2,
        }
    }

    pub fn from_wire(v: u8) -> Self {
        match v {
            0 => MemberRole::Owner,
            1 => MemberRole::Admin,
            _ => MemberRole::Member,
        }
    }
}

/// A change of ours that hasn't landed yet; proposed again after each
/// commit that doesn't include it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    /// `key_package`: the invitee's, as an encoded `MlsMessageOut`.
    Add {
        member: Pid,
        key_package: Vec<u8>,
    },
    Remove {
        member: Pid,
    },
    Roster(Vec<RosterChange>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Intent {
    change: Change,
    attempts: u8,
}

/// A proposal the coordinator stored this epoch, to attach to its commit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Attached {
    reference: Vec<u8>,
    message: Vec<u8>,
    proposer: Pid,
}

/// Internal group state.
struct Group {
    mls: MlsGroup,
    name: String,
    /// Who committed before the group had a roster: its creator, or for a
    /// joiner whoever made the Welcome. With a roster, kept as its lead.
    owner: PublicIdentity,
    intents: Vec<Intent>,
    attachable: Vec<Attached>,
    /// Messages for a later epoch than ours: who sent them, and the
    /// encoded `GroupWire`.
    future: Vec<(Pid, Vec<u8>)>,
    /// The epoch in which we last proposed upgrading our leaf.
    upgrade_epoch: Option<u64>,
    /// When we last asked to be brought back in step: (epoch, term).
    resync: Option<(u64, u32)>,
}

impl Group {
    fn new(mls: MlsGroup, name: String, owner: PublicIdentity) -> Self {
        Self {
            mls,
            name,
            owner,
            intents: Vec::new(),
            attachable: Vec::new(),
            future: Vec::new(),
            upgrade_epoch: None,
            resync: None,
        }
    }

    fn members(&self) -> Vec<Pid> {
        self.mls
            .members()
            .filter_map(|m| identity_of(&m.credential, &m.signature_key).ok())
            .collect()
    }

    fn index_of(&self, who: &Pid) -> Option<LeafNodeIndex> {
        self.mls
            .members()
            .find(|m| identity_of(&m.credential, &m.signature_key).ok().as_ref() == Some(who))
            .map(|m| m.index)
    }

    fn at(&self, index: LeafNodeIndex) -> Option<Pid> {
        self.mls
            .member_at(index)
            .and_then(|m| identity_of(&m.credential, &m.signature_key).ok())
    }

    fn sender(&self, sender: &Sender) -> Option<Pid> {
        match sender {
            Sender::Member(i) => self.at(*i),
            _ => None,
        }
    }

    /// The roster, if the group has one.
    fn roster(&self) -> Result<Option<Roster>> {
        roster_from(self.mls.extensions()).transpose()
    }

    fn roster_or_legacy(&self) -> Result<Roster> {
        Ok(self.roster()?.unwrap_or_else(|| Roster::legacy(self.owner)))
    }

    /// The coordinator.
    fn lead(&self) -> Pid {
        self.roster_or_legacy()
            .map(|r| r.lead)
            .unwrap_or(self.owner)
    }

    fn auth(&self) -> [u8; 8] {
        let mut out = [0; 8];
        out.copy_from_slice(&self.mls.epoch_authenticator().as_slice()[..8]);
        out
    }

    /// Our sync hint, in a group with a roster.
    fn sync(&self) -> Option<SyncHint> {
        let r = self.roster().ok()??;
        Some(SyncHint {
            epoch: self.mls.epoch().as_u64(),
            term: r.term,
            auth: self.auth(),
            lead: roster::lead_prefix(&r.lead),
        })
    }

    fn leaf_capable(&self, index: LeafNodeIndex) -> bool {
        self.mls.public_group().leaf(index).is_some_and(capable)
    }

    /// Members whose leaf doesn't advertise the roster.
    fn outdated(&self) -> Vec<Pid> {
        self.mls
            .members()
            .filter(|m| !self.leaf_capable(m.index))
            .filter_map(|m| identity_of(&m.credential, &m.signature_key).ok())
            .collect()
    }

    fn own_leaf_capable(&self) -> bool {
        self.leaf_capable(self.mls.own_leaf_index())
    }

    fn proposal_kind(&self, p: &Proposal) -> ProposalKind {
        match p {
            Proposal::Add(_) => ProposalKind::Add,
            Proposal::Remove(r) => self
                .at(r.removed())
                .map_or(ProposalKind::Other, ProposalKind::Remove),
            Proposal::Update(_) => ProposalKind::Update,
            Proposal::GroupContextExtensions(e) => {
                proposed_roster(e.extensions()).map_or(ProposalKind::Other, ProposalKind::Roster)
            }
            _ => ProposalKind::Other,
        }
    }

    /// Who proposed each proposal of a commit, what it is, and whether by
    /// reference; `committer` made those by value.
    fn commit_proposals<'a>(
        &self,
        committer: &Pid,
        queued: impl Iterator<Item = &'a QueuedProposal>,
    ) -> Result<Vec<CommitProposal>> {
        queued
            .map(|q| {
                let by_ref = q.proposal_or_ref_type() == ProposalOrRefType::Reference;
                let who = if by_ref {
                    self.sender(q.sender())
                        .ok_or(GroupError::Unexpected("proposal from a non-member"))?
                } else {
                    *committer
                };
                Ok((who, self.proposal_kind(q.proposal()), by_ref))
            })
            .collect()
    }
}

/// Whether a leaf advertises the roster capability.
fn capable(leaf: &LeafNode) -> bool {
    leaf.capabilities()
        .extensions()
        .contains(&ExtensionType::Unknown(ROSTER_EXTENSION))
}

/// Our leaves' capabilities: the defaults (which include our ciphersuite)
/// plus the roster extension, the signal that we enforce roster rules.
fn capabilities() -> Capabilities {
    Capabilities::builder()
        .extensions(vec![ExtensionType::Unknown(ROSTER_EXTENSION)])
        .build()
}

/// Who joined and left in a commit, each with who brought it about.
#[allow(clippy::type_complexity)]
fn membership_changes<'a>(
    g: &Group,
    committer: &Pid,
    queued: impl Iterator<Item = &'a QueuedProposal>,
) -> (Vec<(Pid, Pid)>, Vec<(Pid, Pid)>) {
    let (mut added, mut removed) = (Vec::new(), Vec::new());
    for q in queued {
        let by = if q.proposal_or_ref_type() == ProposalOrRefType::Reference {
            g.sender(q.sender()).unwrap_or(*committer)
        } else {
            *committer
        };
        match q.proposal() {
            Proposal::Add(a) => {
                let leaf = a.key_package().leaf_node();
                if let Ok(p) = identity_of(leaf.credential(), leaf.signature_key().as_slice()) {
                    added.push((p, by));
                }
            }
            Proposal::Remove(r) => {
                if let Some(p) = g.at(r.removed()) {
                    removed.push((p, by));
                }
            }
            _ => {}
        }
    }
    // A repair removes and re-adds one device: not news.
    let both: Vec<Pid> = added
        .iter()
        .filter(|(p, _)| removed.iter().any(|(r, _)| r == p))
        .map(|(p, _)| *p)
        .collect();
    added.retain(|(p, _)| !both.contains(p));
    removed.retain(|(p, _)| !both.contains(p));
    (added, removed)
}

/// What the coordinator (or a takeover or claim) commits of its own.
#[derive(Default)]
struct Own {
    /// Invitees we asked ourselves: their Welcome goes straight to them.
    adds: Vec<(Pid, KeyPackage)>,
    removes: Vec<Pid>,
    roster: Vec<RosterChange>,
    /// The roster after, instead of applying `roster` (a claim).
    set_roster: Option<Roster>,
    /// Reference the proposals others sent us.
    consume: bool,
    /// Update our own leaf to advertise the roster.
    self_update: bool,
    /// Install the first roster in a group without one.
    install: bool,
}

impl Own {
    fn is_empty(&self) -> bool {
        self.adds.is_empty()
            && self.removes.is_empty()
            && self.roster.is_empty()
            && self.set_roster.is_none()
            && !self.self_update
            && !self.install
    }
}

pub struct Groups {
    provider: OpenMlsRustCrypto,
    signer: IdentitySigner,
    me: PublicIdentity,
    groups: HashMap<GroupId, Group>,
    /// Invitations we sent and are waiting on a key package for.
    pending_adds: Vec<(GroupId, PublicIdentity)>,
    /// Key packages we gave out and are waiting on a Welcome for, oldest
    /// first: the group, who asked, and where the key package is stored.
    key_packages: Vec<(GroupId, PublicIdentity, KeyPackageRef)>,
    /// Members we re-added (or are re-adding) to repair a fork, and at
    /// which term: once each per term.
    repairs: Vec<(GroupId, PublicIdentity, u32)>,
    /// Members whose request to be brought back we passed on to our
    /// coordinator.
    relays: Vec<(GroupId, PublicIdentity)>,
    /// Behaves as a version without roles (tests only).
    legacy: bool,
    /// Working through the future-epoch buffer.
    draining: bool,
}

fn credential_for(id: &PublicIdentity) -> CredentialWithKey {
    CredentialWithKey {
        credential: BasicCredential::new(id.as_bytes().to_vec()).into(),
        signature_key: id.as_bytes().to_vec().into(),
    }
}

/// Maps a member credential to a Threnody identity, requiring that the
/// credential's identity is exactly its signature key.
fn identity_of(credential: &Credential, signature_key: &[u8]) -> Result<PublicIdentity> {
    let basic =
        BasicCredential::try_from(credential.clone()).map_err(|_| GroupError::BadCredential)?;
    let bytes: [u8; 32] = basic
        .identity()
        .try_into()
        .map_err(|_| GroupError::BadCredential)?;
    if signature_key != bytes {
        return Err(GroupError::BadCredential);
    }
    PublicIdentity::from_bytes(&bytes).map_err(|_| GroupError::BadCredential)
}

fn parse_key_package(provider: &OpenMlsRustCrypto, bytes: &[u8]) -> Result<KeyPackage> {
    let msg = MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?;
    let MlsMessageBodyIn::KeyPackage(kp_in) = msg.extract() else {
        return Err(GroupError::Unexpected("not a key package"));
    };
    kp_in
        .validate(provider.crypto(), ProtocolVersion::Mls10)
        .map_err(mls)
}

// Local group state, beside openmls's storage (see `export`):
//   4: [* [group_id, { 0: [* Intent], 1: [* [ref, mls, proposer]], 2: [* [from, GroupWire]],
//                      ? 3: upgrade_epoch, ? 4: [epoch, term] }]]
//   5: [* [group_id, member]]          ; pending_adds
//   6: [* [group_id, member, term]]    ; repairs
//   Intent = [0, member, key_package, attempts] / [1, member, attempts]
//          / [2, [* [kind, [* device]]], attempts]

fn encode_change(
    e: &mut const_cbor::Encoder<'_>,
    c: &RosterChange,
) -> core::result::Result<(), const_cbor::Error> {
    let d = c.devices();
    e.array_len(2)?.u8(c.kind())?.array_len(d.len())?;
    for p in &d {
        e.bytes(p.as_bytes())?;
    }
    Ok(())
}

fn decode_pid(d: &mut const_cbor::Decoder<'_>) -> threnody_core::Result<Pid> {
    Pid::from_bytes(&threnody_core::cbor::fixed_bytes::<32>(d)?)
}

impl Groups {
    pub fn new(identity: &Identity) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: IdentitySigner(Identity::from_seed(&identity.seed())),
            me: identity.public(),
            groups: HashMap::new(),
            pending_adds: Vec::new(),
            key_packages: Vec::new(),
            repairs: Vec::new(),
            relays: Vec::new(),
            legacy: false,
            draining: false,
        }
    }

    /// A manager that behaves like a version of Threnody without group
    /// roles: its leaves don't advertise the roster, it never proposes and
    /// it takes commits only from the group's creator. For tests of
    /// mixed-version groups.
    #[doc(hidden)]
    pub fn legacy_for_tests(identity: &Identity) -> Self {
        Self {
            legacy: true,
            ..Self::new(identity)
        }
    }

    /// Serializes every group (openmls key-value store plus our metadata)
    /// for [`Groups::restore`]. Contains secrets: store it encrypted
    /// (`Home::save_state`).
    pub fn export(&self) -> Result<Vec<u8>> {
        let values = self
            .provider
            .storage()
            .values
            .read()
            .map_err(|_| GroupError::Mls("storage lock poisoned".into()))?;
        let size: usize = values
            .iter()
            .map(|(k, v)| k.len() + v.len() + 16)
            .sum::<usize>()
            + 256;
        let key_packages = self
            .key_packages
            .iter()
            .map(|(g, p, kp)| Ok((g, p, kp.tls_serialize_detached().map_err(mls)?)))
            .collect::<Result<Vec<_>>>()?;
        let local: usize = self
            .groups
            .values()
            .map(|g| {
                g.future.iter().map(|(_, b)| b.len() + 48).sum::<usize>()
                    + g.attachable
                        .iter()
                        .map(|a| a.message.len() + a.reference.len() + 48)
                        .sum::<usize>()
                    + g.intents
                        .iter()
                        .map(|i| match &i.change {
                            Change::Add { key_package, .. } => key_package.len() + 48,
                            Change::Remove { .. } => 48,
                            Change::Roster(c) => {
                                c.iter().map(|c| c.devices().len() * 36 + 8).sum::<usize>() + 8
                            }
                        })
                        .sum::<usize>()
                    + 96
            })
            .sum();
        let size = size
            + key_packages.len() * 128
            + local
            + (self.pending_adds.len() + self.repairs.len()) * 64
            + self.groups.len() * 64;
        let mut rosters = HashMap::new();
        for (id, g) in &self.groups {
            rosters.insert(*id, g.roster().ok().flatten());
        }
        Ok(threnody_core::cbor::to_vec(size, |e| {
            e.map_len(7)?;
            e.u8(0)?.u8(STATE_VERSION)?;
            e.u8(1)?.array_len(values.len())?;
            for (k, v) in values.iter() {
                e.array_len(2)?.bytes(k)?.bytes(v)?;
            }
            // As older versions read them: the committer as `owner`, and
            // the roles of the roster.
            e.u8(2)?.array_len(self.groups.len())?;
            for (id, g) in &self.groups {
                let roster = rosters.get(id).cloned().flatten();
                let owner = roster.as_ref().map_or(g.owner, |r| r.lead);
                let roles: Vec<(Pid, u8)> = roster
                    .as_ref()
                    .map(|r| {
                        r.owners
                            .iter()
                            .map(|p| (*p, 0))
                            .chain(r.admins.iter().map(|p| (*p, 1)))
                            .collect()
                    })
                    .unwrap_or_else(|| vec![(g.owner, 0)]);
                e.array_len(4)?
                    .bytes(id)?
                    .str(&g.name)?
                    .bytes(owner.as_bytes())?
                    .map_len(roles.len())?;
                for (member, role) in &roles {
                    e.bytes(member.as_bytes())?.u8(*role)?;
                }
            }
            e.u8(3)?.array_len(key_packages.len())?;
            for (group, peer, kp) in &key_packages {
                e.array_len(3)?
                    .bytes(*group)?
                    .bytes(peer.as_bytes())?
                    .bytes(kp)?;
            }
            e.u8(4)?.array_len(self.groups.len())?;
            for (id, g) in &self.groups {
                e.array_len(2)?.bytes(id)?;
                e.map_len(
                    3 + usize::from(g.upgrade_epoch.is_some()) + usize::from(g.resync.is_some()),
                )?;
                e.u8(0)?.array_len(g.intents.len())?;
                for i in &g.intents {
                    match &i.change {
                        Change::Add {
                            member,
                            key_package,
                        } => {
                            e.array_len(4)?
                                .u8(0)?
                                .bytes(member.as_bytes())?
                                .bytes(key_package)?
                                .u8(i.attempts)?;
                        }
                        Change::Remove { member } => {
                            e.array_len(3)?
                                .u8(1)?
                                .bytes(member.as_bytes())?
                                .u8(i.attempts)?;
                        }
                        Change::Roster(changes) => {
                            e.array_len(3)?.u8(2)?.array_len(changes.len())?;
                            for c in changes {
                                encode_change(e, c)?;
                            }
                            e.u8(i.attempts)?;
                        }
                    }
                }
                e.u8(1)?.array_len(g.attachable.len())?;
                for a in &g.attachable {
                    e.array_len(3)?
                        .bytes(&a.reference)?
                        .bytes(&a.message)?
                        .bytes(a.proposer.as_bytes())?;
                }
                e.u8(2)?.array_len(g.future.len())?;
                for (from, w) in &g.future {
                    e.array_len(2)?.bytes(from.as_bytes())?.bytes(w)?;
                }
                if let Some(u) = g.upgrade_epoch {
                    e.u8(3)?.u64(u)?;
                }
                if let Some((epoch, term)) = g.resync {
                    e.u8(4)?.array_len(2)?.u64(epoch)?.u32(term)?;
                }
            }
            e.u8(5)?.array_len(self.pending_adds.len())?;
            for (g, p) in &self.pending_adds {
                e.array_len(2)?.bytes(g)?.bytes(p.as_bytes())?;
            }
            e.u8(6)?.array_len(self.repairs.len())?;
            for (g, p, term) in &self.repairs {
                e.array_len(3)?.bytes(g)?.bytes(p.as_bytes())?.u32(*term)?;
            }
            Ok(())
        })?)
    }

    /// Rebuilds the manager from [`Groups::export`] output.
    pub fn restore(identity: &Identity, bytes: &[u8]) -> Result<Self> {
        use threnody_core::Error as E;
        use threnody_core::cbor::{fixed_bytes, read_map};
        let mut me = Self::new(identity);
        let mut dec = const_cbor::Decoder::new(bytes);
        let mut meta: Vec<(GroupId, String, [u8; 32])> = Vec::new();
        let mut version = None;
        let mut key_packages = Vec::new();
        let mut local: HashMap<GroupId, Local> = HashMap::new();
        let (mut pending_adds, mut repairs) = (Vec::new(), Vec::new());
        {
            let mut values = me
                .provider
                .storage()
                .values
                .write()
                .map_err(|_| GroupError::Mls("storage lock poisoned".into()))?;
            read_map(&mut dec, |k, d| {
                match k {
                    0 => version = Some(d.u8()?),
                    1 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 2 {
                                return Err(E::Malformed("storage entry"));
                            }
                            let key = d.bytes()?.to_vec();
                            values.insert(key, d.bytes()?.to_vec());
                        }
                    }
                    2 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 4 {
                                return Err(E::Malformed("group entry"));
                            }
                            let id = fixed_bytes::<16>(d)?;
                            let name = d.str()?.to_owned();
                            let owner = fixed_bytes::<32>(d)?;
                            // Roles live in the roster now: skip the map.
                            d.skip()?;
                            meta.push((id, name, owner));
                        }
                    }
                    3 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 3 {
                                return Err(E::Malformed("key package entry"));
                            }
                            let group = fixed_bytes::<16>(d)?;
                            let peer = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?)?;
                            let kp = KeyPackageRef::tls_deserialize_exact(d.bytes()?)
                                .map_err(|_| E::Malformed("key package ref"))?;
                            key_packages.push((group, peer, kp));
                        }
                    }
                    4 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 2 {
                                return Err(E::Malformed("group local state"));
                            }
                            let id = fixed_bytes::<16>(d)?;
                            local.insert(id, decode_local(d)?);
                        }
                    }
                    5 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 2 {
                                return Err(E::Malformed("pending add"));
                            }
                            pending_adds.push((fixed_bytes::<16>(d)?, decode_pid(d)?));
                        }
                    }
                    6 => {
                        for _ in 0..d.array_len()? {
                            if d.array_len()? != 3 {
                                return Err(E::Malformed("repair"));
                            }
                            repairs.push((fixed_bytes::<16>(d)?, decode_pid(d)?, d.u32()?));
                        }
                    }
                    _ => return Ok(false),
                }
                Ok(true)
            })?;
        }
        threnody_core::cbor::finish(&dec)?;
        if version != Some(STATE_VERSION) {
            return Err(GroupError::Unexpected("group state version"));
        }
        for (id, name, owner) in meta {
            let mls = MlsGroup::load(
                me.provider.storage(),
                &openmls::group::GroupId::from_slice(&id),
            )
            .map_err(mls)?
            .ok_or(GroupError::Unexpected("group missing from storage"))?;
            let owner = PublicIdentity::from_bytes(&owner)?;
            let mut group = Group::new(mls, name, owner);
            if let Some(l) = local.remove(&id) {
                group.intents = l.intents;
                group.attachable = l.attachable;
                group.future = l.future;
                group.upgrade_epoch = l.upgrade_epoch;
                group.resync = l.resync;
            }
            me.groups.insert(id, group);
        }
        me.key_packages = key_packages;
        me.pending_adds = pending_adds;
        me.repairs = repairs;
        Ok(me)
    }

    /// `(id, name, owner, members)` for every group we are in. `owner` is
    /// the coordinator: the device that commits.
    pub fn list(&self) -> Vec<(GroupId, String, PublicIdentity, Vec<PublicIdentity>)> {
        self.groups
            .iter()
            .map(|(id, g)| (*id, g.name.clone(), g.lead(), g.members()))
            .collect()
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
        self.groups
            .iter()
            .map(|(id, g)| {
                let members = g.members();
                let roster = g.roster_or_legacy().ok();
                let roles = members
                    .iter()
                    .map(|m| roster.as_ref().map_or(MemberRole::Member, |r| r.role(m)))
                    .collect();
                (*id, g.name.clone(), g.lead(), members, roles)
            })
            .collect()
    }

    fn group(&self, id: &GroupId) -> Result<&Group> {
        self.groups.get(id).ok_or(GroupError::UnknownGroup)
    }

    fn group_mut(&mut self, id: &GroupId) -> Result<&mut Group> {
        self.groups.get_mut(id).ok_or(GroupError::UnknownGroup)
    }

    /// The group's roster; for a group without one, its creator owns and
    /// coordinates it alone.
    pub fn roster(&self, group: &GroupId) -> Result<Roster> {
        self.group(group)?.roster_or_legacy()
    }

    /// Whether the group has a roster (roles agreed by every member).
    pub fn has_roster(&self, group: &GroupId) -> Result<bool> {
        Ok(self.group(group)?.roster()?.is_some())
    }

    pub fn role_of(&self, group: &GroupId, who: &PublicIdentity) -> Result<MemberRole> {
        Ok(self.roster(group)?.role(who))
    }

    /// Our changes still waiting for the coordinator.
    pub fn pending_changes(&self, group: &GroupId) -> usize {
        self.groups.get(group).map_or(0, |g| g.intents.len())
    }

    /// Members whose version doesn't support roles yet.
    pub fn outdated(&self, group: &GroupId) -> Result<Vec<PublicIdentity>> {
        Ok(self.group(group)?.outdated())
    }

    /// Whether we are the group's coordinator.
    pub fn is_lead(&self, group: &GroupId) -> bool {
        self.groups.get(group).is_some_and(|g| g.lead() == self.me)
    }

    /// Whether we asked to be brought back in step and haven't been yet.
    pub fn out_of_step(&self, group: &GroupId) -> bool {
        self.groups.get(group).is_some_and(|g| g.resync.is_some())
    }

    /// The group's epoch and epoch authenticator: equal on two members
    /// exactly when they share the epoch's secrets.
    #[doc(hidden)]
    pub fn epoch_state(&self, group: &GroupId) -> Option<(u64, Vec<u8>)> {
        self.groups.get(group).map(|g| {
            (
                g.mls.epoch().as_u64(),
                g.mls.epoch_authenticator().as_slice().to_vec(),
            )
        })
    }

    /// Proposals waiting in the group's MLS proposal store.
    #[doc(hidden)]
    pub fn stored_proposals(&self, group: &GroupId) -> usize {
        self.groups
            .get(group)
            .map_or(0, |g| g.mls.pending_proposals().count())
    }

    /// Messages waiting for a later epoch.
    #[doc(hidden)]
    pub fn buffered(&self, group: &GroupId) -> usize {
        self.groups.get(group).map_or(0, |g| g.future.len())
    }

    /// Creates a group owned by us.
    pub fn create(&mut self, name: &str) -> Result<GroupId> {
        let id: GroupId = random_bytes();
        let mut config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY);
        if !self.legacy {
            config = config.capabilities(capabilities());
        }
        let mls = MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &config.build(),
            openmls::group::GroupId::from_slice(&id),
            credential_for(&self.me),
        )
        .map_err(mls)?;
        self.groups
            .insert(id, Group::new(mls, truncate(name), self.me));
        Ok(id)
    }

    /// Owner/Admin: asks `peer` for a key package; the add completes when it
    /// arrives (through the coordinator, if that isn't us).
    pub fn invite(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<Output> {
        let g = self.group(group)?;
        if !g.roster_or_legacy()?.role(&self.me).can_manage_members() {
            return Err(GroupError::NotOwner);
        }
        if g.members().contains(&peer) {
            return Err(GroupError::AlreadyMember(peer.fingerprint().to_string()));
        }
        let name = g.name.clone();
        if !self.pending_adds.contains(&(*group, peer)) {
            self.pending_adds.push((*group, peer));
        }
        Ok(Output {
            send: vec![Outgoing {
                to: peer,
                wire: GroupWire::KeyPackageRequest {
                    group: *group,
                    name,
                },
            }],
            events: vec![],
        })
    }

    /// Consents to an invitation: sends `peer` a key package for it.
    pub fn accept_invite(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<Output> {
        // The inviter may ask more than once before the add completes, and
        // adds us with whichever answer reaches it first, so every answer
        // to the same invitation is the same key package.
        let unanswered = self
            .key_packages
            .iter()
            .find(|(g, p, _)| g == group && *p == peer)
            .map(|(.., kp)| kp.clone());
        let stored = match unanswered {
            Some(kp) => self
                .provider
                .storage()
                .key_package::<_, KeyPackageBundle>(&kp)
                .map_err(mls)?,
            None => None,
        };
        let key_package = match stored {
            Some(bundle) if bundle.key_package().life_time().validate().is_ok() => {
                bundle.key_package().clone()
            }
            _ => self.new_key_package(group, peer)?,
        };
        let key_package = MlsMessageOut::from(key_package)
            .tls_serialize_detached()
            .map_err(mls)?;
        Ok(Output {
            send: vec![Outgoing {
                to: peer,
                wire: GroupWire::KeyPackage {
                    group: *group,
                    key_package,
                },
            }],
            events: vec![],
        })
    }

    /// Makes and stores a key package for `peer`'s invitation to `group`.
    fn new_key_package(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<KeyPackage> {
        let mut builder = KeyPackage::builder();
        if !self.legacy {
            builder = builder.leaf_node_capabilities(capabilities());
        }
        let bundle = builder
            .build(
                CIPHERSUITE,
                &self.provider,
                &self.signer,
                credential_for(&self.me),
            )
            .map_err(mls)?;
        // Each stays stored, private keys and all, until a Welcome uses
        // it: keep one per invitation, and only the latest few.
        let kp = bundle
            .key_package()
            .hash_ref(self.provider.crypto())
            .map_err(mls)?;
        self.drop_key_packages(|(g, p, _)| g == group && *p == peer)?;
        self.key_packages.push((*group, peer, kp));
        let extra = self.key_packages.len().saturating_sub(MAX_KEY_PACKAGES);
        for (.., kp) in self.key_packages.drain(..extra).collect::<Vec<_>>() {
            self.provider
                .storage()
                .delete_key_package(&kp)
                .map_err(mls)?;
        }
        Ok(bundle.key_package().clone())
    }

    /// Deletes the key packages `which` picks, which no Welcome will need.
    fn drop_key_packages(
        &mut self,
        which: impl Fn(&(GroupId, PublicIdentity, KeyPackageRef)) -> bool,
    ) -> Result<()> {
        let (gone, kept) = std::mem::take(&mut self.key_packages)
            .into_iter()
            .partition(which);
        self.key_packages = kept;
        for (.., kp) in gone {
            self.provider
                .storage()
                .delete_key_package(&kp)
                .map_err(mls)?;
        }
        Ok(())
    }

    /// Owner/Admin: removes `member`. The coordinator commits it; anyone
    /// else proposes it to the coordinator. Admins remove members only.
    pub fn remove(&mut self, group: &GroupId, member: &PublicIdentity) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        let roster = g.roster_or_legacy()?;
        let role = roster.role(&me);
        if !role.can_manage_members() {
            return Err(GroupError::NotOwner);
        }
        if *member == me {
            return Err(GroupError::NotAllowed("leave the group instead"));
        }
        if !g.members().contains(member) {
            return Err(GroupError::NotMember(member.fingerprint().to_string()));
        }
        if roster.lead == me {
            let consume = g.roster()?.is_some();
            return self.commit(
                group,
                Own {
                    removes: vec![*member],
                    consume,
                    ..Own::default()
                },
            );
        }
        authorize_proposal(&roster, &me, &ProposalKind::Remove(*member))?;
        self.propose(group, Change::Remove { member: *member })
    }

    /// Changes roles. The coordinator commits; the owner's other devices
    /// propose; an admin may only step down (`Demote([itself])`). In a group
    /// without a roster, the first change installs one once every member
    /// runs a version with roles (until then, [`GroupEvent::UpgradePending`]).
    pub fn change_roster(&mut self, group: &GroupId, changes: &[RosterChange]) -> Result<Output> {
        let me = self.me;
        if self.legacy {
            return Err(GroupError::Unexpected("roles need a newer version"));
        }
        let g = self.group(group)?;
        let members = g.members();
        let Some(roster) = g.roster()? else {
            if g.owner != me {
                return Err(GroupError::NotOwner);
            }
            Roster::legacy(me).apply(changes)?.validate(&members)?;
            let g = self.group_mut(group)?;
            g.intents.push(Intent {
                change: Change::Roster(changes.to_vec()),
                attempts: 0,
            });
            return self.install(group);
        };
        if roster.update_needed() {
            return Err(GroupError::Unexpected(
                "update Threnody to change roles here",
            ));
        }
        let after = roster.apply(changes)?;
        after.validate(&members)?;
        match roster.role(&me) {
            MemberRole::Owner if roster.lead == me => self.commit(
                group,
                Own {
                    roster: changes.to_vec(),
                    consume: true,
                    ..Own::default()
                },
            ),
            MemberRole::Owner => self.propose(group, Change::Roster(changes.to_vec())),
            MemberRole::Admin if changes == [RosterChange::Demote(vec![me])] => {
                self.propose(group, Change::Roster(changes.to_vec()))
            }
            MemberRole::Admin => Err(GroupError::NotAllowed("admins can only step down")),
            MemberRole::Member => Err(GroupError::NotOwner),
        }
    }

    /// Owner: promotes `member` to Admin.
    pub fn promote(&mut self, group: &GroupId, member: PublicIdentity) -> Result<Output> {
        let roster = self.roster(group)?;
        match roster.role(&member) {
            MemberRole::Owner => return Err(GroupError::NotAllowed("cannot promote owner")),
            MemberRole::Admin => return Err(GroupError::NotAllowed("already an admin")),
            MemberRole::Member => {}
        }
        self.change_roster(group, &[RosterChange::Promote(vec![member])])
    }

    /// Owner: demotes `member` from Admin to Member; an admin demoting
    /// itself steps down.
    pub fn demote(&mut self, group: &GroupId, member: PublicIdentity) -> Result<Output> {
        if self.roster(group)?.role(&member) != MemberRole::Admin {
            return Err(GroupError::NotAllowed("only admins can be demoted"));
        }
        self.change_roster(group, &[RosterChange::Demote(vec![member])])
    }

    /// An owner device that isn't the coordinator becomes it, committing
    /// directly (without the coordinator) along with our pending changes.
    /// Only for when the coordinator is gone or unreachable: should it
    /// commit meanwhile, the group forks until repaired.
    pub fn take_over(&mut self, group: &GroupId) -> Result<Output> {
        self.take_over_removing(group, &[])
    }

    /// [`Self::take_over`], also removing `removes` (a lost coordinator,
    /// say) in the same commit.
    pub fn take_over_removing(&mut self, group: &GroupId, removes: &[Pid]) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        let Some(roster) = g.roster()? else {
            return Err(GroupError::NotAllowed(
                "only the owner coordinates this group",
            ));
        };
        if roster.role(&me) != MemberRole::Owner {
            return Err(GroupError::NotAllowed("only an owner device can take over"));
        }
        let members = g.members();
        let mut own = self.own_from_intents(group)?;
        if roster.lead == me {
            if removes.is_empty() && own.is_empty() {
                return Ok(Output::default());
            }
            own.consume = true;
        } else {
            own.roster.insert(0, RosterChange::Lead(me));
        }
        for r in removes {
            if members.contains(r) && !own.removes.contains(r) && *r != me {
                own.removes.push(*r);
            }
        }
        self.commit(group, own)
    }

    /// The successor claims ownership: its devices own the group from now
    /// on, the old owners stay as admins. The 14 days of silence the apps
    /// wait for are client policy (`node::GroupNode::can_claim`).
    pub fn claim(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let roster = self.roster(group)?;
        if !roster.successors.contains(&me) {
            return Err(GroupError::NotAllowed("only the successor can take over"));
        }
        self.commit(
            group,
            Own {
                set_roster: Some(roster.claimed_by(me)),
                ..Own::default()
            },
        )
    }

    /// Leaves a group: asks the coordinator to remove us (it commits the
    /// removal for everyone) and forgets the group here at once. The
    /// coordinator first hands over to another owner device; the last owner
    /// device must transfer ownership or delete the group instead.
    pub fn leave(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        let roster = g.roster()?;
        let lead = g.lead();
        let mut out = Output::default();
        if lead == me {
            let next = roster.as_ref().and_then(|r| {
                let members = g.members();
                r.owners
                    .iter()
                    .copied()
                    .filter(|o| *o != me && members.contains(o))
                    .min_by_key(|o| *o.as_bytes())
            });
            let Some(next) = next else {
                return Err(GroupError::NotAllowed(
                    "transfer ownership first, or delete the group",
                ));
            };
            out = self.commit(
                group,
                Own {
                    roster: vec![RosterChange::Lead(next)],
                    consume: true,
                    ..Own::default()
                },
            )?;
        }
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let lead = g.lead();
        let proposal = if g.roster()?.is_some() {
            let msg = g
                .mls
                .leave_group(&self.provider, &self.signer)
                .map_err(mls)?;
            Some(msg.tls_serialize_detached().map_err(mls)?)
        } else {
            None
        };
        self.forget(group)?;
        out.send.push(Outgoing {
            to: lead,
            wire: GroupWire::Leave {
                group: *group,
                proposal,
            },
        });
        out.events.push(GroupEvent::Left { group: *group });
        Ok(out)
    }

    /// Owner: deletes the group: one commit removes every other member
    /// (each sees `Left`), then we forget it. An owner device that isn't
    /// the coordinator takes over to do it.
    pub fn disband(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        let roster = g.roster_or_legacy()?;
        if roster.role(&me) != MemberRole::Owner {
            return Err(GroupError::NotOwner);
        }
        let others: Vec<Pid> = g.members().into_iter().filter(|p| *p != me).collect();
        let mut out = Output::default();
        if !others.is_empty() {
            let own = Own {
                removes: others,
                roster: if roster.lead == me {
                    Vec::new()
                } else {
                    vec![RosterChange::Lead(me)]
                },
                ..Own::default()
            };
            out = self.commit(group, own)?;
            // The removed see `Left`; nothing more for us to show.
            out.events.clear();
        }
        self.forget(group)?;
        out.events.push(GroupEvent::Left { group: *group });
        Ok(out)
    }

    /// Picks up where we left off after a restart: the coordinator commits
    /// proposals it stored but hadn't committed; roles waiting for an
    /// upgrade are installed if they can be; members propose their upgrade.
    pub fn resume(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        let mut out = Output::default();
        if g.lead() == me {
            if g.roster()?.is_some() {
                let own = self.own_from_intents(group)?;
                let g = self.group(group)?;
                if g.mls.pending_proposals().next().is_some() || !own.is_empty() {
                    out.extend(self.commit(
                        group,
                        Own {
                            consume: true,
                            ..own
                        },
                    )?);
                }
            } else if g
                .intents
                .iter()
                .any(|i| matches!(i.change, Change::Roster(_)))
            {
                out.extend(self.install(group)?);
            } else {
                out.extend(self.commit_upgrade_if_ready(group)?);
            }
        } else {
            out.extend(self.propose_upgrade(group, true)?);
        }
        Ok(out)
    }

    /// Drops a group and its MLS secrets from our state.
    fn forget(&mut self, group: &GroupId) -> Result<()> {
        if let Some(mut g) = self.groups.remove(group) {
            let storage = self.provider.storage();
            // A commit that removed us moved the group on an epoch without
            // us, so our leaf keys are still filed under the one before,
            // which `delete` doesn't look at.
            if let Some(prev) = g.mls.epoch().as_u64().checked_sub(1) {
                storage
                    .delete_encryption_epoch_key_pairs(
                        g.mls.group_id(),
                        &GroupEpoch::from(prev),
                        g.mls.own_leaf_index().u32(),
                    )
                    .map_err(mls)?;
            }
            g.mls.delete(storage).map_err(mls)?;
        }
        self.pending_adds.retain(|(g, _)| g != group);
        self.repairs.retain(|(g, ..)| g != group);
        self.relays.retain(|(g, _)| g != group);
        Ok(())
    }

    /// Encrypts `text` for the group and fans it out to every other member.
    pub fn send_text(&mut self, group: &GroupId, text: &str) -> Result<Output> {
        self.send(
            group,
            &Content::Text {
                text: text.to_owned(),
                id: 0,
                quote: None,
                mentions: Vec::new(),
                forwarded: false,
            },
        )
    }

    /// Encrypts `content` for the group and fans it out to every other
    /// member. Proposals of ours still waiting for the coordinator don't
    /// hold it up (openmls would refuse while any are pending).
    pub fn send(&mut self, group: &GroupId, content: &Content) -> Result<Output> {
        let me = self.me;
        let plain = content.encode()?;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let recipients = g.members();
        let signer = &self.signer;
        let out = with_proposals_aside(&self.provider, &mut g.mls, |group_mls, provider| {
            group_mls
                .create_message(provider, signer, &plain)
                .map_err(mls)
        })?;
        let message = out.tls_serialize_detached().map_err(mls)?;
        let sync = g.sync();
        Ok(Output {
            send: recipients
                .into_iter()
                .filter(|p| *p != me)
                .map(|to| Outgoing {
                    to,
                    wire: GroupWire::Message {
                        group: *group,
                        message: message.clone(),
                        proposals: Vec::new(),
                        sync,
                    },
                })
                .collect(),
            events: vec![],
        })
    }

    /// Handles a group message received from authenticated peer `from`.
    pub fn handle(&mut self, from: PublicIdentity, wire: GroupWire) -> Result<Output> {
        match wire {
            GroupWire::KeyPackageRequest { group, name } => Ok(Output {
                send: vec![],
                events: vec![GroupEvent::InviteRequested {
                    group,
                    name: truncate(&name),
                    peer: from,
                }],
            }),
            GroupWire::KeyPackage { group, key_package } => {
                self.on_key_package(from, group, &key_package)
            }
            GroupWire::Welcome {
                group,
                name,
                welcome,
            } => self.on_welcome(from, group, &name, &welcome),
            GroupWire::Message {
                group,
                message,
                proposals,
                sync,
            } => self.on_message(from, group, message, proposals, sync),
            GroupWire::Forward {
                group,
                to,
                message,
                proposals,
                sync,
                ..
            } => self.on_forward(from, group, to, message, proposals, sync),
            // Delivery bookkeeping, not group state: see `node::GroupNode`.
            GroupWire::Receipt { .. } | GroupWire::AddDevice { .. } => Ok(Output::default()),
            GroupWire::Leave { group, proposal } => self.on_leave(from, group, proposal),
            GroupWire::PassWelcome {
                group,
                member,
                welcome,
                ..
            } => self.on_pass_welcome(from, group, member, welcome),
            GroupWire::Resync {
                group,
                member,
                sync,
            } => self.on_resync(from, group, member, sync),
        }
    }

    /// Passes a member's message on to another member it couldn't reach.
    /// Both must be in the group as we know it; the result goes out as a
    /// plain `Message`, never forwarded again.
    fn on_forward(
        &self,
        from: PublicIdentity,
        group: GroupId,
        to: [u8; 32],
        message: Vec<u8>,
        proposals: Vec<Vec<u8>>,
        sync: Option<SyncHint>,
    ) -> Result<Output> {
        let to = PublicIdentity::from_bytes(&to)?;
        let members = self.group(&group)?.members();
        if !members.contains(&from) {
            return Err(GroupError::NotMember(from.fingerprint().to_string()));
        }
        if to == self.me || to == from || !members.contains(&to) {
            return Err(GroupError::NotMember(to.fingerprint().to_string()));
        }
        Ok(Output {
            send: vec![Outgoing {
                to,
                wire: GroupWire::Message {
                    group,
                    message,
                    proposals,
                    sync,
                },
            }],
            events: vec![],
        })
    }

    fn on_key_package(
        &mut self,
        from: PublicIdentity,
        group: GroupId,
        bytes: &[u8],
    ) -> Result<Output> {
        // Only complete adds we asked for, for the peer we asked.
        let Some(pos) = self.pending_adds.iter().position(|p| *p == (group, from)) else {
            return Err(GroupError::Unexpected("unsolicited key package"));
        };
        let kp = parse_key_package(&self.provider, bytes)?;
        let leaf = kp.leaf_node();
        if identity_of(leaf.credential(), leaf.signature_key().as_slice())? != from {
            return Err(GroupError::BadCredential);
        }
        if kp.ciphersuite() != CIPHERSUITE {
            return Err(GroupError::Unexpected("key package ciphersuite"));
        }
        let g = self.group(&group)?;
        let has_roster = g.roster()?.is_some();
        let lead = g.lead();
        // A member we're bringing back in step: removed and re-added in one
        // commit (as openmls's fork resolution does).
        let repair = g.members().contains(&from);
        self.pending_adds.remove(pos);
        if has_roster && !capable(kp.leaf_node()) {
            return Err(GroupError::NeedsUpdate(vec![
                from.fingerprint().to_string(),
            ]));
        }
        if lead == self.me {
            return self.commit(
                &group,
                Own {
                    adds: vec![(from, kp)],
                    removes: if repair { vec![from] } else { Vec::new() },
                    consume: has_roster && !repair,
                    ..Own::default()
                },
            );
        }
        if repair {
            return Ok(Output::default());
        }
        self.propose(
            &group,
            Change::Add {
                member: from,
                key_package: bytes.to_vec(),
            },
        )
    }

    fn on_welcome(
        &mut self,
        from: PublicIdentity,
        group: GroupId,
        name: &str,
        bytes: &[u8],
    ) -> Result<Output> {
        let rejoin = self.groups.contains_key(&group);
        if rejoin && self.legacy {
            return Err(GroupError::Unexpected("already in group"));
        }
        // Only from someone whose invitation to this group we accepted;
        // anyone else's must not use up the key package.
        if !self
            .key_packages
            .iter()
            .any(|(g, p, _)| *g == group && *p == from)
        {
            return Err(GroupError::Unexpected(if rejoin {
                "already in group"
            } else {
                "welcome we didn't ask for"
            }));
        }
        let msg = MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?;
        let MlsMessageBodyIn::Welcome(welcome) = msg.extract() else {
            return Err(GroupError::Unexpected("not a welcome"));
        };
        let config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .build();
        // A group we were removed from before its secrets were dropped on
        // removal, or one we're brought back into, is replaced once the
        // Welcome checks out.
        let staged = StagedWelcome::build_from_welcome(&self.provider, &config, welcome)
            .map_err(mls)?
            .replace_old_group()
            .build()
            .map_err(mls)?;
        if staged.group_context().group_id().as_slice() != group {
            return Err(GroupError::Unexpected("group id mismatch"));
        }
        if staged.group_context().ciphersuite() != CIPHERSUITE {
            return Err(GroupError::Unexpected("group ciphersuite"));
        }
        // Every member must be a Threnody identity, and both we and the
        // one who passed it to us among them.
        let members: Vec<PublicIdentity> = staged
            .members()
            .map(|m| identity_of(&m.credential, &m.signature_key))
            .collect::<Result<_>>()?;
        if !members.contains(&from) || !members.contains(&self.me) {
            return Err(GroupError::Unexpected("welcome from non-member"));
        }
        // ...and who made the Welcome: the group's coordinator.
        let sender = staged.welcome_sender().map_err(mls)?;
        let sender = identity_of(sender.credential(), sender.signature_key().as_slice())?;
        let roster = if self.legacy {
            None
        } else {
            roster_from(staged.group_context().extensions()).transpose()?
        };
        let owner = match &roster {
            None if sender != from => {
                return Err(GroupError::Unexpected("welcome made by someone else"));
            }
            None => sender,
            Some(r) => {
                if !r.update_needed() {
                    r.validate(&members)?;
                }
                if sender != r.lead {
                    return Err(GroupError::Unexpected(
                        "welcome not made by the coordinator",
                    ));
                }
                // Passed on by whoever invited us, who must be able to.
                if from != sender && !r.role(&from).can_manage_members() {
                    return Err(GroupError::Unexpected("welcome passed on by a member"));
                }
                r.lead
            }
        };
        let epoch = staged.group_context().epoch().as_u64();
        if let Some(cur) = self.groups.get(&group) {
            // Brought back in step: only by an owner or successor device of
            // the group as we know it, into a branch that outranks ours, or
            // by our own coordinator into a later epoch. Anyone else could
            // swap the group for a look-alike under the same id.
            let before = cur.roster_or_legacy()?;
            let trusted = before.owners.contains(&sender) || before.successors.contains(&sender);
            let outranks = roster.as_ref().is_some_and(|r| r.rank() > before.rank());
            let later = sender == before.lead && epoch > cur.mls.epoch().as_u64();
            if !trusted || !(outranks || later) {
                return Err(GroupError::Unexpected("already in group"));
            }
        }
        let name = match self.groups.remove(&group) {
            Some(mut old) => {
                old.mls.delete(self.provider.storage()).map_err(mls)?;
                old.name
            }
            None => {
                let stale = openmls::group::GroupId::from_slice(&group);
                if let Some(mut old) =
                    MlsGroup::load(self.provider.storage(), &stale).map_err(mls)?
                {
                    old.delete(self.provider.storage()).map_err(mls)?;
                }
                truncate(name)
            }
        };
        let mls = staged.into_group(&self.provider).map_err(mls)?;
        // The Welcome used one; any others for this group are spare.
        self.drop_key_packages(|(g, ..)| *g == group)?;
        self.groups
            .insert(group, Group::new(mls, name.clone(), owner));
        let mut out = Output {
            send: vec![],
            events: vec![GroupEvent::Joined { group, name, owner }],
        };
        self.after_merge(&group, &mut out);
        Ok(out)
    }

    fn on_message(
        &mut self,
        from: PublicIdentity,
        group: GroupId,
        bytes: Vec<u8>,
        proposals: Vec<Vec<u8>>,
        sync: Option<SyncHint>,
    ) -> Result<Output> {
        let legacy = self.legacy;
        let g = self.group(&group)?;
        let msg = MlsMessageIn::tls_deserialize_exact(&bytes).map_err(mls)?;
        let protocol = msg
            .try_into_protocol_message()
            .map_err(|_| GroupError::Unexpected("not a protocol message"))?;
        if protocol.group_id().as_slice() != group {
            return Err(GroupError::Unexpected("group id mismatch"));
        }
        let epoch = protocol.epoch().as_u64();
        let ours = g.mls.epoch().as_u64();
        // A sender on a branch that outranks ours, at an epoch we're past
        // or at ours with other secrets: we're on the losing side of a
        // fork. (Later epochs wait below, as reordering looks alike.)
        if let (Some(h), Some(r)) = (sync, g.roster()?)
            && !legacy
            && h.rank() > r.rank()
            && h.epoch <= ours
            && (h.epoch < ours || h.auth != g.auth())
        {
            return self.ask_resync(from, &group);
        }
        if epoch > ours {
            self.buffer(
                &group,
                from,
                GroupWire::Message {
                    group,
                    message: bytes,
                    proposals,
                    sync,
                },
            );
            return Ok(Output::default());
        }
        let is_proposal = protocol.content_type() == ContentType::Proposal;
        if epoch < ours && is_proposal {
            // Held or mailboxed across a commit: its proposer proposes it
            // again.
            return Ok(Output::default());
        }
        let g = self
            .groups
            .get_mut(&group)
            .ok_or(GroupError::UnknownGroup)?;
        // The proposals a commit references come with it.
        let snapshot: Option<Vec<QueuedProposal>> =
            (!legacy && !proposals.is_empty() && epoch == ours).then(|| {
                let snap = g.mls.pending_proposals().cloned().collect();
                for p in &proposals {
                    let Ok(m) = MlsMessageIn::tls_deserialize_exact(p) else {
                        continue;
                    };
                    let Ok(pm) = m.try_into_protocol_message() else {
                        continue;
                    };
                    if let Ok(processed) = g.mls.process_message(&self.provider, pm)
                        && let ProcessedMessageContent::ProposalMessage(q) =
                            processed.into_content()
                    {
                        let _ = g.mls.store_pending_proposal(self.provider.storage(), *q);
                    }
                }
                snap
            });
        let processed = match g.mls.process_message(&self.provider, protocol) {
            Ok(p) => p,
            Err(e) => {
                restore_proposals(&self.provider, &mut g.mls, snapshot);
                return Err(mls(e));
            }
        };
        // The sender's credential must belong to a current member whose
        // signature key matches it (checked against the ratchet tree).
        let cred = processed.credential().clone();
        let found = g
            .mls
            .members()
            .find(|m| m.credential == cred)
            .ok_or(GroupError::BadCredential)
            .and_then(|m| identity_of(&m.credential, &m.signature_key));
        let sender = match found {
            Ok(s) => s,
            Err(e) => {
                restore_proposals(&self.provider, &mut g.mls, snapshot);
                return Err(e);
            }
        };
        let event = match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                application_event(group, sender, &app.into_bytes())?
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                return self.on_commit(&group, sender, *staged, snapshot);
            }
            ProcessedMessageContent::ProposalMessage(q) => {
                return self.on_proposal(&group, sender, *q, bytes);
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_) => {
                return Err(GroupError::Unexpected("proposals are not used"));
            }
            ProcessedMessageContent::OwnPendingCommit
            | ProcessedMessageContent::OwnPrivateMessage => {
                return Err(GroupError::Unexpected("own message echoed back"));
            }
        };
        Ok(Output {
            send: vec![],
            events: vec![event],
        })
    }

    /// Keeps a message for a later epoch than ours until the commit that
    /// gets us there.
    fn buffer(&mut self, group: &GroupId, from: Pid, wire: GroupWire) {
        let Ok(bytes) = wire.encode() else { return };
        let Some(g) = self.groups.get_mut(group) else {
            return;
        };
        g.future.push((from, bytes));
        while g.future.len() > MAX_FUTURE
            || g.future.iter().map(|(_, b)| b.len()).sum::<usize>() > MAX_FUTURE_BYTES
        {
            g.future.remove(0);
        }
    }

    /// Handles what waited for the epoch we're now in.
    fn drain_future(&mut self, group: &GroupId) -> Output {
        let mut out = Output::default();
        if self.draining {
            return out;
        }
        self.draining = true;
        loop {
            let Some(g) = self.groups.get_mut(group) else {
                break;
            };
            if g.future.is_empty() {
                break;
            }
            let epoch = g.mls.epoch();
            let items = std::mem::take(&mut g.future);
            for (from, bytes) in items {
                if let Ok(w) = GroupWire::decode(&bytes)
                    && let Ok(o) = self.handle(from, w)
                {
                    out.extend(o);
                }
            }
            if self
                .groups
                .get(group)
                .is_none_or(|g| g.mls.epoch() == epoch)
            {
                break;
            }
        }
        self.draining = false;
        out
    }

    /// Another member's commit: checked against the roster before it,
    /// then merged.
    fn on_commit(
        &mut self,
        group: &GroupId,
        sender: Pid,
        staged: StagedCommit,
        snapshot: Option<Vec<QueuedProposal>>,
    ) -> Result<Output> {
        let me = self.me;
        let legacy = self.legacy;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let before_roster = g.roster()?;
        let before = before_roster
            .clone()
            .unwrap_or_else(|| Roster::legacy(g.owner));
        let checked = (|| -> Result<Option<Roster>> {
            if legacy {
                // As versions without roles: the creator commits, alone.
                if sender != g.owner {
                    return Err(GroupError::NotOwner);
                }
                return Ok(None);
            }
            let after = match roster_from(staged.group_context().extensions()).transpose()? {
                Some(r) => Some(r),
                None if before_roster.is_some() => {
                    return Err(GroupError::NotAllowed("a roster can't be removed"));
                }
                None => None,
            };
            let props = g.commit_proposals(&sender, staged.queued_proposals())?;
            let mut members_after = g.members();
            for q in staged.remove_proposals() {
                if let Some(p) = g.at(q.remove_proposal().removed()) {
                    members_after.retain(|m| *m != p);
                }
            }
            for q in staged.add_proposals() {
                let leaf = q.add_proposal().key_package().leaf_node();
                members_after.push(identity_of(
                    leaf.credential(),
                    leaf.signature_key().as_slice(),
                )?);
            }
            let after_r = after.clone().unwrap_or_else(|| before.clone());
            check_commit(&before, &after_r, &sender, &props, &members_after)?;
            Ok(after)
        })();
        let after = match checked {
            Ok(a) => a,
            Err(e) => {
                restore_proposals(&self.provider, &mut g.mls, snapshot);
                return Err(e);
            }
        };
        let (added, removed) = membership_changes(g, &sender, staged.queued_proposals());
        g.mls
            .merge_staged_commit(&self.provider, staged)
            .map_err(mls)?;
        if !g.mls.is_active() {
            // We were removed: drop the group's secrets too, or they'd stay
            // in our state, and a later Welcome to the same group would
            // find it there and fail.
            self.forget(group)?;
            return Ok(Output {
                send: vec![],
                events: vec![GroupEvent::Left { group: *group }],
            });
        }
        let mut events: Vec<GroupEvent> = added
            .iter()
            .filter(|(p, _)| *p != me)
            .map(|(p, by)| GroupEvent::MemberAdded {
                group: *group,
                member: *p,
                by: Some(*by),
            })
            .collect();
        events.extend(removed.iter().map(|(p, by)| GroupEvent::MemberRemoved {
            group: *group,
            member: *p,
            by: Some(*by),
        }));
        if let Some(after) = &after {
            events.extend(role_changes(group, &before, after, &g.members()));
            g.owner = after.lead;
        }
        // A commit from our coordinator: we're in step.
        if sender == before.lead {
            g.resync = None;
        }
        let mut out = Output {
            send: vec![],
            events,
        };
        self.after_merge(group, &mut out);
        Ok(out)
    }

    /// A proposal reached us: as the coordinator, we check it, store it and
    /// commit it at once (in a group without a roster, upgrades wait for
    /// the last outdated member).
    fn on_proposal(
        &mut self,
        group: &GroupId,
        sender: Pid,
        q: QueuedProposal,
        message: Vec<u8>,
    ) -> Result<Output> {
        let me = self.me;
        if self.legacy {
            return Err(GroupError::Unexpected("proposals are not used"));
        }
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        if g.lead() != me {
            // Misrouted, or for a coordinator we no longer are.
            return Ok(Output::default());
        }
        let roster = g.roster()?;
        let kind = g.proposal_kind(q.proposal());
        match &roster {
            Some(r) => authorize_proposal(r, &sender, &kind)?,
            None if kind == ProposalKind::Update => {}
            None => return Err(GroupError::Unexpected("proposals are not used")),
        }
        let theirs = g.attachable.iter().filter(|a| a.proposer == sender).count();
        if g.attachable.len() >= MAX_STORED_PROPOSALS || theirs >= MAX_PROPOSALS_PER_PROPOSER {
            return Ok(Output::default());
        }
        let reference = q.proposal_reference_ref().as_slice().to_vec();
        g.mls
            .store_pending_proposal(self.provider.storage(), q)
            .map_err(mls)?;
        g.attachable.push(Attached {
            reference,
            message,
            proposer: sender,
        });
        if roster.is_some() {
            self.commit(
                group,
                Own {
                    consume: true,
                    ..Own::default()
                },
            )
        } else {
            self.commit_upgrade_if_ready(group)
        }
    }

    /// Records `change` as ours, proposes it to the coordinator, and keeps
    /// it until a commit includes it.
    fn propose(&mut self, group: &GroupId, change: Change) -> Result<Output> {
        let out = self.proposal_for(group, &change)?;
        let g = self.group_mut(group)?;
        if !g.intents.iter().any(|i| i.change == change) {
            g.intents.push(Intent {
                change,
                attempts: 0,
            });
        }
        Ok(out)
    }

    /// A proposal for `change` at the current epoch, to the coordinator.
    fn proposal_for(&mut self, group: &GroupId, change: &Change) -> Result<Output> {
        let me = self.me;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let lead = g.lead();
        // Proposing doesn't mind other pending proposals (sending does).
        let msg = match change {
            Change::Add { key_package, .. } => {
                let kp = parse_key_package(&self.provider, key_package)?;
                g.mls
                    .propose_add_member(&self.provider, &self.signer, &kp)
                    .map_err(mls)?
                    .0
            }
            Change::Remove { member } => {
                let index = g
                    .index_of(member)
                    .ok_or_else(|| GroupError::NotMember(member.fingerprint().to_string()))?;
                if *member == me {
                    return Err(GroupError::NotAllowed("leave the group instead"));
                }
                g.mls
                    .propose_remove_member(&self.provider, &self.signer, index)
                    .map_err(mls)?
                    .0
            }
            Change::Roster(changes) => {
                let after = g.roster_or_legacy()?.apply(changes)?;
                g.mls
                    .propose_group_context_extensions(
                        &self.provider,
                        roster_extensions(&after)?,
                        &self.signer,
                    )
                    .map_err(mls)?
                    .0
            }
        };
        let message = msg.tls_serialize_detached().map_err(mls)?;
        let sync = g.sync();
        Ok(Output {
            send: vec![Outgoing {
                to: lead,
                wire: GroupWire::Message {
                    group: *group,
                    message,
                    proposals: Vec::new(),
                    sync,
                },
            }],
            events: vec![],
        })
    }

    /// Our pending changes, to commit ourselves (we coordinate now).
    fn own_from_intents(&self, group: &GroupId) -> Result<Own> {
        let g = self.group(group)?;
        let members = g.members();
        let mut own = Own::default();
        for i in &g.intents {
            match &i.change {
                Change::Add {
                    member,
                    key_package,
                } if !members.contains(member) => {
                    if let Ok(kp) = parse_key_package(&self.provider, key_package) {
                        own.adds.push((*member, kp));
                    }
                }
                Change::Remove { member } if members.contains(member) => {
                    own.removes.push(*member);
                }
                Change::Roster(c) => own.roster.extend(c.iter().cloned()),
                _ => {}
            }
        }
        Ok(own)
    }

    /// Commits `own` changes (and, with `consume`, the proposals stored
    /// this epoch) as the coordinator, a takeover or a claim; merges it and
    /// fans it out.
    fn commit(&mut self, group: &GroupId, own: Own) -> Result<Output> {
        match self.try_commit(group, &own) {
            Ok(out) => Ok(out),
            Err(e) if own.consume => {
                // A stored proposal clashes with ours (or with another):
                // drop them; their proposers propose again.
                let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
                g.mls
                    .clear_pending_proposals(self.provider.storage())
                    .map_err(mls)?;
                g.attachable.clear();
                if own.is_empty() {
                    return Ok(Output::default());
                }
                let _ = e;
                self.try_commit(
                    group,
                    &Own {
                        consume: false,
                        ..own
                    },
                )
            }
            Err(e) => Err(e),
        }
    }

    fn try_commit(&mut self, group: &GroupId, own: &Own) -> Result<Output> {
        let me = self.me;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let before_roster = g.roster()?;
        let before = before_roster
            .clone()
            .unwrap_or_else(|| Roster::legacy(g.owner));
        let members = g.members();
        // Which stored proposals go in.
        let mut include: Vec<ProposalRef> = Vec::new();
        let mut removed: Vec<Pid> = own.removes.clone();
        let mut added: Vec<Pid> = own.adds.iter().map(|(p, _)| *p).collect();
        let mut stored_gce: Option<(ProposalRef, Roster)> = None;
        if own.consume {
            for q in g.mls.pending_proposals() {
                let Some(proposer) = g.sender(q.sender()) else {
                    continue;
                };
                let reference = q.proposal_reference_ref().clone();
                match q.proposal() {
                    Proposal::Add(a) => {
                        let leaf = a.key_package().leaf_node();
                        let Ok(p) = identity_of(leaf.credential(), leaf.signature_key().as_slice())
                        else {
                            continue;
                        };
                        if added.contains(&p) || (members.contains(&p) && !removed.contains(&p)) {
                            continue;
                        }
                        added.push(p);
                        include.push(reference);
                    }
                    Proposal::Remove(r) => {
                        let Some(p) = g.at(r.removed()) else { continue };
                        if removed.contains(&p) || p == me {
                            continue;
                        }
                        removed.push(p);
                        include.push(reference);
                    }
                    Proposal::Update(_) if !removed.contains(&proposer) => include.push(reference),
                    Proposal::GroupContextExtensions(e) if stored_gce.is_none() => {
                        if let Some(r) = proposed_roster(e.extensions()) {
                            stored_gce = Some((reference, r));
                        }
                    }
                    _ => {}
                }
            }
        }
        let mut members_after: Vec<Pid> = members
            .iter()
            .copied()
            .filter(|m| !removed.contains(m))
            .collect();
        members_after.extend(added.iter().copied());
        let roster_mode = before_roster.is_some() || own.install;
        let mut gce: Option<Roster> = None;
        if roster_mode {
            let base = stored_gce
                .as_ref()
                .map_or_else(|| before.clone(), |(_, r)| r.clone());
            let applied = match &own.set_roster {
                Some(r) => r.clone(),
                None => base.apply(&own.roster)?,
            };
            // Roles go with the devices removed (not those re-added).
            let gone: Vec<Pid> = removed
                .iter()
                .copied()
                .filter(|p| !added.contains(p))
                .collect();
            let after = applied.without(&gone);
            after.validate(&members_after)?;
            if own.install || after != base {
                // Ours carries the stored change too, which then stays out.
                gce = Some(after);
            } else if let Some((r, _)) = &stored_gce {
                include.push(r.clone());
            }
        }
        // Build it.
        let mut indices = Vec::new();
        for r in &own.removes {
            indices.push(
                g.index_of(r)
                    .ok_or_else(|| GroupError::NotMember(r.fingerprint().to_string()))?,
            );
        }
        let mut builder = g
            .mls
            .commit_builder()
            .consume_proposal_store(own.consume)
            .propose_adds(own.adds.iter().map(|(_, kp)| kp.clone()))
            .propose_removals(indices);
        if let Some(r) = &gce {
            builder = builder
                .propose_group_context_extensions(roster_extensions(r)?)
                .map_err(mls)?;
        }
        if own.self_update {
            builder = builder
                .leaf_node_parameters(
                    LeafNodeParameters::builder()
                        .with_capabilities(capabilities())
                        .build(),
                )
                .force_self_update(true);
        }
        let bundle = builder
            .load_psks(self.provider.storage())
            .map_err(mls)?
            .build(
                self.provider.rand(),
                self.provider.crypto(),
                &self.signer,
                |q| {
                    q.proposal_or_ref_type() == ProposalOrRefType::Proposal
                        || include.contains(q.proposal_reference_ref())
                },
            )
            .map_err(mls)?
            .stage_commit(&self.provider)
            .map_err(mls)?;
        // Check our own commit as every member will, before it's final.
        let staged = g
            .mls
            .pending_commit()
            .ok_or(GroupError::Unexpected("no pending commit"))?;
        let props = g.commit_proposals(&me, staged.queued_proposals())?;
        let after_roster = roster_from(staged.group_context().extensions()).transpose();
        let checked = after_roster.and_then(|after| {
            let after_r = after.clone().unwrap_or_else(|| before.clone());
            check_commit(&before, &after_r, &me, &props, &members_after).map(|_| after)
        });
        let after = match checked {
            Ok(a) => a,
            Err(e) => {
                g.mls
                    .clear_pending_commit(self.provider.storage())
                    .map_err(mls)?;
                return Err(e);
            }
        };
        let (joined, left) = membership_changes(g, &me, staged.queued_proposals());
        let referenced: Vec<Vec<u8>> = staged
            .queued_proposals()
            .filter(|q| q.proposal_or_ref_type() == ProposalOrRefType::Reference)
            .map(|q| q.proposal_reference_ref().as_slice().to_vec())
            .collect();
        // Who proposed each add someone else asked for: they pass on its
        // Welcome.
        let proposed_adds: Vec<(Pid, Pid)> = staged
            .queued_proposals()
            .filter(|q| q.proposal_or_ref_type() == ProposalOrRefType::Reference)
            .filter_map(|q| {
                let Proposal::Add(a) = q.proposal() else {
                    return None;
                };
                let leaf = a.key_package().leaf_node();
                let p = identity_of(leaf.credential(), leaf.signature_key().as_slice()).ok()?;
                Some((p, g.sender(q.sender())?))
            })
            .collect();
        g.mls.merge_pending_commit(&self.provider).map_err(mls)?;
        let (commit, welcome, _) = bundle.into_contents();
        let message = commit.tls_serialize_detached().map_err(mls)?;
        let attached: Vec<Vec<u8>> = g
            .attachable
            .iter()
            .filter(|a| referenced.contains(&a.reference))
            .map(|a| a.message.clone())
            .collect();
        g.attachable.clear();
        if let Some(a) = &after {
            g.owner = a.lead;
        }
        let sync = g.sync();
        let mut send: Vec<Outgoing> = members
            .iter()
            .filter(|p| **p != me)
            .map(|to| Outgoing {
                to: *to,
                wire: GroupWire::Message {
                    group: *group,
                    message: message.clone(),
                    proposals: attached.clone(),
                    sync,
                },
            })
            .collect();
        if let Some(w) = welcome {
            let welcome = MlsMessageOut::from_welcome(w, ProtocolVersion::Mls10)
                .tls_serialize_detached()
                .map_err(mls)?;
            for (p, _) in &own.adds {
                send.push(Outgoing {
                    to: *p,
                    wire: GroupWire::Welcome {
                        group: *group,
                        name: g.name.clone(),
                        welcome: welcome.clone(),
                    },
                });
            }
            for (p, by) in &proposed_adds {
                send.push(Outgoing {
                    to: *by,
                    wire: GroupWire::PassWelcome {
                        group: *group,
                        member: *p.as_bytes(),
                        name: g.name.clone(),
                        welcome: welcome.clone(),
                    },
                });
            }
        }
        let mut events: Vec<GroupEvent> = joined
            .iter()
            .map(|(p, by)| GroupEvent::MemberAdded {
                group: *group,
                member: *p,
                by: Some(*by),
            })
            .collect();
        events.extend(left.iter().map(|(p, by)| GroupEvent::MemberRemoved {
            group: *group,
            member: *p,
            by: Some(*by),
        }));
        if let Some(a) = &after {
            events.extend(role_changes(group, &before, a, &g.members()));
        }
        let mut out = Output { send, events };
        self.after_merge(group, &mut out);
        Ok(out)
    }

    /// After every epoch change: our changes that landed are done, the
    /// rest are proposed again (or committed, if we coordinate now); roles
    /// waiting for an upgrade go in once they can; members upgrade their
    /// leaves; what waited for this epoch is handled.
    fn after_merge(&mut self, group: &GroupId, out: &mut Output) {
        let me = self.me;
        let Some(g) = self.groups.get_mut(group) else {
            return;
        };
        let Ok(roster) = g.roster_or_legacy() else {
            return;
        };
        let has_roster = g.roster().ok().flatten().is_some();
        let members = g.members();
        let mut failed = Vec::new();
        g.intents.retain_mut(|i| {
            let done = match &i.change {
                Change::Add { member, .. } => members.contains(member),
                Change::Remove { member } => !members.contains(member),
                Change::Roster(c) => match roster.apply(c) {
                    Ok(r) => r == roster || r.validate(&members).is_err(),
                    Err(_) => {
                        failed.push("the change no longer applies");
                        return false;
                    }
                },
            };
            if done {
                return false;
            }
            // Roles waiting for every member to upgrade aren't being
            // passed over: no count.
            if !has_roster {
                return true;
            }
            i.attempts += 1;
            if i.attempts > MAX_INTENT_ATTEMPTS {
                failed.push("the coordinator didn't take it");
                return false;
            }
            true
        });
        out.events
            .extend(failed.into_iter().map(|reason| GroupEvent::ChangeFailed {
                group: *group,
                reason,
            }));
        let intents: Vec<Change> = g.intents.iter().map(|i| i.change.clone()).collect();
        if !intents.is_empty() {
            if roster.lead == me && has_roster {
                if let Ok(own) = self.own_from_intents(group)
                    && !own.is_empty()
                {
                    match self.commit(
                        group,
                        Own {
                            consume: true,
                            ..own
                        },
                    ) {
                        Ok(o) => out.extend(o),
                        Err(_) => {
                            if let Some(g) = self.groups.get_mut(group) {
                                g.intents.clear();
                            }
                            out.events.push(GroupEvent::ChangeFailed {
                                group: *group,
                                reason: "the change couldn't be applied",
                            });
                        }
                    }
                }
            } else if roster.lead == me {
                if let Ok(o) = self.install(group) {
                    out.extend(o);
                }
            } else {
                for c in intents {
                    match self.proposal_for(group, &c) {
                        Ok(o) => out.extend(o),
                        Err(_) => {
                            if let Some(g) = self.groups.get_mut(group) {
                                g.intents.retain(|i| i.change != c);
                            }
                            out.events.push(GroupEvent::ChangeFailed {
                                group: *group,
                                reason: "the change no longer applies",
                            });
                        }
                    }
                }
            }
        }
        if let Ok(o) = self.propose_upgrade(group, false) {
            out.extend(o);
        }
        out.extend(self.drain_future(group));
    }

    /// In a group without a roster, a member running this version whose
    /// leaf predates roles proposes updating it once the coordinator has
    /// (once per epoch, unless `again`).
    fn propose_upgrade(&mut self, group: &GroupId, again: bool) -> Result<Output> {
        let me = self.me;
        if self.legacy {
            return Ok(Output::default());
        }
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let epoch = g.mls.epoch().as_u64();
        let lead = g.lead();
        let lead_capable = g.index_of(&lead).is_some_and(|i| g.leaf_capable(i));
        if g.roster()?.is_some()
            || lead == me
            || g.own_leaf_capable()
            || !lead_capable
            || (!again && g.upgrade_epoch == Some(epoch))
        {
            return Ok(Output::default());
        }
        let (msg, _) = g
            .mls
            .propose_self_update(
                &self.provider,
                &self.signer,
                LeafNodeParameters::builder()
                    .with_capabilities(capabilities())
                    .build(),
            )
            .map_err(mls)?;
        g.upgrade_epoch = Some(epoch);
        Ok(Output {
            send: vec![Outgoing {
                to: lead,
                wire: GroupWire::message(*group, msg.tls_serialize_detached().map_err(mls)?),
            }],
            events: vec![],
        })
    }

    /// Coordinator of a group without a roster, with role changes waiting:
    /// updates our own leaf first, then installs the roster once every
    /// leaf supports it; until then says who must update.
    fn install(&mut self, group: &GroupId) -> Result<Output> {
        let g = self.group(group)?;
        if g.roster()?.is_some() {
            return Ok(Output::default());
        }
        if !g.own_leaf_capable() {
            // Members only propose upgrades once our leaf has it; then the
            // commit that takes them installs the roster too.
            return self.commit(
                group,
                Own {
                    self_update: true,
                    ..Own::default()
                },
            );
        }
        let outdated = g.outdated();
        if !outdated.is_empty() {
            let mut out = self.commit_upgrade_if_ready(group)?;
            if out.send.is_empty() {
                out.events.push(GroupEvent::UpgradePending {
                    group: *group,
                    outdated,
                });
            }
            return Ok(out);
        }
        let changes: Vec<RosterChange> = g
            .intents
            .iter()
            .filter_map(|i| match &i.change {
                Change::Roster(c) => Some(c.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        if changes.is_empty() {
            return Ok(Output::default());
        }
        self.commit(
            group,
            Own {
                roster: changes,
                install: true,
                ..Own::default()
            },
        )
    }

    /// Coordinator of a group without a roster: once every outdated
    /// member proposed updating its leaf this epoch, commits all those
    /// updates (no member left is then outdated).
    fn commit_upgrade_if_ready(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let g = self.group(group)?;
        if g.roster()?.is_some() || g.lead() != me || !g.own_leaf_capable() {
            return Ok(Output::default());
        }
        let outdated: Vec<LeafNodeIndex> = g
            .mls
            .members()
            .filter(|m| !g.leaf_capable(m.index))
            .map(|m| m.index)
            .collect();
        if outdated.is_empty() {
            return Ok(Output::default());
        }
        let ready = outdated.iter().all(|i| {
            g.mls.pending_proposals().any(|q| {
                matches!(q.sender(), Sender::Member(s) if s == i)
                    && matches!(q.proposal(), Proposal::Update(u) if capable(u.leaf_node()))
            })
        });
        if !ready {
            return Ok(Output::default());
        }
        self.commit(
            group,
            Own {
                consume: true,
                ..Own::default()
            },
        )
    }

    /// We're on the losing branch of a fork: ask `from` (on the winning
    /// one) to have its coordinator bring us back, once per epoch and term.
    fn ask_resync(&mut self, from: Pid, group: &GroupId) -> Result<Output> {
        let g = self.group_mut(group)?;
        let Some(sync) = g.sync() else {
            return Ok(Output::default());
        };
        if g.resync == Some((sync.epoch, sync.term)) {
            return Ok(Output::default());
        }
        g.resync = Some((sync.epoch, sync.term));
        Ok(Output {
            send: vec![Outgoing {
                to: from,
                wire: GroupWire::Resync {
                    group: *group,
                    member: None,
                    sync,
                },
            }],
            events: vec![GroupEvent::OutOfStep { group: *group }],
        })
    }

    fn on_resync(
        &mut self,
        from: Pid,
        group: GroupId,
        member: Option<[u8; 32]>,
        sync: SyncHint,
    ) -> Result<Output> {
        let me = self.me;
        let g = self.group(&group)?;
        let Some(roster) = g.roster()? else {
            return Ok(Output::default());
        };
        let members = g.members();
        let name = g.name.clone();
        if !members.contains(&from) {
            return Err(GroupError::NotMember(from.fingerprint().to_string()));
        }
        let stale = match member {
            Some(m) => PublicIdentity::from_bytes(&m)?,
            None => from,
        };
        if roster.lead != me {
            // The stale device asked us: pass it on to our coordinator.
            if member.is_some() {
                return Ok(Output::default());
            }
            if !self.relays.contains(&(group, stale)) {
                self.relays.push((group, stale));
            }
            return Ok(Output {
                send: vec![Outgoing {
                    to: roster.lead,
                    wire: GroupWire::Resync {
                        group,
                        member: Some(*stale.as_bytes()),
                        sync,
                    },
                }],
                events: vec![],
            });
        }
        // Only a member really behind us, once per term: a forged or
        // replayed request about one in step changes nothing.
        if stale == me
            || !members.contains(&stale)
            || sync.rank() >= roster.rank()
            || self
                .repairs
                .iter()
                .any(|(g, p, t)| *g == group && *p == stale && *t == roster.term)
        {
            return Ok(Output::default());
        }
        self.repairs.push((group, stale, roster.term));
        if !self.pending_adds.contains(&(group, stale)) {
            self.pending_adds.push((group, stale));
        }
        Ok(Output {
            send: vec![Outgoing {
                to: stale,
                wire: GroupWire::KeyPackageRequest { group, name },
            }],
            events: vec![],
        })
    }

    fn on_pass_welcome(
        &mut self,
        from: Pid,
        group: GroupId,
        member: [u8; 32],
        welcome: Vec<u8>,
    ) -> Result<Output> {
        let g = self.group(&group)?;
        if g.roster_or_legacy()?.role(&from) != MemberRole::Owner {
            return Err(GroupError::Unexpected("welcome passed by a non-owner"));
        }
        let member = PublicIdentity::from_bytes(&member)?;
        let ours = g.members().contains(&member)
            || g.intents
                .iter()
                .any(|i| matches!(&i.change, Change::Add { member: m, .. } if *m == member))
            || self.relays.contains(&(group, member));
        if !ours {
            return Err(GroupError::Unexpected("welcome for someone we didn't add"));
        }
        Ok(Output {
            send: vec![Outgoing {
                to: member,
                wire: GroupWire::Welcome {
                    group,
                    name: g.name.clone(),
                    welcome,
                },
            }],
            events: vec![],
        })
    }

    fn on_leave(&mut self, from: Pid, group: GroupId, proposal: Option<Vec<u8>>) -> Result<Output> {
        let me = self.me;
        let g = self.group(&group)?;
        let roster = g.roster()?;
        let lead = g.lead();
        let members = g.members();
        if !members.contains(&from) {
            return Err(GroupError::NotMember(from.fingerprint().to_string()));
        }
        let epoch = proposal.as_ref().and_then(|p| {
            MlsMessageIn::tls_deserialize_exact(p)
                .ok()
                .and_then(|m| m.try_into_protocol_message().ok())
                .map(|m| m.epoch().as_u64())
        });
        let ours = g.mls.epoch().as_u64();
        if roster.is_some() && epoch.is_some_and(|e| e > ours) {
            // The leaver merged a commit we haven't yet (which may make us
            // its coordinator).
            self.buffer(&group, from, GroupWire::Leave { group, proposal });
            return Ok(Output::default());
        }
        if lead != me {
            // Meant for a coordinator we were: `from` asked us itself, so
            // an owner device or admin may propose removing it.
            return match &roster {
                Some(r) if authorize_proposal(r, &me, &ProposalKind::Remove(from)).is_ok() => {
                    self.propose(&group, Change::Remove { member: from })
                }
                _ => Err(GroupError::NotOwner),
            };
        }
        if from == me {
            return Err(GroupError::Unexpected("leave request from ourselves"));
        }
        // Its own Remove, committed by reference: members see it left
        // rather than was removed.
        if let (Some(_), Some(p), Some(e)) = (&roster, &proposal, epoch)
            && e == ours
            && let Ok(mut out) = self.on_message(from, group, p.clone(), Vec::new(), None)
            && out
                .events
                .iter()
                .any(|e| matches!(e, GroupEvent::MemberRemoved { member, .. } if *member == from))
        {
            // The leaver has already forgotten the group.
            out.send.retain(|o| o.to != from);
            return Ok(out);
        }
        // The leaver has already forgotten the group.
        let mut out = self.commit(
            &group,
            Own {
                removes: vec![from],
                consume: roster.is_some(),
                ..Own::default()
            },
        )?;
        out.send.retain(|o| o.to != from);
        Ok(out)
    }
}

/// Group state kept beside openmls's, as restored.
struct Local {
    intents: Vec<Intent>,
    attachable: Vec<Attached>,
    future: Vec<(Pid, Vec<u8>)>,
    upgrade_epoch: Option<u64>,
    resync: Option<(u64, u32)>,
}

fn decode_local(d: &mut const_cbor::Decoder<'_>) -> threnody_core::Result<Local> {
    use threnody_core::Error as E;
    let mut out = Local {
        intents: Vec::new(),
        attachable: Vec::new(),
        future: Vec::new(),
        upgrade_epoch: None,
        resync: None,
    };
    threnody_core::cbor::read_map(d, |k, d| {
        match k {
            0 => {
                for _ in 0..d.array_len()? {
                    let n = d.array_len()?;
                    let change = match (d.u8()?, n) {
                        (0, 4) => Change::Add {
                            member: decode_pid(d)?,
                            key_package: d.bytes()?.to_vec(),
                        },
                        (1, 3) => Change::Remove {
                            member: decode_pid(d)?,
                        },
                        (2, 3) => {
                            let mut changes = Vec::new();
                            for _ in 0..d.array_len()? {
                                if d.array_len()? != 2 {
                                    return Err(E::Malformed("roster change"));
                                }
                                let kind = d.u8()?;
                                let mut devices = Vec::new();
                                for _ in 0..d.array_len()? {
                                    devices.push(decode_pid(d)?);
                                }
                                changes.push(
                                    RosterChange::from_parts(kind, devices)
                                        .ok_or(E::Malformed("roster change"))?,
                                );
                            }
                            Change::Roster(changes)
                        }
                        _ => return Err(E::Malformed("intent")),
                    };
                    out.intents.push(Intent {
                        change,
                        attempts: d.u8()?,
                    });
                }
            }
            1 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 3 {
                        return Err(E::Malformed("attached proposal"));
                    }
                    out.attachable.push(Attached {
                        reference: d.bytes()?.to_vec(),
                        message: d.bytes()?.to_vec(),
                        proposer: decode_pid(d)?,
                    });
                }
            }
            2 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 2 {
                        return Err(E::Malformed("buffered message"));
                    }
                    out.future.push((decode_pid(d)?, d.bytes()?.to_vec()));
                }
            }
            3 => out.upgrade_epoch = Some(d.u64()?),
            4 => {
                if d.array_len()? != 2 {
                    return Err(E::Malformed("resync"));
                }
                out.resync = Some((d.u64()?, d.u32()?));
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(out)
}

/// Runs `f` with our pending proposals set aside, then puts them back:
/// openmls refuses to send while any are pending, but ours may wait days
/// for an offline coordinator (see Appendix F, "Proposals").
fn with_proposals_aside<T>(
    provider: &OpenMlsRustCrypto,
    group: &mut MlsGroup,
    f: impl FnOnce(&mut MlsGroup, &OpenMlsRustCrypto) -> Result<T>,
) -> Result<T> {
    let kept: Vec<QueuedProposal> = group.pending_proposals().cloned().collect();
    if kept.is_empty() {
        return f(group, provider);
    }
    group
        .clear_pending_proposals(provider.storage())
        .map_err(mls)?;
    let out = f(group, provider);
    for q in kept {
        group
            .store_pending_proposal(provider.storage(), q)
            .map_err(mls)?;
    }
    out
}

/// Puts the proposal store back as it was before a commit's attachments
/// were stored, when the commit is refused.
fn restore_proposals(
    provider: &OpenMlsRustCrypto,
    group: &mut MlsGroup,
    snapshot: Option<Vec<QueuedProposal>>,
) {
    let Some(snapshot) = snapshot else { return };
    let _ = group.clear_pending_proposals(provider.storage());
    for q in snapshot {
        let _ = group.store_pending_proposal(provider.storage(), q);
    }
}

/// Role changes between two rosters, for the members after.
fn role_changes(
    group: &GroupId,
    before: &Roster,
    after: &Roster,
    members: &[Pid],
) -> Vec<GroupEvent> {
    members
        .iter()
        .filter(|m| before.role(m) != after.role(m))
        .map(|m| GroupEvent::RoleChanged {
            group: *group,
            member: *m,
            role: after.role(m),
        })
        .collect()
}

fn application_event(group: GroupId, sender: Pid, plain: &[u8]) -> Result<GroupEvent> {
    Ok(match Content::decode(plain)? {
        Content::Text {
            text,
            id,
            quote,
            mentions,
            forwarded,
        } => GroupEvent::Text {
            group,
            from: sender,
            text,
            id,
            quote,
            mentions,
            forwarded,
        },
        Content::File {
            name,
            data,
            sensitive,
            caption,
            album,
            id,
            clip,
            forwarded,
        } => GroupEvent::File {
            group,
            from: sender,
            name: truncate(&name),
            data,
            sensitive,
            caption,
            album,
            id,
            clip,
            forwarded,
        },
        Content::React { id, emoji, add } => GroupEvent::React {
            group,
            from: sender,
            id,
            emoji,
            add,
        },
        Content::Pin { id, pin } => GroupEvent::Pin {
            group,
            from: sender,
            id,
            pin,
        },
        Content::Poll {
            id,
            question,
            options,
            multi,
        } => GroupEvent::Poll {
            group,
            from: sender,
            id,
            question,
            options,
            multi,
        },
        Content::Vote { id, choices } => GroupEvent::Vote {
            group,
            from: sender,
            id,
            choices,
        },
        Content::ClosePoll { id } => GroupEvent::ClosePoll {
            group,
            from: sender,
            id,
        },
        Content::List(list) => GroupEvent::List {
            group,
            from: sender,
            list,
        },
        Content::Call(msg) => GroupEvent::Call {
            group,
            from: sender,
            msg,
        },
    })
}

/// Whether `message` (the MLS message in a `GroupWire::Message`) is a
/// commit or proposal, without which its member can't read what follows.
/// Anyone can tell: MLS leaves the content type outside the encryption.
#[cfg(feature = "node")]
pub(crate) fn is_handshake(message: &[u8]) -> bool {
    let Ok(msg) = MlsMessageIn::tls_deserialize_exact(message) else {
        return false;
    };
    let content = match msg.extract() {
        MlsMessageBodyIn::PrivateMessage(m) => m.content_type(),
        MlsMessageBodyIn::PublicMessage(m) => m.content_type(),
        _ => return false,
    };
    content != ContentType::Application
}

/// A name as we show it: at most [`MAX_NAME`] characters, without
/// control characters or the invisible ones that reorder or hide text
/// (bidirectional overrides and isolates, zero-width characters).
fn truncate(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && !invisible(*c))
        .take(MAX_NAME)
        .collect()
}

fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Groups by identity, delivering everything until quiet (invitations
    /// accepted, errors noted).
    struct Mini {
        nodes: HashMap<Pid, Groups>,
        errors: Vec<String>,
    }

    impl Mini {
        fn node(&mut self, p: &Pid) -> &mut Groups {
            self.nodes.get_mut(p).unwrap()
        }

        fn deliver(&mut self, from: Pid, out: Output) {
            for o in out.send {
                let Some(n) = self.nodes.get_mut(&o.to) else {
                    continue;
                };
                match n.handle(from, o.wire) {
                    Ok(res) => {
                        for e in &res.events {
                            if let GroupEvent::InviteRequested { group, peer, .. } = e {
                                let reply = self.node(&o.to).accept_invite(group, *peer).unwrap();
                                self.deliver(o.to, reply);
                            }
                        }
                        self.deliver(o.to, res);
                    }
                    Err(e) => self.errors.push(e.to_string()),
                }
            }
        }
    }

    /// `n` people, sorted; the first creates a group and invites the rest.
    fn team(n: usize) -> (Vec<Identity>, Vec<Pid>, Mini, GroupId) {
        let mut ids: Vec<Identity> = (0..n).map(|_| Identity::generate()).collect();
        ids.sort_by_key(|i| *i.public().as_bytes());
        let p: Vec<Pid> = ids.iter().map(Identity::public).collect();
        let mut net = Mini {
            nodes: ids.iter().map(|i| (i.public(), Groups::new(i))).collect(),
            errors: Vec::new(),
        };
        let g = net.node(&p[0]).create("team").unwrap();
        for q in &p[1..] {
            let out = net.node(&p[0]).invite(&g, *q).unwrap();
            net.deliver(p[0], out);
        }
        (ids, p, net, g)
    }

    fn epoch(net: &mut Mini, p: &Pid, g: &GroupId) -> u64 {
        net.node(p).epoch_state(g).unwrap().0
    }

    #[test]
    fn capabilities_advertise_the_roster_and_xwing() {
        let (a, b) = (Identity::generate(), Identity::generate());
        let mut ga = Groups::new(&a);
        let g = ga.create("x").unwrap();
        let leaf = ga.groups[&g].mls.own_leaf_node().unwrap().clone();
        for caps in [leaf.capabilities().clone(), {
            let mut gb = Groups::new(&b);
            let out = gb.accept_invite(&g, a.public()).unwrap();
            let GroupWire::KeyPackage { key_package, .. } = &out.send[0].wire else {
                panic!("a key package")
            };
            parse_key_package(&gb.provider, key_package)
                .unwrap()
                .leaf_node()
                .capabilities()
                .clone()
        }] {
            assert!(
                caps.extensions()
                    .contains(&ExtensionType::Unknown(ROSTER_EXTENSION))
            );
            assert!(caps.ciphersuites().contains(&CIPHERSUITE.into()));
        }
        // An older version's don't.
        let old = Groups::legacy_for_tests(&b);
        let _ = old;
    }

    #[test]
    fn non_coordinator_commits_are_rejected() {
        let (_, p, mut net, g) = team(3);
        let (a, b, c) = (p[0], p[1], p[2]);
        let out = net.node(&a).promote(&g, b).unwrap();
        net.deliver(a, out);
        let before = epoch(&mut net, &c, &g);
        // Bob, an admin, commits a removal himself.
        let gb = net.node(&b);
        let Groups {
            groups,
            provider,
            signer,
            ..
        } = gb;
        let grp = groups.get_mut(&g).unwrap();
        let idx = grp.index_of(&c).unwrap();
        let (commit, _, _) = grp.mls.remove_members(provider, signer, &[idx]).unwrap();
        let commit = commit.tls_serialize_detached().unwrap();
        for x in [a, c] {
            let r = net
                .node(&x)
                .handle(b, GroupWire::message(g, commit.clone()));
            assert!(matches!(r, Err(GroupError::NotAllowed(_))), "{r:?}");
        }
        assert_eq!(epoch(&mut net, &c, &g), before);
    }

    #[test]
    fn forged_proposals_in_a_coordinator_commit_are_rejected() {
        let (_, p, mut net, g) = team(5);
        let (a, b, c, d, e) = (p[0], p[1], p[2], p[3], p[4]);
        for x in [b, d] {
            let out = net.node(&a).promote(&g, x).unwrap();
            net.deliver(a, out);
        }
        // Dave (an admin) has a proposal of his own waiting.
        let _ = net.node(&d).remove(&g, &e).unwrap();
        assert_eq!(net.node(&d).stored_proposals(&g), 1);
        // Bob proposes promoting Carol, which only owners may; a modified
        // coordinator stores and commits it anyway.
        let gb = net.node(&b);
        let Groups {
            groups,
            provider,
            signer,
            ..
        } = gb;
        let grp = groups.get_mut(&g).unwrap();
        let promoted = grp
            .roster_or_legacy()
            .unwrap()
            .apply(&[RosterChange::Promote(vec![c])])
            .unwrap();
        let (proposal, _) = grp
            .mls
            .propose_group_context_extensions(
                provider,
                roster_extensions(&promoted).unwrap(),
                signer,
            )
            .unwrap();
        let proposal = proposal.tls_serialize_detached().unwrap();
        let ga = net.node(&a);
        let Groups {
            groups,
            provider,
            signer,
            ..
        } = ga;
        let grp = groups.get_mut(&g).unwrap();
        let pm = MlsMessageIn::tls_deserialize_exact(&proposal)
            .unwrap()
            .try_into_protocol_message()
            .unwrap();
        let ProcessedMessageContent::ProposalMessage(q) = grp
            .mls
            .process_message(provider, pm)
            .unwrap()
            .into_content()
        else {
            panic!("a proposal")
        };
        grp.mls
            .store_pending_proposal(provider.storage(), *q)
            .unwrap();
        let (commit, _, _) = grp
            .mls
            .commit_to_pending_proposals(provider, signer)
            .unwrap();
        let wire = GroupWire::Message {
            group: g,
            message: commit.tls_serialize_detached().unwrap(),
            proposals: vec![proposal, vec![1, 2, 3]],
            sync: None,
        };
        let before = epoch(&mut net, &d, &g);
        assert!(matches!(
            net.node(&d).handle(a, wire),
            Err(GroupError::NotAllowed(_))
        ));
        assert_eq!(epoch(&mut net, &d, &g), before);
        // The store holds Dave's own proposal again, and nothing else.
        assert_eq!(net.node(&d).stored_proposals(&g), 1);
    }

    #[test]
    fn welcome_must_come_from_the_coordinator() {
        let (ids, p, mut net, g) = team(3);
        let (a, b, c) = (p[0], p[1], p[2]);
        let out = net.node(&a).promote(&g, b).unwrap();
        net.deliver(a, out);
        let e_id = Identity::generate();
        let e = e_id.public();
        net.nodes.insert(e, Groups::new(&e_id));
        let _ = ids;
        // Eve gives Bob (an admin) a key package; Bob adds her himself.
        let out = net.node(&e).accept_invite(&g, b).unwrap();
        let GroupWire::KeyPackage { key_package, .. } = &out.send[0].wire else {
            panic!("a key package")
        };
        let gb = net.node(&b);
        let kp = parse_key_package(&gb.provider, key_package).unwrap();
        let Groups {
            groups,
            provider,
            signer,
            ..
        } = gb;
        let (_, welcome, _) = groups
            .get_mut(&g)
            .unwrap()
            .mls
            .add_members(provider, signer, &[kp])
            .unwrap();
        let w = GroupWire::Welcome {
            group: g,
            name: "team".into(),
            welcome: welcome.tls_serialize_detached().unwrap(),
        };
        assert!(net.node(&e).handle(b, w.clone()).is_err());
        assert!(net.node(&e).list().is_empty());
        // Nor relayed by a member Eve gave nothing.
        assert!(net.node(&e).handle(c, w).is_err());
    }

    #[test]
    fn legacy_commits_never_reference_proposals() {
        let (a, b, l, x) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let (pa, pb, pl, px) = (a.public(), b.public(), l.public(), x.public());
        let mut net = Mini {
            nodes: [
                (pa, Groups::new(&a)),
                (pb, Groups::legacy_for_tests(&b)),
                (pl, Groups::legacy_for_tests(&l)),
                (px, Groups::new(&x)),
            ]
            .into(),
            errors: Vec::new(),
        };
        let g = net.node(&pa).create("old").unwrap();
        for q in [pb, pl, px] {
            let out = net.node(&pa).invite(&g, q).unwrap();
            net.deliver(pa, out);
        }
        // Alice wants roles: Lynn hasn't updated, Bob has and proposes
        // upgrading his leaf, which Alice keeps for later.
        let out = net.node(&pa).promote(&g, px).unwrap();
        net.deliver(pa, out);
        let state = net.node(&pb).export().unwrap();
        net.nodes.insert(pb, Groups::restore(&b, &state).unwrap());
        let out = net.node(&pb).resume(&g).unwrap();
        net.deliver(pb, out);
        assert_eq!(net.node(&pa).stored_proposals(&g), 1);
        // Her next commit leaves it out, so Lynn's version takes it.
        let out = net.node(&pa).remove(&g, &px).unwrap();
        net.deliver(pa, out);
        assert!(net.errors.is_empty(), "{:?}", net.errors);
        assert_eq!(epoch(&mut net, &pl, &g), epoch(&mut net, &pa, &g));
        // Bob proposed again for the new epoch; still waiting for Lynn.
        assert_eq!(net.node(&pa).stored_proposals(&g), 1);
        assert!(!net.node(&pa).has_roster(&g).unwrap());
    }

    /// A group with id `id` made by `gs` alone, with `roster`, that adds
    /// the owner of `key_package`: its Welcome.
    fn look_alike(gs: &mut Groups, id: GroupId, roster: &Roster, key_package: &[u8]) -> Vec<u8> {
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .capabilities(capabilities())
            .build();
        let mut mls = MlsGroup::new_with_group_id(
            &gs.provider,
            &gs.signer,
            &config,
            openmls::group::GroupId::from_slice(&id),
            credential_for(&gs.me),
        )
        .unwrap();
        let kp = parse_key_package(&gs.provider, key_package).unwrap();
        let bundle = mls
            .commit_builder()
            .propose_group_context_extensions(roster_extensions(roster).unwrap())
            .unwrap()
            .propose_adds([kp])
            .load_psks(gs.provider.storage())
            .unwrap()
            .build(gs.provider.rand(), gs.provider.crypto(), &gs.signer, |_| {
                true
            })
            .unwrap()
            .stage_commit(&gs.provider)
            .unwrap();
        mls.merge_pending_commit(&gs.provider).unwrap();
        MlsMessageOut::from_welcome(bundle.into_welcome().unwrap(), ProtocolVersion::Mls10)
            .tls_serialize_detached()
            .unwrap()
    }

    #[test]
    fn forged_rewelcome_is_refused() {
        let (ids, p, mut net, g) = team(4);
        let (a, a2, c, m) = (p[0], p[1], p[2], p[3]);
        let out = net
            .node(&a)
            .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
            .unwrap();
        net.deliver(a, out);
        let before = net.node(&c).epoch_state(&g);
        // Mallory, a member, asks Carol for a key package and welcomes her
        // into her own group under the same id, naming herself coordinator.
        let forge = |net: &mut Mini, who: &Identity| {
            let out = net.node(&c).accept_invite(&g, who.public()).unwrap();
            let GroupWire::KeyPackage { key_package, .. } = &out.send[0].wire else {
                panic!("a key package")
            };
            let mut roster = Roster::legacy(who.public());
            roster.term = 9;
            let welcome = look_alike(&mut Groups::new(who), g, &roster, key_package);
            GroupWire::Welcome {
                group: g,
                name: "team".into(),
                welcome,
            }
        };
        let w = forge(&mut net, &ids[3]);
        assert!(net.node(&c).handle(m, w).is_err());
        assert_eq!(net.node(&c).epoch_state(&g), before);
        assert_eq!(net.node(&c).roster(&g).unwrap().lead, a);
        // From an owner device of the group, a branch that outranks ours is
        // taken.
        let w = forge(&mut net, &ids[1]);
        net.node(&c).handle(a2, w).unwrap();
        let r = net.node(&c).roster(&g).unwrap();
        assert_eq!((r.lead, r.term), (a2, 9));
    }
}
