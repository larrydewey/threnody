//! Who may change a group, kept in the group's authenticated MLS state.
//!
//! The roster is a private-use GroupContext extension (`0xFA01`, RFC 9420
//! §17.3), so every member holds the same one at every epoch and it is
//! bound into the key schedule. It names the owner's devices, the admins,
//! an optional successor (one admin account's devices) and the
//! *coordinator* (`lead`): the one owner device that commits. Admins and
//! the owner's other devices propose; the lead commits what they propose;
//! every member checks every commit against the roster it had before it
//! ([`check_commit`]). Only an explicit takeover or claim is committed by
//! anyone else, and each bumps the `term`, which ranks branches should two
//! ever race (see `docs/appendix-f-groups.md`, "Roles and the roster").
//!
//! ```text
//! Roster = {                    ; deterministic CBOR, keys in order, exactly these keys
//!   0: 1,                       ; roster version
//!   1: term uint,               ; +1 whenever the coordinator changes
//!   2: lead bstr .size 32,      ; coordinating device; must be in owners
//!   3: [+ bstr .size 32],       ; owner devices, sorted ascending, 1..=16
//!   4: [* bstr .size 32],       ; admin devices, sorted ascending, 0..=80, disjoint from owners
//!   ? 5: [+ bstr .size 32]      ; successor devices (one account's), sorted, 1..=16, all in admins
//! }
//! ```
//!
//! Version 1 is decoded strictly (the documented exception to "ignore
//! unknown fields": this is security-critical). A later version is read
//! for keys 1-3 only; its group then takes commits from its lead alone and
//! asks the user to update.

use std::cmp::Reverse;

use const_cbor::Decoder;
use openmls::prelude::*;
use threnody_core::PublicIdentity;
use threnody_core::cbor::{self, finish, read_map};

use crate::{GroupError, MemberRole, Result};

/// The roster's GroupContext extension type (and leaf capability).
pub const ROSTER_EXTENSION: u16 = 0xFA01;
/// The roster version this code writes and fully understands.
pub const ROSTER_VERSION: u8 = 1;
/// Owner devices (one account's).
pub const MAX_OWNERS: usize = 16;
/// Admins anyone may promote; former owners can add up to [`MAX_OWNERS`]
/// more after a transfer or claim.
pub const MAX_PROMOTED: usize = 64;
pub const MAX_ADMINS: usize = MAX_PROMOTED + MAX_OWNERS;

type Pid = PublicIdentity;

/// The group's roles, as every member agrees on them at an epoch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roster {
    pub version: u8,
    pub term: u32,
    /// The coordinating device: the only one that commits in steady state.
    pub lead: Pid,
    pub owners: Vec<Pid>,
    pub admins: Vec<Pid>,
    /// One admin account's devices that may claim ownership (empty: none).
    pub successors: Vec<Pid>,
}

/// One change to a roster, as owners (and, for stepping down, admins) ask
/// for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RosterChange {
    Promote(Vec<Pid>),
    Demote(Vec<Pid>),
    /// More owner devices (the owner's own newly added devices).
    AddOwners(Vec<Pid>),
    /// Hand ownership to these devices (one account's); the first leads.
    /// The old owners become admins.
    Transfer(Vec<Pid>),
    /// The successor's devices; empty clears it.
    Successor(Vec<Pid>),
    /// Make this owner device the coordinator.
    Lead(Pid),
}

impl RosterChange {
    pub(crate) fn kind(&self) -> u8 {
        match self {
            Self::Promote(_) => 0,
            Self::Demote(_) => 1,
            Self::AddOwners(_) => 2,
            Self::Transfer(_) => 3,
            Self::Successor(_) => 4,
            Self::Lead(_) => 5,
        }
    }

    pub(crate) fn devices(&self) -> Vec<Pid> {
        match self {
            Self::Promote(d)
            | Self::Demote(d)
            | Self::AddOwners(d)
            | Self::Transfer(d)
            | Self::Successor(d) => d.clone(),
            Self::Lead(d) => vec![*d],
        }
    }

    pub(crate) fn from_parts(kind: u8, devices: Vec<Pid>) -> Option<Self> {
        Some(match kind {
            0 => Self::Promote(devices),
            1 => Self::Demote(devices),
            2 => Self::AddOwners(devices),
            3 => Self::Transfer(devices),
            4 => Self::Successor(devices),
            5 => Self::Lead(*devices.first()?),
            _ => return None,
        })
    }
}

