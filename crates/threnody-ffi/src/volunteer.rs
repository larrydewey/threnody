//! Relay directories, volunteering (Appendix P) and credentials
//! (Appendix O) for apps.

use threnody_net::cred::{CredentialAsk, CredentialOffer};

use crate::persona::attrs;
use crate::{ProfileAttr, Result, ThrenodyNode, fail, fp};

/// A subscribed relay directory.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DirectoryRecord {
    pub link: String,
    /// Short id for display; `id_hex` addresses it in calls.
    pub id: String,
    pub id_hex: String,
    pub relays: u32,
    pub valid_until_ms: Option<u64>,
    /// Relay-token credentials held from it for today and later.
    pub tokens: u32,
}

/// A credential we hold.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CredentialRecord {
    pub id: u64,
    pub issuer: String,
    pub schema: String,
    pub attributes: Vec<ProfileAttr>,
    pub expires_day: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CredentialOfferRecord {
    pub id: u64,
    pub peer: String,
    pub issuer: String,
    pub schema: String,
    pub attributes: Vec<ProfileAttr>,
    pub expires_day: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CredentialAskRecord {
    pub id: u64,
    pub peer: String,
    pub schema: String,
    pub keys: Vec<String>,
}

pub(crate) fn offer_record(o: CredentialOffer) -> CredentialOfferRecord {
    CredentialOfferRecord {
        id: o.id,
        peer: fp(&o.peer),
        issuer: o.issuer.to_string(),
        schema: o.schema,
        attributes: attrs(&o.attributes),
        expires_day: o.expires_day,
    }
}

pub(crate) fn ask_record(a: CredentialAsk) -> CredentialAskRecord {
    CredentialAskRecord {
        id: a.id,
        peer: fp(&a.peer),
        schema: a.schema,
        keys: a.keys,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[uniffi::export]
impl ThrenodyNode {
    /// Subscribes to a relay directory (`threnody-dir://…`), over an
    /// anonymous link; gets its relays and relay tokens.
    pub fn subscribe_directory(&self, link: String) -> Result<DirectoryRecord> {
        let _guard = self.rt.enter();
        let d = self
            .rt
            .block_on(self.node.subscribe_directory(&link))
            .map_err(fail)?;
        Ok(record(d))
    }

    pub fn unsubscribe_directory(&self, id_hex: String) -> bool {
        self.node
            .directories()
            .into_iter()
            .find(|d| hex(&d.issuer) == id_hex)
            .is_some_and(|d| self.node.unsubscribe_directory(&d.issuer))
    }

    pub fn directories(&self) -> Vec<DirectoryRecord> {
        self.node.directories().into_iter().map(record).collect()
    }

    /// Fetches new documents and tokens from every subscription now.
    pub fn refresh_directories(&self) {
        self.rt.block_on(self.node.refresh_directories());
    }

    /// Volunteer relays usable now.
    pub fn volunteer_relay_count(&self) -> u32 {
        u32::try_from(self.node.volunteer_relays().len()).unwrap_or(u32::MAX)
    }

    /// Route through volunteer relays when contacts can't (on by default).
    pub fn set_use_volunteers(&self, on: bool) {
        self.node.set_use_volunteers(on);
    }

    pub fn use_volunteers(&self) -> bool {
        self.node.use_volunteers()
    }

    /// Volunteers as a relay at public `addrs`, or stops (empty).
    pub fn set_volunteer(&self, addrs: Vec<String>) -> Result<()> {
        let _guard = self.rt.enter();
        self.node
            .set_volunteer((!addrs.is_empty()).then_some(addrs))
            .map_err(fail)
    }

    pub fn volunteering(&self) -> bool {
        self.node.volunteering()
    }

    // ----- Credentials -----

    pub fn credentials(&self) -> Vec<CredentialRecord> {
        self.node
            .credentials()
            .into_iter()
            .map(|c| CredentialRecord {
                id: c.id,
                issuer: c.issuer.to_string(),
                schema: c.schema,
                attributes: attrs(&c.attributes),
                expires_day: c.expires_day,
            })
            .collect()
    }

    /// Offers `peer` a credential on `attributes`, valid for `days`.
    pub fn offer_credential(
        &self,
        peer: String,
        schema: String,
        attributes: Vec<ProfileAttr>,
        days: u32,
    ) -> Result<u64> {
        let p = self.resolve(&peer)?;
        let expires = threnody_core::credential::day(threnody_core::now_ms()) + days;
        let _guard = self.rt.enter();
        self.node
            .offer_credential(
                &p,
                &schema,
                attributes.into_iter().map(|a| (a.key, a.value)).collect(),
                expires,
            )
            .map_err(fail)
    }

    pub fn credential_offers(&self) -> Vec<CredentialOfferRecord> {
        self.node
            .credential_offers()
            .into_iter()
            .map(offer_record)
            .collect()
    }

    pub fn accept_credential_offer(&self, id: u64) -> Result<()> {
        let _guard = self.rt.enter();
        self.node.accept_credential_offer(id).map_err(fail)
    }

    pub fn decline_credential(&self, id: u64) -> Result<()> {
        let _guard = self.rt.enter();
        self.node.decline_credential(id).map_err(fail)
    }

    /// Asks `peer` to prove `keys` of a `schema` credential.
    pub fn ask_credential(&self, peer: String, schema: String, keys: Vec<String>) -> Result<u64> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.ask_credential(&p, &schema, keys).map_err(fail)
    }

    pub fn credential_asks(&self) -> Vec<CredentialAskRecord> {
        self.node
            .credential_asks()
            .into_iter()
            .map(ask_record)
            .collect()
    }

    /// Answers request `id` with credential `credential`, showing `keys`
    /// (only those asked for are shown).
    pub fn present_credential(&self, id: u64, credential: u64, keys: Vec<String>) -> Result<()> {
        let _guard = self.rt.enter();
        self.node
            .present_credential(id, credential, &keys)
            .map_err(fail)
    }

    pub fn delete_credential(&self, id: u64) -> bool {
        self.node.delete_credential(id)
    }
}

fn record(d: threnody_net::DirectoryInfo) -> DirectoryRecord {
    DirectoryRecord {
        id: threnody_core::directory::short_id(&d.issuer),
        id_hex: hex(&d.issuer),
        link: d.link,
        relays: u32::try_from(d.relays).unwrap_or(u32::MAX),
        valid_until_ms: d.valid_until_ms,
        tokens: u32::try_from(d.tokens).unwrap_or(u32::MAX),
    }
}
