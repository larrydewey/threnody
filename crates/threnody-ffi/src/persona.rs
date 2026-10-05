//! Anonymous identities and profiles for apps (spec §4.1, §4.3;
//! `docs/appendix-m-anonymity.md`).
//!
//! The main identity's node manages personas: it creates them, lists them
//! and burns them, and signs the proof a persona sends to reveal who it
//! is. Each persona runs as a node of its own, opened with
//! [`ThrenodyNode::open`] on its home directory.

use std::sync::Arc;

use threnody_core::persona::PersonaInfo;

use crate::{Result, ThrenodyNode, fail};

/// One profile attribute.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ProfileAttr {
    pub key: String,
    pub value: String,
}

/// An anonymous identity, as the main identity lists it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PersonaRecord {
    pub id: String,
    /// The user's own label; never sent.
    pub label: String,
    pub created_ms: u64,
    /// When it burns itself, if ever.
    pub expires_ms: Option<u64>,
    /// Its data directory: open its node with `ThrenodyNode::open`.
    pub home: String,
}

pub(crate) fn attrs(p: &[(String, String)]) -> Vec<ProfileAttr> {
    p.iter()
        .map(|(k, v)| ProfileAttr {
            key: k.clone(),
            value: v.clone(),
        })
        .collect()
}

impl ThrenodyNode {
    fn record(&self, p: PersonaInfo) -> Result<PersonaRecord> {
        let home = self.node.persona_home(&p.id).map_err(fail)?;
        Ok(PersonaRecord {
            id: p.id,
            label: p.label,
            created_ms: p.created_ms,
            expires_ms: p.expires_ms,
            home: home.display().to_string(),
        })
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Whether this node is an anonymous identity.
    pub fn is_persona(&self) -> bool {
        self.node.is_persona()
    }

    /// Our anonymous identities (call on the main identity's node).
    pub fn personas(&self) -> Result<Vec<PersonaRecord>> {
        self.node
            .personas()
            .map_err(fail)?
            .into_iter()
            .map(|p| self.record(p))
            .collect()
    }

    /// Makes an anonymous identity, sealed with `passphrase`; `expires_ms`
    /// is when it burns itself. Open it with `ThrenodyNode::open(home, …)`.
    pub fn create_persona(
        &self,
        label: String,
        expires_ms: Option<u64>,
        passphrase: Option<String>,
    ) -> Result<PersonaRecord> {
        let (info, _, _) = self
            .node
            .create_persona(&label, expires_ms, passphrase.as_deref().map(str::as_bytes))
            .map_err(fail)?;
        self.record(info)
    }

    pub fn rename_persona(&self, id: String, label: String) -> Result<()> {
        self.node.rename_persona(&id, &label).map_err(fail)
    }

    /// Deletes an anonymous identity and everything it kept. Shut its node
    /// down first.
    pub fn burn_persona(&self, id: String) -> Result<()> {
        self.node.burn_persona(&id).map_err(fail)
    }

    /// Burns the anonymous identities whose time is up (shut their nodes
    /// down first); returns their ids.
    pub fn burn_expired_personas(&self) -> Result<Vec<String>> {
        self.node.burn_expired_personas().map_err(fail)
    }

    /// Shows `peer`, who knows us only as `persona`, who we are: we sign
    /// the proof, the persona sends it with `invite` (our invite link).
    /// This can't be taken back.
    pub fn reveal_through(
        &self,
        persona: Arc<ThrenodyNode>,
        peer: String,
        invite: Option<String>,
    ) -> Result<()> {
        let proof = self
            .node
            .link_proof(&persona.node.identity())
            .map_err(fail)?;
        let p = persona.resolve(&peer)?;
        let _guard = persona.rt.enter();
        persona.node.reveal(&p, proof, invite).map_err(fail)
    }

    /// Our profile: what contacts may be shown, each only what we share.
    pub fn profile(&self) -> Vec<ProfileAttr> {
        attrs(&self.node.profile())
    }

    pub fn set_profile(&self, attributes: Vec<ProfileAttr>) -> Result<()> {
        let _guard = self.rt.enter();
        self.node
            .set_profile(attributes.into_iter().map(|a| (a.key, a.value)).collect())
            .map_err(fail)
    }

    /// The profile keys `peer` sees.
    pub fn shared_with(&self, peer: String) -> Result<Vec<String>> {
        Ok(self.node.shared_with(&self.resolve(&peer)?))
    }

    /// Chooses which profile keys `peer` (all its account's devices) sees.
    pub fn set_shared_with(&self, peer: String, keys: Vec<String>) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.set_shared_with(&p, &keys).map_err(fail)
    }
}