fn sort(v: &mut Vec<Pid>) {
    v.sort_by_key(|p| *p.as_bytes());
    v.dedup();
}

fn sorted(v: &[Pid]) -> bool {
    v.windows(2).all(|w| w[0].as_bytes() < w[1].as_bytes())
}

fn not_allowed(why: &'static str) -> GroupError {
    GroupError::NotAllowed(why)
}

/// How branches of a forked group are ordered: the higher term wins, and on
/// equal terms the branch whose lead has the *lower* identity bytes.
/// Leads are compared by their first 8 bytes, as the sync hint carries them.
pub type Rank = (u32, Reverse<[u8; 8]>);

pub(crate) fn lead_prefix(lead: &Pid) -> [u8; 8] {
    let mut out = [0; 8];
    out.copy_from_slice(&lead.as_bytes()[..8]);
    out
}

impl Roster {
    /// A group without a roster: its creator owns it and commits alone.
    pub fn legacy(owner: Pid) -> Self {
        Self {
            version: ROSTER_VERSION,
            term: 0,
            lead: owner,
            owners: vec![owner],
            admins: Vec::new(),
            successors: Vec::new(),
        }
    }

    pub fn role(&self, who: &Pid) -> MemberRole {
        if self.owners.contains(who) {
            MemberRole::Owner
        } else if self.admins.contains(who) {
            MemberRole::Admin
        } else {
            MemberRole::Member
        }
    }

    /// Written by a newer version than this one: only its lead is
    /// followed, and the user should update.
    pub fn update_needed(&self) -> bool {
        self.version > ROSTER_VERSION
    }

    pub fn rank(&self) -> Rank {
        (self.term, Reverse(lead_prefix(&self.lead)))
    }

