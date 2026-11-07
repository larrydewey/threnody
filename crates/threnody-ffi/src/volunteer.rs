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

/// Credential offers and asks from one contact, waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PendingCredentials {
    pub offers: Vec<CredentialOfferRecord>,
    pub asks: Vec<CredentialAskRecord>,
}

/// A relay registered with the directory this node runs.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DirectoryRelayRecord {
    pub fingerprint: String,
    pub addrs: Vec<String>,
    /// Whether clients see it (it may wait for review).
    pub listed: bool,
    pub expires_ms: u64,
}

/// Parses relay addresses the user typed: `host:port`s separated by
/// commas or spaces. An IPv6 host needs brackets (`[::1]:7450`). The list
/// must also pass the descriptor rule (1 to 8, each at most 64 bytes).
#[uniffi::export]
pub fn relay_addrs(text: String) -> Result<Vec<String>> {
    let addrs: Vec<String> = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
        .collect();
    for a in &addrs {
        let ok = a.rsplit_once(':').is_some_and(|(host, port)| {
            let bracketed = host.len() > 2 && host.starts_with('[') && host.ends_with(']');
            !host.is_empty()
                && (bracketed || !host.contains([':', '[', ']']))
                && port.parse::<u16>().is_ok_and(|p| p > 0)
        });
        if !ok {
            return Err(fail(format!("{a:?} isn't host:port")));
        }
    }
    threnody_core::directory::check_relay_addrs(&addrs)
        .map_err(|_| fail("give 1 to 8 addresses, each at most 64 characters"))?;
    Ok(addrs)
}

pub(crate) fn hex(b: &[u8]) -> String {
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

    /// Runs a relay directory on this identity, reachable at `addr`
    /// (`host:port`); returns the `threnody-dir://` link to share. With
    /// `review`, registered relays wait for `set_relay_listed`. Not for an
    /// anonymous identity.
    pub fn serve_directory(&self, addr: String, review: bool) -> Result<String> {
        threnody_core::directory::check_relay_addrs(std::slice::from_ref(&addr)).map_err(fail)?;
        let _guard = self.rt.enter();
        self.node.serve_directory(&addr, review).map_err(fail)
    }

    /// Stops the directory and deletes its state; returns whether one ran.
    pub fn stop_serving_directory(&self) -> bool {
        self.node.stop_serving_directory()
    }

    pub fn serving_directory(&self) -> bool {
        self.node.serving_directory()
    }

    /// Relays registered with our directory.
    pub fn directory_relays(&self) -> Vec<DirectoryRelayRecord> {
        let mut out: Vec<DirectoryRelayRecord> = self
            .node
            .directory_relays()
            .into_iter()
            .map(|r| DirectoryRelayRecord {
                fingerprint: fp(&r.identity),
                addrs: r.addrs,
                listed: r.listed,
                expires_ms: r.expires_ms,
            })
            .collect();
        out.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        out
    }

    /// Lists (or unlists) a registered relay; `Ok(false)` if there is no
    /// such relay.
    pub fn set_relay_listed(&self, relay: String, listed: bool) -> Result<bool> {
        let f: threnody_core::Fingerprint = relay.parse().map_err(fail)?;
        let _guard = self.rt.enter();
        Ok(self.node.set_relay_listed(&f, listed))
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

    /// The offers and asks from `peer`'s account (any of its devices)
    /// still waiting for an answer, in a stable order (by id).
    pub fn pending_credentials(&self, peer: String) -> Result<PendingCredentials> {
        let devices = self.account_devices(&self.resolve(&peer)?);
        let mut offers: Vec<CredentialOfferRecord> = self
            .node
            .credential_offers()
            .into_iter()
            .filter(|o| devices.contains(&o.peer))
            .map(offer_record)
            .collect();
        offers.sort_by_key(|o| o.id);
        let mut asks: Vec<CredentialAskRecord> = self
            .node
            .credential_asks()
            .into_iter()
            .filter(|a| devices.contains(&a.peer))
            .map(ask_record)
            .collect();
        asks.sort_by_key(|a| a.id);
        Ok(PendingCredentials { offers, asks })
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
