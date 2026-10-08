//! MLS groups for Threnody (spec §6.2) on openmls.
//!
//! - Ciphersuite `MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519`: X-Wing
//!   HPKE, so group key agreement is post-quantum hybrid like the 1:1
//!   protocol; Ed25519 signatures with each device's Threnody identity key.
//! - Every member's credential must be its own signature key, so an MLS
//!   member *is* a Threnody identity; anything else is rejected.
//! - There is no server. Group messages travel as [`GroupWire`] inside the
//!   1:1 ratchets, fanned out by the sender to every other member.
//! - Membership policy (spec §6.2 "application layer"): the creator is the
//!   group's owner and the only committer. That avoids concurrent-commit
//!   forks without a delivery service; members send application messages.
//!
//! The manager is sans-IO: callers feed it messages and deliver what it
//! returns. See `docs/appendix-f-groups.md`.

#[cfg(feature = "node")]
pub mod node;
pub mod wire;

use std::collections::HashMap;

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::{Signer, SignerError};
use threnody_core::crypto::random_bytes;
use threnody_core::{Identity, PublicIdentity};

pub use wire::{Content, GroupId, GroupWire};

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519;
const STATE_VERSION: u8 = 1;
/// How many past epochs' application messages can still be decrypted.
const MAX_PAST_EPOCHS: usize = 5;
/// Group names are display labels; keep them short.
pub const MAX_NAME: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("no such group")]
    UnknownGroup,
    #[error("only the group owner can change membership")]
    NotOwner,
    #[error("{0} is not a member of this group")]
    NotMember(String),
    #[error("{0} is already a member of this group")]
    AlreadyMember(String),
    #[error("credential is not a Threnody identity")]
    BadCredential,
    #[error("unexpected message: {0}")]
    Unexpected(&'static str),
    #[error("mls: {0}")]
    Mls(String),
    #[error(transparent)]
    Core(#[from] threnody_core::Error),
    /// The file couldn't be prepared to send (too large, or a damaged
    /// image whose metadata couldn't be removed).
    #[error("{0}")]
    File(String),
}

fn mls<E: std::fmt::Debug>(e: E) -> GroupError {
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
    /// We were removed, or the group was otherwise closed for us.
    Left { group: GroupId },
    /// `id` is the sender's id for the message (0 from older members).
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
        id: u64,
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
    },
    /// `from` added (or took away) its `emoji` on message `id`.
    React {
        group: GroupId,
        from: PublicIdentity,
        id: u64,
        emoji: String,
        add: bool,
    },
}

/// Results of handling one input.
#[derive(Debug, Default)]
pub struct Output {
    pub send: Vec<Outgoing>,
    pub events: Vec<GroupEvent>,
}

/// Internal group state.
struct Group {
    mls: MlsGroup,
    name: String,
    owner: PublicIdentity,
    /// Member roles: maps PublicIdentity -> MemberRole
    roles: HashMap<PublicIdentity, MemberRole>,
}

/// Member role in a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberRole {
    Owner,
    Admin,
    Member,
}

impl MemberRole {
    /// Returns true if this role can add/remove members.
    pub fn can_commit(self) -> bool {
        matches!(self, MemberRole::Owner | MemberRole::Admin)
    }

    /// Returns true if this role can promote/demote other members.
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

impl Group {
    fn new(mls: MlsGroup, name: String, owner: PublicIdentity) -> Self {
        let mut roles = HashMap::new();
        roles.insert(owner, MemberRole::Owner);
        Self {
            mls,
            name,
            owner,
            roles,
        }
    }

    fn role(&self, member: &PublicIdentity) -> MemberRole {
        self.roles
            .get(member)
            .copied()
            .unwrap_or(MemberRole::Member)
    }

    fn set_role(&mut self, member: PublicIdentity, role: MemberRole) {
        if role == MemberRole::Owner {
            // Only one owner allowed
            self.roles.retain(|_, r| *r != MemberRole::Owner);
        }
        self.roles.insert(member, role);
    }