    pub fn encode(&self) -> Vec<u8> {
        let list = |e: &mut const_cbor::Encoder<'_>,
                    v: &[Pid]|
         -> core::result::Result<(), const_cbor::Error> {
            e.array_len(v.len())?;
            for p in v {
                e.bytes(p.as_bytes())?;
            }
            Ok(())
        };
        let size = 64 + 36 * (self.owners.len() + self.admins.len() + self.successors.len());
        cbor::to_vec(size, |e| {
            e.map_len(5 + usize::from(!self.successors.is_empty()))?;
            e.u8(0)?.u8(self.version)?;
            e.u8(1)?.u32(self.term)?;
            e.u8(2)?.bytes(self.lead.as_bytes())?;
            e.u8(3)?;
            list(e, &self.owners)?;
            e.u8(4)?;
            list(e, &self.admins)?;
            if !self.successors.is_empty() {
                e.u8(5)?;
                list(e, &self.successors)?;
            }
            Ok(())
        })
        .unwrap_or_default()
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let malformed = |what| GroupError::Core(threnody_core::Error::Malformed(what));
        let mut dec = Decoder::new(b);
        let mut version: Option<u8> = None;
        let mut last: Option<u64> = None;
        let (mut term, mut lead, mut owners) = (None, None, None);
        let (mut admins, mut successors) = (None, None);
        let list = |d: &mut Decoder<'_>| -> threnody_core::Result<Vec<Pid>> {
            let n = d.array_len()?;
            if n > MAX_ADMINS {
                return Err(threnody_core::Error::Malformed("roster list too long"));
            }
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                out.push(Pid::from_bytes(&cbor::fixed_bytes::<32>(d)?)?);
            }
            Ok(out)
        };
        read_map(&mut dec, |k, d| {
            let strict = version.is_none_or(|v| v <= ROSTER_VERSION);
            if strict && last.is_some_and(|l| k <= l) {
                return Err(threnody_core::Error::Malformed("roster keys out of order"));
            }
            last = Some(k);
            match k {
                0 => version = Some(d.u8()?),
                _ if version.is_none() => {
                    return Err(threnody_core::Error::Malformed("roster without version"));
                }
                1 => term = Some(d.u32()?),
                2 => lead = Some(Pid::from_bytes(&cbor::fixed_bytes::<32>(d)?)?),
                3 => owners = Some(list(d)?),
                4 if strict => admins = Some(list(d)?),
                5 if strict => successors = Some(list(d)?),
                _ if strict => {
                    return Err(threnody_core::Error::Malformed("unknown roster key"));
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let version = version.ok_or(malformed("roster version"))?;
        if version == 0 {
            return Err(malformed("roster version"));
        }
        let r = Self {
            version,
            term: term.ok_or(malformed("roster term"))?,
            lead: lead.ok_or(malformed("roster lead"))?,
            owners: owners.ok_or(malformed("roster owners"))?,
            admins: if version > ROSTER_VERSION {
                Vec::new()
            } else {
                admins.ok_or(malformed("roster admins"))?
            },
            successors: successors.clone().unwrap_or_default(),
        };
        if successors.as_ref().is_some_and(Vec::is_empty) {
            // Absent means none: an empty list isn't deterministic.
            return Err(malformed("empty successor list"));
        }
        r.check()?;
        Ok(r)
    }

    /// The structural rules, without the members.
    fn check(&self) -> Result<()> {
        if self.owners.is_empty() || self.owners.len() > MAX_OWNERS {
            return Err(not_allowed("a group needs 1 to 16 owner devices"));
        }
        if !sorted(&self.owners) || !self.owners.contains(&self.lead) {
            return Err(not_allowed("the coordinator must be an owner device"));
        }
        if self.update_needed() {
            return Ok(());
        }
        if self.admins.len() > MAX_ADMINS || !sorted(&self.admins) {
            return Err(not_allowed("too many admins"));
        }
        if self.admins.iter().any(|a| self.owners.contains(a)) {
            return Err(not_allowed("an owner can't also be an admin"));
        }
        if self.successors.len() > MAX_OWNERS
            || !sorted(&self.successors)
            || self.successors.iter().any(|s| !self.admins.contains(s))
        {
            return Err(not_allowed("the successor must be an admin"));
        }
        Ok(())
    }

    /// [`Self::check`], and every device it names is a member.
    pub fn validate(&self, members: &[Pid]) -> Result<()> {
        self.check()?;
        let named = self
            .owners
            .iter()
            .chain(&self.admins)
            .chain(&self.successors);
        for p in named {
            if !members.contains(p) {
                return Err(not_allowed("the roster names someone not in the group"));
            }
        }
        Ok(())
    }

    /// The roster after `changes`, in order. The term goes up by one if
    /// the coordinator changed.
    pub fn apply(&self, changes: &[RosterChange]) -> Result<Roster> {
        let mut r = self.clone();
        for c in changes {
            match c {
                RosterChange::Promote(ds) => {
                    for d in ds {
                        if r.owners.contains(d) {
                            return Err(not_allowed("owners already have every power"));
                        }
                        if !r.admins.contains(d) {
                            r.admins.push(*d);
                        }
                    }
                    sort(&mut r.admins);
                    if r.admins.len() > self.admins.len().max(MAX_PROMOTED) {
                        return Err(not_allowed("a group can have at most 64 admins"));
                    }
                }
                RosterChange::Demote(ds) => {
                    if ds.iter().any(|d| r.owners.contains(d)) {
                        return Err(not_allowed("transfer ownership instead"));
                    }
                    r.admins.retain(|a| !ds.contains(a));
                    r.successors.retain(|s| !ds.contains(s));
                }
                RosterChange::AddOwners(ds) => {
                    for d in ds {
                        if !r.owners.contains(d) {
                            r.owners.push(*d);
                        }
                    }
                    sort(&mut r.owners);
                    r.admins.retain(|a| !ds.contains(a));
                    r.successors.retain(|s| !ds.contains(s));
                }
                RosterChange::Transfer(ds) => {
                    let Some(first) = ds.first() else {
                        return Err(not_allowed("transfer to whom?"));
                    };
                    let mut admins = r.admins.clone();
                    admins.extend(r.owners.iter().copied());
                    admins.retain(|a| !ds.contains(a));
                    sort(&mut admins);
                    if r.successors.iter().any(|s| ds.contains(s)) {
                        r.successors.clear();
                    }
                    r.lead = *first;
                    r.owners = ds.clone();
                    sort(&mut r.owners);
                    r.admins = admins;
                }
                RosterChange::Successor(ds) => {
                    if ds.iter().any(|d| !r.admins.contains(d)) {
                        return Err(not_allowed("the successor must be an admin"));
                    }
                    r.successors = ds.clone();
                    sort(&mut r.successors);
                }
                RosterChange::Lead(d) => {
                    if !r.owners.contains(d) {
                        return Err(not_allowed("only an owner device can coordinate"));
                    }
                    r.lead = *d;
                }
            }
        }
        r.term = self.term + u32::from(r.lead != self.lead);
        r.check()?;
        Ok(r)
    }

    /// The roster once successor device `who` claims ownership: the
    /// successor's devices own the group, the old owners become admins.
    pub fn claimed_by(&self, who: Pid) -> Roster {
        let mut admins: Vec<Pid> = self
            .admins
            .iter()
            .chain(&self.owners)
            .copied()
            .filter(|a| !self.successors.contains(a))
            .collect();
        sort(&mut admins);
        Roster {
            version: self.version,
            term: self.term + 1,
            lead: who,
            owners: self.successors.clone(),
            admins,
            successors: Vec::new(),
        }
    }

    /// The roster with `removed` devices dropped from every role (not the
    /// lead: a coordinator change comes first).
    pub fn without(&self, removed: &[Pid]) -> Roster {
        let mut r = self.clone();
        r.owners.retain(|p| !removed.contains(p) || *p == r.lead);
        r.admins.retain(|p| !removed.contains(p));
        r.successors.retain(|p| !removed.contains(p));
        r
    }
}

/// The GroupContext extensions for `roster`: `RequiredCapabilities` naming
/// the roster extension, and the roster. A GroupContextExtensions proposal
/// replaces the whole set, so every roster change carries both.
pub(crate) fn roster_extensions(roster: &Roster) -> Result<Extensions<GroupContext>> {
    let required =
        RequiredCapabilitiesExtension::new(&[ExtensionType::Unknown(ROSTER_EXTENSION)], &[], &[]);
    Extensions::from_vec(vec![
        Extension::RequiredCapabilities(required),
        Extension::Unknown(ROSTER_EXTENSION, UnknownExtension(roster.encode())),
    ])
    .map_err(crate::mls)
}

/// The roster in a GroupContext's extensions, if there is one.
pub(crate) fn roster_from(extensions: &Extensions<GroupContext>) -> Option<Result<Roster>> {
    extensions
        .unknown(ROSTER_EXTENSION)
        .map(|u| Roster::decode(&u.0))
}

/// The roster a GroupContextExtensions proposal sets, if it sets exactly
/// `RequiredCapabilities([0xFA01])` and a roster, and nothing else.
pub(crate) fn proposed_roster(extensions: &Extensions<GroupContext>) -> Option<Roster> {
    let mut n = 0;
    for e in extensions.iter() {
        n += 1;
        match e {
            Extension::RequiredCapabilities(r) => {
                if r.extension_types() != [ExtensionType::Unknown(ROSTER_EXTENSION)]
                    || !r.proposal_types().is_empty()
                    || !r.credential_types().is_empty()
                {
                    return None;
                }
            }
            Extension::Unknown(ROSTER_EXTENSION, _) => {}
            _ => return None,
        }
    }
    if n != 2 {
        return None;
    }
    roster_from(extensions)?.ok()
}

/// A proposal as the rules see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProposalKind {
    Add,
    /// Removes this member.
    Remove(Pid),
    /// The proposer's own leaf.
    Update,
    /// A GroupContextExtensions proposal setting this roster (and nothing
    /// else).
    Roster(Roster),
    /// Anything else (PSK, ReInit, ExternalInit, other extensions): never
    /// allowed.
    Other,
}

/// Whether `proposer` may propose `p` under `before`.
pub(crate) fn authorize_proposal(before: &Roster, proposer: &Pid, p: &ProposalKind) -> Result<()> {
    if before.update_needed() {
        return Err(GroupError::Unexpected("roster from a newer version"));
    }
    let role = before.role(proposer);
    match p {
        ProposalKind::Update => Ok(()),
        ProposalKind::Add if role.can_manage_members() => Ok(()),
        ProposalKind::Add => Err(GroupError::NotOwner),
        ProposalKind::Remove(t) if t == proposer => Ok(()),
        ProposalKind::Remove(t) => match role {
            MemberRole::Owner if *t != before.lead => Ok(()),
            MemberRole::Owner => Err(not_allowed("take over as coordinator first")),
            MemberRole::Admin if before.role(t) == MemberRole::Member => Ok(()),
            MemberRole::Admin => Err(not_allowed("admins can't remove admins or owners")),
            MemberRole::Member => Err(GroupError::NotOwner),
        },
        ProposalKind::Roster(_) if role == MemberRole::Owner => Ok(()),
        ProposalKind::Roster(r) if role == MemberRole::Admin => {
            let down = before.apply(&[RosterChange::Demote(vec![*proposer])])?;
            if *r == down {
                Ok(())
            } else {
                Err(not_allowed("admins can only step down"))
            }
        }
        ProposalKind::Roster(_) => Err(GroupError::NotOwner),
        ProposalKind::Other => Err(GroupError::Unexpected("proposal kind not used")),
    }
}

/// How a valid commit came about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitKind {
    /// By the coordinator, as every commit in steady state.
    Coordinator,
    /// An owner device took over as coordinator.
    Takeover,
    /// The successor claimed ownership.
    Claim,
}

