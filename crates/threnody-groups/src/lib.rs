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

pub mod wire;

use std::collections::HashMap;

use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::{Signer, SignerError};
use threnody_core::crypto::random_bytes;
use threnody_core::{Identity, PublicIdentity};

pub use wire::{GroupId, GroupWire};

pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519;
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
    /// We were removed, or the group was otherwise closed for us.
    Left { group: GroupId },
    Text {
        group: GroupId,
        from: PublicIdentity,
        text: String,
    },
}

/// Results of handling one input.
#[derive(Debug, Default)]
pub struct Output {
    pub send: Vec<Outgoing>,
    pub events: Vec<GroupEvent>,
}

struct Group {
    mls: MlsGroup,
    name: String,
    owner: PublicIdentity,
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

    /// `(id, name, owner, members)` for every group we are in.
    pub fn list(&self) -> Vec<(GroupId, String, PublicIdentity, Vec<PublicIdentity>)> {
        self.groups
            .iter()
            .map(|(id, g)| (*id, g.name.clone(), g.owner, self.members_of(g)))
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
        self.groups.insert(
            id,
            Group {
                mls,
                name: truncate(name),
                owner: self.me,
            },
        );
        Ok(id)
    }

    /// Owner: asks `peer` for a key package; the add completes when it
    /// arrives.
    pub fn invite(&mut self, group: &GroupId, peer: PublicIdentity) -> Result<Output> {
        let g = self.group(group)?;
        if g.owner != self.me {
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

    /// Owner: removes `member` and distributes the commit.
    pub fn remove(&mut self, group: &GroupId, member: &PublicIdentity) -> Result<Output> {
        let me = self.me;
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        if g.owner != me {
            return Err(GroupError::NotOwner);
        }
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

    /// Encrypts `text` for the group and fans it out to every other member.
    pub fn send_text(&mut self, group: &GroupId, text: &str) -> Result<Output> {
        let recipients = self.members_of(self.group(group)?);
        let g = self.groups.get_mut(group).ok_or(GroupError::UnknownGroup)?;
        let out = g
            .mls
            .create_message(&self.provider, &self.signer, text.as_bytes())
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
        }
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
        self.groups.insert(
            group,
            Group {
                mls,
                name: name.clone(),
                owner: from,
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
                let text = String::from_utf8_lossy(&app.into_bytes()).into_owned();
                events.push(GroupEvent::Text {
                    group,
                    from: sender,
                    text,
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