    fn committer_role(&self) -> MemberRole {
        self.role(&self.owner)
    }
}

pub struct Groups {
    provider: OpenMlsRustCrypto,
    signer: IdentitySigner,
    me: PublicIdentity,
    groups: HashMap<GroupId, Group>,
    /// Invitations we sent and are waiting on a key package for.
    pending_adds: Vec<(GroupId, PublicIdentity)>,
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

impl Groups {
    pub fn new(identity: &Identity) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: IdentitySigner(Identity::from_seed(&identity.seed())),
            me: identity.public(),
            groups: HashMap::new(),
            pending_adds: Vec::new(),
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
        Ok(threnody_core::cbor::to_vec(size, |e| {
            e.map_len(3)?;
            e.u8(0)?.u8(STATE_VERSION)?;
            e.u8(1)?.array_len(values.len())?;
            for (k, v) in values.iter() {
                e.array_len(2)?.bytes(k)?.bytes(v)?;
            }
            e.u8(2)?.array_len(self.groups.len())?;
            for (id, g) in &self.groups {
                e.array_len(4)?
                    .bytes(id)?
                    .str(&g.name)?
                    .bytes(g.owner.as_bytes())?
                    .map_len(g.roles.len())?;
                for (member, role) in &g.roles {
                    e.bytes(member.as_bytes())?.u8(*role as u8)?;
                }
            }
            Ok(())
        })?)
    }