/// One proposal in a commit: who proposed it, what it is, and whether it
/// was committed by reference (sent on its own first) or by value.
pub(crate) type CommitProposal = (Pid, ProposalKind, bool);

/// Checks a commit by `committer` that takes the group from roster
/// `before` to `after` with `proposals`, leaving `members_after`.
pub(crate) fn check_commit(
    before: &Roster,
    after: &Roster,
    committer: &Pid,
    proposals: &[CommitProposal],
    members_after: &[Pid],
) -> Result<CommitKind> {
    if before.update_needed() {
        // A newer version's rules: follow its lead only.
        return if *committer == before.lead {
            Ok(CommitKind::Coordinator)
        } else {
            Err(not_allowed("not the group's coordinator"))
        };
    }
    after.validate(members_after)?;
    let by_ref = proposals.iter().any(|(_, _, r)| *r);
    let term_up = after.term == before.term.saturating_add(1);
    if *committer == before.lead {
        for (p, k, r) in proposals {
            if *r {
                authorize_proposal(before, p, k)?;
            } else if *k == ProposalKind::Other {
                return Err(GroupError::Unexpected("proposal kind not used"));
            }
        }
        let term_ok = if after.lead == before.lead {
            after.term == before.term
        } else {
            term_up
        };
        if !term_ok {
            return Err(not_allowed("the term must go up with a new coordinator"));
        }
        Ok(CommitKind::Coordinator)
    } else if before.owners.contains(committer) {
        if by_ref {
            return Err(not_allowed("a takeover commits only its own changes"));
        }
        if proposals.iter().any(|(_, k, _)| *k == ProposalKind::Other) {
            return Err(GroupError::Unexpected("proposal kind not used"));
        }
        if after.lead != *committer || !term_up {
            return Err(not_allowed("a takeover makes its committer coordinator"));
        }
        Ok(CommitKind::Takeover)
    } else if before.successors.contains(committer) {
        let only_gce = proposals
            .iter()
            .all(|(_, k, r)| !*r && matches!(k, ProposalKind::Roster(_)));
        if !only_gce || *after != before.claimed_by(*committer) {
            return Err(not_allowed("a claim changes only who owns the group"));
        }
        Ok(CommitKind::Claim)
    } else {
        Err(not_allowed("not the group's coordinator"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use threnody_core::Identity;

    /// `n` identities in ascending byte order.
    fn ids(n: usize) -> Vec<Pid> {
        let mut v: Vec<Pid> = (0..n).map(|_| Identity::generate().public()).collect();
        sort(&mut v);
        v
    }

    fn roster(owners: &[Pid], admins: &[Pid]) -> Roster {
        let mut r = Roster::legacy(owners[0]);
        r.owners = owners.to_vec();
        sort(&mut r.owners);
        r.admins = admins.to_vec();
        sort(&mut r.admins);
        r
    }

    /// The roster as CBOR with `f` editing the parts first.
    fn raw(r: &Roster, extra: Option<(u8, u8)>) -> Vec<u8> {
        let list = |e: &mut const_cbor::Encoder<'_>,
                    v: &[Pid]|
         -> core::result::Result<(), const_cbor::Error> {
            e.array_len(v.len())?;
            for p in v {
                e.bytes(p.as_bytes())?;
            }
            Ok(())
        };
        cbor::to_vec(4096, |e| {
            e.map_len(5 + usize::from(extra.is_some()))?;
            e.u8(0)?.u8(r.version)?;
            e.u8(1)?.u32(r.term)?;
            e.u8(2)?.bytes(r.lead.as_bytes())?;
            e.u8(3)?;
            list(e, &r.owners)?;
            e.u8(4)?;
            list(e, &r.admins)?;
            if let Some((k, v)) = extra {
                e.u8(k)?.u8(v)?;
            }
            Ok(())
        })
        .unwrap()
    }

    #[test]
    fn roster_round_trips_and_rejects_malformed() {
        let p = ids(6);
        let mut r = roster(&p[..2], &p[2..5]);
        r.successors = vec![p[3]];
        r.term = 7;
        assert_eq!(Roster::decode(&r.encode()).unwrap(), r);
        let plain = roster(&p[..1], &[]);
        assert_eq!(Roster::decode(&plain.encode()).unwrap(), plain);

        let bad = |r: &Roster| Roster::decode(&r.encode()).is_err();
        // Unknown key.
        assert!(Roster::decode(&raw(&plain, Some((9, 1)))).is_err());
        // Unsorted or duplicate owners.
        let mut x = plain.clone();
        x.owners = vec![p[1], p[0]];
        assert!(bad(&x));
        x.owners = vec![p[0], p[0]];
        assert!(bad(&x));
        // An owner who is also an admin.
        let mut x = roster(&p[..1], &p[..2]);
        x.lead = p[0];
        assert!(bad(&x));
        // Lead not an owner.
        let mut x = plain.clone();
        x.lead = p[1];
        assert!(bad(&x));
        // Successor not an admin.
        let mut x = roster(&p[..1], &p[1..2]);
        x.successors = vec![p[2]];
        assert!(bad(&x));
        // Too many owners or admins, or none.
        let many = ids(MAX_ADMINS + 2);
        assert!(bad(&roster(&many[..MAX_OWNERS + 1], &[])));
        assert!(bad(&roster(&many[..1], &many[1..])));
        let mut x = plain.clone();
        x.owners.clear();
        assert!(bad(&x));
        // Members: everyone named must be in the group.
        let r = roster(&p[..1], &p[1..2]);
        assert!(r.validate(&p[..2]).is_ok());
        assert!(r.validate(&p[..1]).is_err());
        assert!(Roster::decode(b"\xa0").is_err());
    }

    #[test]
    fn later_roster_versions_decode_leniently() {
        let p = ids(3);
        let mut r = roster(&p[..1], &p[1..2]);
        r.version = 2;
        let decoded = Roster::decode(&raw(&r, Some((9, 1)))).unwrap();
        assert!(decoded.update_needed());
        assert_eq!((decoded.lead, &decoded.owners[..]), (p[0], &p[..1]));
        assert!(decoded.admins.is_empty(), "keys past 3 are skipped");
        r.version = 1;
        assert!(Roster::decode(&raw(&r, Some((9, 1)))).is_err());
        // Only its lead commits, without other checks.
        let other = roster(&p[..1], &[]);
        assert!(check_commit(&decoded, &other, &p[0], &[], &p).is_ok());
        assert!(check_commit(&decoded, &other, &p[1], &[], &p).is_err());
    }

    #[test]
    fn roster_changes_follow_the_rules() {
        let p = ids(8);
        let (a, b, c, d) = (p[0], p[1], p[2], p[3]);
        let r = roster(&[a], &[]);
        let r1 = r.apply(&[RosterChange::Promote(vec![b, c])]).unwrap();
        assert_eq!((r1.admins.clone(), r1.term), (vec![b, c], 0));
        let r2 = r1.apply(&[RosterChange::Successor(vec![b])]).unwrap();
        assert_eq!(r2.successors, vec![b]);
        // Demoting a successor drops it from the list.
        let r3 = r2.apply(&[RosterChange::Demote(vec![b])]).unwrap();
        assert!(r3.successors.is_empty() && r3.admins == vec![c]);
        // The successor must be an admin; owners can't be demoted.
        assert!(r1.apply(&[RosterChange::Successor(vec![d])]).is_err());
        assert!(r1.apply(&[RosterChange::Demote(vec![a])]).is_err());
        // Transfer: old owners become admins, the term goes up.
        let t = r2.apply(&[RosterChange::Transfer(vec![c, d])]).unwrap();
        assert_eq!(t.lead, c);
        assert_eq!(t.term, 1);
        let mut want = vec![c, d];
        sort(&mut want);
        assert_eq!(t.owners, want);
        assert!(t.admins.contains(&a) && t.admins.contains(&b));
        // Lead: another owner device coordinates, the term goes up.
        let two = r.apply(&[RosterChange::AddOwners(vec![d])]).unwrap();
        let l = two.apply(&[RosterChange::Lead(d)]).unwrap();
        assert_eq!((l.lead, l.term), (d, 1));
        assert!(r.apply(&[RosterChange::Lead(b)]).is_err());
        // Claim: every successor device owns it, the old owners are admins.
        let mut s = roster(&[a, d], &[b, c, p[4]]);
        s.successors = vec![b, c];
        let claimed = s.claimed_by(c);
        assert_eq!(claimed.owners, vec![b, c]);
        assert_eq!(claimed.lead, c);
        assert_eq!(claimed.term, 1);
        assert!(claimed.admins.contains(&a) && claimed.admins.contains(&d));
        assert!(claimed.successors.is_empty());
        assert!(claimed.validate(&p).is_ok());
        // Removing devices drops them from every role.
        let w = s.without(&[b, d]);
        assert_eq!(w.successors, vec![c]);
        assert_eq!(w.owners, vec![a]);
        // At most 64 promoted, yet a transfer or claim with 64 admins and
        // 16 owners still fits.
        let many = ids(MAX_ADMINS + 1);
        let owners = &many[..MAX_OWNERS];
        let admins = &many[MAX_OWNERS..MAX_OWNERS + MAX_PROMOTED];
        let full = roster(owners, admins);
        assert!(
            full.apply(&[RosterChange::Promote(vec![many[MAX_ADMINS]])])
                .is_err()
        );
        let to = full
            .apply(&[RosterChange::Transfer(vec![admins[0]])])
            .unwrap();
        assert_eq!(to.admins.len(), MAX_ADMINS - 1);
        let mut s = full.clone();
        s.successors = vec![admins[0]];
        assert!(s.claimed_by(admins[0]).validate(&many).is_ok());
    }

    #[test]
    fn rank_breaks_ties_by_lead() {
        let p = ids(2);
        let mut lo = Roster::legacy(p[0]);
        let mut hi = Roster::legacy(p[1]);
        lo.term = 3;
        hi.term = 3;
        assert!(lo.rank() > hi.rank(), "equal terms: the lower lead wins");
        hi.term = 4;
        assert!(hi.rank() > lo.rank(), "a higher term wins");
    }

    #[test]
    fn roster_extensions_always_carry_required_capabilities() {
        let p = ids(2);
        let r = roster(&p[..1], &p[1..]);
        let e = roster_extensions(&r).unwrap();
        assert!(
            e.required_capabilities()
                .is_some_and(|c| c.extension_types() == [ExtensionType::Unknown(ROSTER_EXTENSION)])
        );
        assert_eq!(roster_from(&e).unwrap().unwrap(), r);
        assert_eq!(proposed_roster(&e), Some(r));
        // A roster without its RequiredCapabilities is not a valid change.
        let bare = Extensions::single(Extension::Unknown(
            ROSTER_EXTENSION,
            UnknownExtension(Roster::legacy(p[0]).encode()),
        ))
        .unwrap();
        assert_eq!(proposed_roster(&bare), None);
    }

    #[test]
    fn proposals_are_authorized_by_role() {
        let p = ids(6);
        let (owner, owner2, admin, admin2, member, member2) = (p[0], p[1], p[2], p[3], p[4], p[5]);
        let mut r = roster(&[owner, owner2], &[admin, admin2]);
        r.lead = owner;
        use ProposalKind::*;
        let ok = |who: &Pid, k: ProposalKind| authorize_proposal(&r, who, &k).is_ok();
        // Adds: owners and admins.
        assert!(ok(&owner2, Add) && ok(&admin, Add) && !ok(&member, Add));
        // Removes.
        assert!(ok(&owner2, Remove(admin)), "an owner removes an admin");
        assert!(ok(&owner2, Remove(member)));
        assert!(!ok(&owner2, Remove(owner)), "not the coordinator");
        assert!(ok(&admin, Remove(member)), "an admin removes a member");
        assert!(
            !ok(&admin, Remove(admin2)),
            "an admin can't remove an admin"
        );
        assert!(!ok(&admin, Remove(owner)), "nor the owner");
        assert!(!ok(&admin, Remove(owner2)));
        assert!(!ok(&member, Remove(member2)));
        assert!(ok(&member, Remove(member)), "anyone leaves");
        // Updates: anyone's own leaf.
        assert!(ok(&member, Update));
        // Roster changes: owners; admins only step down.
        let promote = r.apply(&[RosterChange::Promote(vec![member])]).unwrap();
        assert!(ok(&owner2, Roster(promote.clone())));
        assert!(
            !ok(&admin, Roster(promote.clone())),
            "an admin can't promote"
        );
        assert!(!ok(&member, Roster(promote)));
        let down = r.apply(&[RosterChange::Demote(vec![admin])]).unwrap();
        assert!(ok(&admin, Roster(down.clone())), "an admin steps down");
        assert!(!ok(&admin2, Roster(down)), "but doesn't demote another");
        assert!(!ok(&owner, Other));
    }

    #[test]
    fn commits_are_checked_against_the_roster_before_them() {
        let p = ids(6);
        let (lead, owner2, admin, admin2, member, succ) = (p[0], p[1], p[2], p[3], p[4], p[5]);
        let mut before = roster(&[lead, owner2], &[admin, admin2, succ]);
        before.lead = lead;
        before.successors = vec![succ];
        let all = p.clone();
        use ProposalKind::*;
        // The coordinator commits an admin's remove of a member.
        let removed: Vec<Pid> = all.iter().copied().filter(|x| *x != member).collect();
        assert_eq!(
            check_commit(
                &before,
                &before,
                &lead,
                &[(admin, Remove(member), true)],
                &removed
            )
            .unwrap(),
            CommitKind::Coordinator
        );
        // ...but not an admin's remove of an admin.
        let without_admin2 = before.without(&[admin2]);
        let rest: Vec<Pid> = all.iter().copied().filter(|x| *x != admin2).collect();
        assert!(
            check_commit(
                &before,
                &without_admin2,
                &lead,
                &[(admin, Remove(admin2), true)],
                &rest
            )
            .is_err()
        );
        // The roster after must cover removals.
        assert!(
            check_commit(
                &before,
                &before,
                &lead,
                &[(lead, Remove(admin2), false)],
                &rest
            )
            .is_err()
        );
        assert!(
            check_commit(
                &before,
                &without_admin2,
                &lead,
                &[(lead, Remove(admin2), false)],
                &rest
            )
            .is_ok()
        );
        // A non-coordinator admin's commit is refused.
        assert!(check_commit(&before, &before, &admin, &[], &all).is_err());
        assert!(check_commit(&before, &before, &member, &[], &all).is_err());
        // Takeover: the term goes up by one, its committer leads, and it
        // commits nothing by reference.
        let took = before.apply(&[RosterChange::Lead(owner2)]).unwrap();
        assert_eq!(
            check_commit(
                &before,
                &took,
                &owner2,
                &[(owner2, Roster(took.clone()), false)],
                &all
            )
            .unwrap(),
            CommitKind::Takeover
        );
        let mut same_term = took.clone();
        same_term.term = before.term;
        assert!(check_commit(&before, &same_term, &owner2, &[], &all).is_err());
        assert!(
            check_commit(
                &before,
                &took,
                &owner2,
                &[(admin, Remove(member), true)],
                &removed
            )
            .is_err()
        );
        // The coordinator handing over bumps the term too.
        assert!(check_commit(&before, &took, &lead, &[], &all).is_ok());
        assert!(check_commit(&before, &same_term, &lead, &[], &all).is_err());
        // Claim: exactly the claimed roster.
        let claimed = before.claimed_by(succ);
        assert_eq!(
            check_commit(
                &before,
                &claimed,
                &succ,
                &[(succ, Roster(claimed.clone()), false)],
                &all
            )
            .unwrap(),
            CommitKind::Claim
        );
        let mut more = claimed.clone();
        more.successors = vec![admin];
        assert!(check_commit(&before, &more, &succ, &[], &all).is_err());
        assert!(
            check_commit(
                &before,
                &claimed,
                &succ,
                &[(succ, Remove(member), false)],
                &removed
            )
            .is_err()
        );
        // Legacy groups: only the owner commits, and others' proposals are
        // limited to their own leaves.
        let legacy = super::Roster::legacy(lead);
        assert!(check_commit(&legacy, &legacy, &lead, &[(member, Update, true)], &all).is_ok());
        assert!(check_commit(&legacy, &legacy, &member, &[], &all).is_err());
        assert!(
            check_commit(
                &legacy,
                &legacy,
                &lead,
                &[(member, Remove(admin), true)],
                &removed
            )
            .is_err()
        );
    }
}
