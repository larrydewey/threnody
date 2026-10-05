//! Profiles shared per contact, and revealing a persona's main identity
//! (`threnody_core::persona`, `docs/appendix-m-anonymity.md`).
//!
//! A contact sees only the profile attributes we share with it, sent when
//! a session starts and whenever the profile or the choice changes. A
//! changed choice replaces what the contact had, so unsharing an
//! attribute removes it from their view (though they may have kept it).
//! Identity messages only go to peers whose `Hello` has
//! `FEATURE_IDENTITY`.

use threnody_core::message::FEATURE_IDENTITY;
use threnody_core::persona::{self, IdentityMsg, LinkProof, PersonaInfo, Personas, Profile};
use threnody_core::{AppMessage, PublicIdentity};

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

const STATE: &str = "profile";

pub(crate) fn decode_profile(b: &[u8]) -> Profile {
    match IdentityMsg::decode(b) {
        Ok(IdentityMsg::Profile(p)) => p,
        _ => Vec::new(),
    }
}

impl Node {
    /// Our profile attributes (what we could share).
    pub fn profile(&self) -> Profile {
        lock(&self.shared.profile).clone()
    }

    /// Replaces our profile; contacts see the change to what they're shared.
    pub fn set_profile(&self, profile: Profile) -> Result<()> {
        persona::check_profile(&profile)?;
        let bytes = IdentityMsg::Profile(profile.clone()).encode()?;
        self.shared.save_state(STATE, Ok(bytes));
        *lock(&self.shared.profile) = profile;
        for s in self.sessions() {
            self.send_profile(&s.peer);
        }
        Ok(())
    }

    /// The attribute keys we share with `peer`.
    pub fn shared_with(&self, peer: &PublicIdentity) -> Vec<String> {
        lock(&self.shared.contacts)
            .get(peer)
            .map(|c| c.shares.clone())
            .unwrap_or_default()
    }

    /// Chooses which attributes `peer` sees: every device of its account.
    /// Shares only keys we have; the rest are ignored.
    pub fn set_shared_with(&self, peer: &PublicIdentity, keys: &[String]) -> Result<()> {
        let devices = self.devices_of(peer);
        {
            let mut contacts = lock(&self.shared.contacts);
            if contacts.get(peer).is_none() {
                return Err(NetError::NotAllowed("not a contact".into()));
            }
            for d in &devices {
                if let Some(c) = contacts.get_mut(d) {
                    c.shares = keys.iter().take(persona::MAX_ATTRIBUTES).cloned().collect();
                }
            }
            self.shared.save_contacts(&contacts);
        }
        for d in &devices {
            self.send_profile(d);
        }
        Ok(())
    }

    /// `peer` and the other devices of its account that we know.
    fn devices_of(&self, peer: &PublicIdentity) -> Vec<PublicIdentity> {
        let account = lock(&self.shared.contacts)
            .get(peer)
            .and_then(|c| c.account);
        let mut out = vec![*peer];
        if let Some(a) = account {
            out.extend(
                lock(&self.shared.contacts)
                    .iter()
                    .filter(|c| c.account == Some(a) && c.key != *peer)
                    .map(|c| c.key),
            );
        }
        out
    }

    /// Sends `peer` what we share with it now, if it understands profiles.
    pub(crate) fn send_profile(&self, peer: &PublicIdentity) {
        if !self.supports(peer, FEATURE_IDENTITY) || self.is_own_device(peer) {
            return;
        }
        let shown = persona::disclose(&lock(&self.shared.profile), &self.shared_with(peer));
        if let Ok(b) = IdentityMsg::Profile(shown).encode() {
            let _ = self.send(peer, AppMessage::Identity(b));
        }
    }