    /// Rebuilds the manager from [`Groups::export`] output.
    pub fn restore(identity: &Identity, bytes: &[u8]) -> Result<Self> {
        use threnody_core::cbor::{fixed_bytes, read_map};
        let mut me = Self::new(identity);
        let mut dec = const_cbor::Decoder::new(bytes);
        let mut meta: Vec<(
            GroupId,
            String,
            [u8; 32],
            HashMap<PublicIdentity, MemberRole>,
        )> = Vec::new();
        let mut version = None;
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
                                return Err(threnody_core::Error::Malformed("storage entry"));
                            }
                            let key = d.bytes()?.to_vec();
                            values.insert(key, d.bytes()?.to_vec());
                        }
                    }
                    2 => {
                        for _ in 0..d.array_len()? {
                            let mut roles = HashMap::new();
                            if d.array_len()? != 4 {
                                return Err(threnody_core::Error::Malformed("group entry"));
                            }
                            let id = fixed_bytes::<16>(d)?;
                            let name = d.str()?.to_owned();
                            let owner = fixed_bytes::<32>(d)?;
                            let role_count = d.map_len()?;
                            for _ in 0..role_count {
                                let member = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?)
                                    .map_err(|_| {
                                        threnody_core::Error::Malformed("member identity")
                                    })?;
                                let role = MemberRole::from_wire(d.u8()?);
                                roles.insert(member, role);
                            }
                            meta.push((id, name, owner, roles));
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
        for (id, name, owner, roles) in meta {
            let mls = MlsGroup::load(
                me.provider.storage(),
                &openmls::group::GroupId::from_slice(&id),
            )
            .map_err(mls)?
            .ok_or(GroupError::Unexpected("group missing from storage"))?;
            let owner = PublicIdentity::from_bytes(&owner)?;
            let mut group = Group::new(mls, name, owner);
            group.roles = roles;
            me.groups.insert(id, group);
        }
        Ok(me)
    }

    /// `(id, name, owner, members)` for every group we are in.
    pub fn list(&self) -> Vec<(GroupId, String, PublicIdentity, Vec<PublicIdentity>)> {
        self.groups
            .iter()
            .map(|(id, g)| (*id, g.name.clone(), g.owner, self.members_of(g)))
            .collect()
    }

    /// `(id, name, owner, members, roles)` for every group we are in.
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
                let members = self.members_of(g);
                let roles = members.iter().map(|m| g.role(m)).collect();
                (*id, g.name.clone(), g.owner, members, roles)
            })
            .collect()
    }

    fn members_of(&self, g: &Group) -> Vec<PublicIdentity> {
        g.mls
            .members()
            .filter_map(|m| identity_of(&m.credential, &m.signature_key).ok())
            .collect()
    }

    fn group(&self, id: &GroupId) -> Result<&Group> {
        self.groups.get(id).ok_or(GroupError::UnknownGroup)
    }

    /// Creates a group owned by us.
    pub fn create(&mut self, name: &str) -> Result<GroupId> {
        let id: GroupId = random_bytes();
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(CIPHERSUITE)
            .use_ratchet_tree_extension(true)
            .max_past_epochs(MAX_PAST_EPOCHS)
            .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
            .build();
        let mls = MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &config,
            openmls::group::GroupId::from_slice(&id),
            credential_for(&self.me),
        )
        .map_err(mls)?;
        self.groups
            .insert(id, Group::new(mls, truncate(name), self.me));
        Ok(id)
    }

    /// Owner/Admin: asks `peer` for a key package; the add completes when it
    /// arrives.
    pub fn invite(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<Output> {
        let g = self.group(group)?;
        if !self.can_commit(group)? {
            return Err(GroupError::NotOwner);
        }
        if self.members_of(g).contains(&peer) {
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

    /// Consents to an invitation: sends `peer` a fresh key package.
    pub fn accept_invite(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<Output> {
        let bundle = KeyPackage::builder()
            .build(
                CIPHERSUITE,
                &self.provider,
                &self.signer,
                credential_for(&self.me),
            )
            .map_err(mls)?;
        let key_package = MlsMessageOut::from(bundle.key_package().clone())
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

    /// Checks if the current user can commit (owner or admin).
    fn can_commit(&self, group: &GroupId) -> Result<bool> {
        let g = self.group(group)?;
        Ok(g.role(&self.me).can_commit())
    }

    /// Owner/Admin: removes `member` and distributes the commit.
    pub fn remove(&mut self, group: &GroupId, member: &PublicIdentity) -> Result<Output> {
        let me = self.me;
        if !self.can_commit(group)? {
            return Err(GroupError::NotOwner);
        }
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let index = g
            .mls
            .members()
            .find(|m| identity_of(&m.credential, &m.signature_key).ok().as_ref() == Some(member))
            .map(|m| m.index)
            .ok_or_else(|| GroupError::NotMember(member.fingerprint().to_string()))?;
        let recipients = self.members_of(self.group(group)?);
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let (commit, _, _) = g
            .mls
            .remove_members(&self.provider, &self.signer, &[index])
            .map_err(mls)?;
        g.mls.merge_pending_commit(&self.provider).map_err(mls)?;
        let message = commit.tls_serialize_detached().map_err(mls)?;
        let send = recipients
            .into_iter()
            .filter(|p| *p != me)
            .map(|to| Outgoing {
                to,
                wire: GroupWire::Message {
                    group: *group,
                    message: message.clone(),
                },
            })
            .collect();
        Ok(Output {
            send,
            events: vec![GroupEvent::MemberRemoved {
                group: *group,
                member: *member,
            }],
        })
    }

    /// Owner/Admin: promotes `member` to Admin.
    pub fn promote(&mut self, group: &GroupId, member: PublicIdentity) -> Result<Output> {
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        if g.owner != self.me {
            return Err(GroupError::NotOwner);
        }
        let role = g.role(&member);
        if role == MemberRole::Owner {
            return Err(GroupError::Unexpected("cannot promote owner"));
        }
        if role == MemberRole::Admin {
            return Err(GroupError::Unexpected("already an admin"));
        }
        g.set_role(member, MemberRole::Admin);
        Ok(Output {
            send: vec![],
            events: vec![GroupEvent::RoleChanged {
                group: *group,
                member,
                role: MemberRole::Admin,
            }],
        })
    }

    /// Owner: demotes `member` from Admin to Member.
    pub fn demote(&mut self, group: &GroupId, member: PublicIdentity) -> Result<Output> {
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        if g.owner != self.me {
            return Err(GroupError::NotOwner);
        }
        let role = g.role(&member);
        if role != MemberRole::Admin {
            return Err(GroupError::Unexpected("only admins can be demoted"));
        }
        g.set_role(member, MemberRole::Member);
        Ok(Output {
            send: vec![],
            events: vec![GroupEvent::RoleChanged {
                group: *group,
                member,
                role: MemberRole::Member,
            }],
        })
    }

    /// Leaves a group we don't own: asks the owner to remove us (it commits
    /// the removal for everyone) and forgets the group here at once.
    pub fn leave(&mut self, group: &GroupId) -> Result<Output> {
        let owner = self.group(group)?.owner;
        if owner == self.me {
            return Err(GroupError::Unexpected(
                "the owner deletes the group instead",
            ));
        }
        self.forget(group)?;
        Ok(Output {
            send: vec![Outgoing {
                to: owner,
                wire: GroupWire::Leave { group: *group },
            }],
            events: vec![GroupEvent::Left { group: *group }],
        })
    }

    /// Deletes a group we own: one commit removes every other member (each
    /// sees `Left`), then we forget it.
    pub fn disband(&mut self, group: &GroupId) -> Result<Output> {
        let me = self.me;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        if g.owner != me {
            return Err(GroupError::NotOwner);
        }
        let others: Vec<(LeafNodeIndex, PublicIdentity)> = g
            .mls
            .members()
            .filter_map(|m| {
                identity_of(&m.credential, &m.signature_key)
                    .ok()
                    .filter(|p| *p != me)
                    .map(|p| (m.index, p))
            })
            .collect();
        let mut send = Vec::new();
        if !others.is_empty() {
            let indices: Vec<LeafNodeIndex> = others.iter().map(|(i, _)| *i).collect();
            let (commit, _, _) = g
                .mls
                .remove_members(&self.provider, &self.signer, &indices)
                .map_err(mls)?;
            g.mls.merge_pending_commit(&self.provider).map_err(mls)?;
            let message = commit.tls_serialize_detached().map_err(mls)?;
            send = others
                .into_iter()
                .map(|(_, to)| Outgoing {
                    to,
                    wire: GroupWire::Message {
                        group: *group,
                        message: message.clone(),
                    },
                })
                .collect();
        }
        self.forget(group)?;
        Ok(Output {
            send,
            events: vec![GroupEvent::Left { group: *group }],
        })
    }

    /// Drops a group and its MLS secrets from our state.
    fn forget(&mut self, group: &GroupId) -> Result<()> {
        if let Some(mut g) = self.groups.remove(group) {
            g.mls.delete(self.provider.storage()).map_err(mls)?;
        }
        self.pending_adds.retain(|(g, _)| g != group);
        Ok(())
    }

    /// Encrypts `text` for the group and fans it out to every other member.
    pub fn send_text(&mut self, group: &GroupId, text: &str) -> Result<Output> {
        self.send(
            group,
            &Content::Text {
                text: text.to_owned(),
                id: 0,
            },
        )
    }

    /// Encrypts `content` for the group and fans it out to every other
    /// member.
    pub fn send(&mut self, group: &GroupId, content: &Content) -> Result<Output> {
        let recipients = self.members_of(self.group(group)?);
        let plain = content.encode()?;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let out = g
            .mls
            .create_message(&self.provider, &self.signer, &plain)
            .map_err(mls)?;
        let message = out.tls_serialize_detached().map_err(mls)?;
        let me = self.me;
        Ok(Output {
            send: recipients
                .into_iter()
                .filter(|p| *p != me)
                .map(|to| Outgoing {
                    to,
                    wire: GroupWire::Message {
                        group: *group,
                        message: message.clone(),
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
            GroupWire::Message { group, message } => self.on_message(group, &message),
            GroupWire::Forward {
                group, to, message, ..
            } => self.on_forward(from, group, to, message),
            // Delivery bookkeeping, not group state: see `node::GroupNode`.
            GroupWire::Receipt { .. } => Ok(Output::default()),
            GroupWire::Leave { group } => {
                // Only the owner commits; it removes a member who asks.
                if self.group(&group)?.owner != self.me {
                    return Err(GroupError::NotOwner);
                }
                if from == self.me {
                    return Err(GroupError::Unexpected("leave request from ourselves"));
                }
                // The leaver has already forgotten the group.
                let mut out = self.remove(&group, &from)?;
                out.send.retain(|o| o.to != from);
                Ok(out)
            }
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
    ) -> Result<Output> {
        let to = PublicIdentity::from_bytes(&to)?;
        let members = self.members_of(self.group(&group)?);
        if !members.contains(&from) {
            return Err(GroupError::NotMember(from.fingerprint().to_string()));
        }
        if to == self.me || to == from || !members.contains(&to) {
            return Err(GroupError::NotMember(to.fingerprint().to_string()));
        }
        Ok(Output {
            send: vec![Outgoing {
                to,
                wire: GroupWire::Message { group, message },
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
        let msg = MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?;
        let MlsMessageBodyIn::KeyPackage(kp_in) = msg.extract() else {
            return Err(GroupError::Unexpected("not a key package"));
        };
        let kp = kp_in
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(mls)?;
        let leaf = kp.leaf_node();
        if identity_of(leaf.credential(), leaf.signature_key().as_slice())? != from {
            return Err(GroupError::BadCredential);
        }
        if kp.ciphersuite() != CIPHERSUITE {
            return Err(GroupError::Unexpected("key package ciphersuite"));
        }
        self.pending_adds.remove(pos);

        let existing = self.members_of(self.group(&group)?);
        let me = self.me;
        let g = self
            .groups
            .get_mut(&group)
            .ok_or(GroupError::UnknownGroup)?;
        let (commit, welcome, _) = g
            .mls
            .add_members(&self.provider, &self.signer, &[kp])
            .map_err(mls)?;
        g.mls.merge_pending_commit(&self.provider).map_err(mls)?;
        let name = g.name.clone();
        let commit = commit.tls_serialize_detached().map_err(mls)?;
        let welcome = welcome.tls_serialize_detached().map_err(mls)?;
        let mut send: Vec<Outgoing> = existing
            .into_iter()
            .filter(|p| *p != me)
            .map(|to| Outgoing {
                to,
                wire: GroupWire::Message {
                    group,
                    message: commit.clone(),
                },
            })
            .collect();
        send.push(Outgoing {
            to: from,
            wire: GroupWire::Welcome {
                group,
                name,
                welcome,
            },
        });
        Ok(Output {
            send,
            events: vec![GroupEvent::MemberAdded {
                group,
                member: from,
            }],
        })
    }

    fn on_welcome(
        &mut self,
        from: PublicIdentity,
        group: GroupId,
        name: &str,
        bytes: &[u8],
    ) -> Result<Output> {
        if self.groups.contains_key(&group) {
            return Err(GroupError::Unexpected("already in group"));
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
        let mls = StagedWelcome::new_from_welcome(&self.provider, &config, welcome, None)
            .map_err(mls)?
            .into_group(&self.provider)
            .map_err(mls)?;
        if mls.group_id().as_slice() != group {
            return Err(GroupError::Unexpected("group id mismatch"));
        }
        if mls.ciphersuite() != CIPHERSUITE {
            return Err(GroupError::Unexpected("group ciphersuite"));
        }
        // Every member must be a Threnody identity, and the inviter (the
        // owner, our only committer) must be one of them.
        let members: Vec<PublicIdentity> = mls
            .members()
            .map(|m| identity_of(&m.credential, &m.signature_key))
            .collect::<Result<_>>()?;
        if !members.contains(&from) || !members.contains(&self.me) {
            return Err(GroupError::Unexpected("welcome from non-member"));
        }
        let name = truncate(name);
        let mut roles = HashMap::new();
        roles.insert(from, MemberRole::Owner);
        roles.insert(self.me, MemberRole::Member);
        self.groups.insert(
            group,
            Group {
                mls,
                name: name.clone(),
                owner: from,
                roles,
            },
        );
        Ok(Output {
            send: vec![],
            events: vec![GroupEvent::Joined {
                group,
                name,
                owner: from,
            }],
        })
    }

    fn on_message(&mut self, group: GroupId, bytes: &[u8]) -> Result<Output> {
        let me = self.me;
        let g = self
            .groups
            .get_mut(&group)
            .ok_or(GroupError::UnknownGroup)?;
        let msg = MlsMessageIn::tls_deserialize_exact(bytes).map_err(mls)?;
        let protocol = msg
            .try_into_protocol_message()
            .map_err(|_| GroupError::Unexpected("not a protocol message"))?;
        if protocol.group_id().as_slice() != group {
            return Err(GroupError::Unexpected("group id mismatch"));
        }
        let processed = g
            .mls
            .process_message(&self.provider, protocol)
            .map_err(mls)?;
        // The sender's credential must belong to a current member whose
        // signature key matches it (checked against the ratchet tree).
        let cred = processed.credential().clone();
        let sender = g
            .mls
            .members()
            .find(|m| m.credential == cred)
            .ok_or(GroupError::BadCredential)
            .and_then(|m| identity_of(&m.credential, &m.signature_key))?;
        let before: Vec<PublicIdentity> = g
            .mls
            .members()
            .filter_map(|m| identity_of(&m.credential, &m.signature_key).ok())
            .collect();
        let mut events = Vec::new();
        match processed.into_content() {
            ProcessedMessageContent::ApplicationMessage(app) => {
                events.push(match Content::decode(&app.into_bytes())? {
                    Content::Text { text, id } => GroupEvent::Text {
                        group,
                        from: sender,
                        text,
                        id,
                    },
                    Content::File {
                        name,
                        data,
                        sensitive,
                        caption,
                        album,
                        id,
                    } => GroupEvent::File {
                        group,
                        from: sender,
                        name: truncate(&name),
                        data,
                        sensitive,
                        caption,
                        album,
                        id,
                    },
                    Content::React { id, emoji, add } => GroupEvent::React {
                        group,
                        from: sender,
                        id,
                        emoji,
                        add,
                    },
                });
            }
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                if sender != g.owner {
                    return Err(GroupError::NotOwner);
                }
                g.mls
                    .merge_staged_commit(&self.provider, *staged)
                    .map_err(mls)?;
                if !g.mls.is_active() {
                    self.groups.remove(&group);
                    return Ok(Output {
                        send: vec![],
                        events: vec![GroupEvent::Left { group }],
                    });
                }
                let after: Vec<PublicIdentity> = g
                    .mls
                    .members()
                    .filter_map(|m| identity_of(&m.credential, &m.signature_key).ok())
                    .collect();
                events.extend(
                    after
                        .iter()
                        .filter(|p| !before.contains(p) && **p != me)
                        .map(|p| GroupEvent::MemberAdded { group, member: *p }),
                );
                events.extend(
                    before
                        .iter()
                        .filter(|p| !after.contains(p))
                        .map(|p| GroupEvent::MemberRemoved { group, member: *p }),
                );
            }
            ProcessedMessageContent::ProposalMessage(_)
            | ProcessedMessageContent::ExternalJoinProposalMessage(_) => {
                return Err(GroupError::Unexpected("proposals are not used"));
            }
            ProcessedMessageContent::OwnPendingCommit
            | ProcessedMessageContent::OwnPrivateMessage => {
                return Err(GroupError::Unexpected("own message echoed back"));
            }
        }
        Ok(Output {
            send: vec![],
            events,
        })
    }
}

fn truncate(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect()
}