    /// Shows `peer` (whom a persona talks to) our main identity: `proof`
    /// is signed by it ([`LinkProof::sign`]), `invite` reaches it.
    pub fn reveal(
        &self,
        peer: &PublicIdentity,
        proof: LinkProof,
        invite: Option<String>,
    ) -> Result<()> {
        proof
            .verify(&self.identity())
            .map_err(|_| NetError::NotAllowed("the proof doesn't name this identity".into()))?;
        if !self.supports(peer, FEATURE_IDENTITY) {
            return Err(NetError::NotAllowed("%s".into()));
        }
        let b = IdentityMsg::Reveal {
            proof: Box::new(proof),
            invite,
        }
        .encode()?;
        self.send_tracked(peer, AppMessage::Identity(b))
    }

    pub(crate) fn on_identity(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = IdentityMsg::decode(payload) else {
            return;
        };
        match msg {
            IdentityMsg::Profile(p) => {
                let changed = {
                    let mut contacts = lock(&self.shared.contacts);
                    let changed = contacts
                        .get_mut(&from)
                        .filter(|c| c.profile != p)
                        .map(|c| c.profile = p)
                        .is_some();
                    if changed {
                        self.shared.save_contacts(&contacts);
                    }
                    changed
                };
                if changed {
                    self.emit(Event::ProfileChanged { peer: from });
                }
            }
            IdentityMsg::Reveal { proof, invite } => {
                if proof.verify(&from).is_ok() {
                    {
                        let mut contacts = lock(&self.shared.contacts);
                        if let Some(c) = contacts.get_mut(&from) {
                            c.revealed = Some((proof.identity, invite.clone()));
                            self.shared.save_contacts(&contacts);
                        }
                    }
                    self.emit(Event::IdentityRevealed {
                        peer: from,
                        identity: proof.identity,
                        invite,
                    });
                }
            }
        }
    }

    /// Whether this node is an anonymous identity (a persona).
    pub fn is_persona(&self) -> bool {
        self.shared.persona
    }

    fn main_only(&self) -> Result<()> {
        if self.shared.persona {
            return Err(NetError::NotAllowed("%s".into()));
        }
        Ok(())
    }

    /// Our anonymous identities (from the main identity's node).
    pub fn personas(&self) -> Result<Vec<PersonaInfo>> {
        self.main_only()?;
        Ok(Personas::new(&self.shared.home, self.identity_ref()).list()?)
    }

    /// Makes an anonymous identity, its key sealed with `passphrase` if
    /// given. Returns it, its home directory and its public key.
    pub fn create_persona(
        &self,
        label: &str,
        expires_ms: Option<u64>,
        passphrase: Option<&[u8]>,
    ) -> Result<(PersonaInfo, std::path::PathBuf, PublicIdentity)> {
        self.main_only()?;
        let (info, home, id) = Personas::new(&self.shared.home, self.identity_ref()).create(
            label,
            expires_ms,
            passphrase,
            threnody_core::now_ms(),
        )?;
        Ok((info, home.dir().to_path_buf(), id.public()))
    }

    /// Where anonymous identity `id` keeps its data.
    pub fn persona_home(&self, id: &str) -> Result<std::path::PathBuf> {
        self.main_only()?;
        Ok(Personas::new(&self.shared.home, self.identity_ref())
            .home(id)?
            .dir()
            .to_path_buf())
    }

    pub fn rename_persona(&self, id: &str, label: &str) -> Result<()> {
        self.main_only()?;
        Ok(Personas::new(&self.shared.home, self.identity_ref()).rename(id, label)?)
    }

    /// Deletes anonymous identity `id` and everything it kept. Stop its
    /// node first.
    pub fn burn_persona(&self, id: &str) -> Result<()> {
        self.main_only()?;
        Ok(Personas::new(&self.shared.home, self.identity_ref()).burn(id)?)
    }

    /// Burns anonymous identities whose time is up; returns their ids.
    pub fn burn_expired_personas(&self) -> Result<Vec<String>> {
        self.main_only()?;
        Ok(Personas::new(&self.shared.home, self.identity_ref())
            .burn_expired(threnody_core::now_ms())?)
    }

    /// Our signed statement that anonymous identity `persona` is us, for
    /// it to [`Node::reveal`].
    pub fn link_proof(&self, persona: &PublicIdentity) -> Result<LinkProof> {
        self.main_only()?;
        Ok(LinkProof::sign(self.identity_ref(), persona))
    }
}
