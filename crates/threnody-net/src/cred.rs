//! Credentials between peers (Appendix O): issuing them, asking for them,
//! and presenting them.
//!
//! Any identity can issue credentials on attributes it vouches for. The
//! holder keeps them and later proves chosen attributes to someone else,
//! who learns those attributes, the issuer and a pseudonym that is stable
//! for that verifier only. Proofs are bound to the session they travel in,
//! so a verifier can't show them to a third party as fresh.
//!
//! Every step that gives something away waits for the user: accepting an
//! offer, and presenting.

use std::collections::HashMap;

use threnody_core::credential::{
    self, CredMsg, Credential, Issued, Issuer, IssuerKey, Presentation, Request, Verified,
};
use threnody_core::crypto::random_bytes;
use threnody_core::message::FEATURE_CREDENTIALS;
use threnody_core::persona::{Profile, check_profile};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

const STATE: &str = "credentials";
/// Exchanges waiting for the user or the peer, per kind.
const MAX_PENDING: usize = 64;

/// A held credential, as apps show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialInfo {
    pub id: u64,
    pub issuer: Fingerprint,
    pub schema: String,
    pub attributes: Profile,
    pub expires_day: u32,
}

/// A request from a peer for a presentation, waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialAsk {
    pub id: u64,
    pub peer: PublicIdentity,
    pub schema: String,
    pub keys: Vec<String>,
}

/// An offer from a peer, waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialOffer {
    pub id: u64,
    pub peer: PublicIdentity,
    pub issuer: Fingerprint,
    pub schema: String,
    pub attributes: Profile,
    pub expires_day: u32,
}

#[derive(Default)]
pub(crate) struct CredState {
    held: Vec<Credential>,
    /// Offers we made: id -> (peer, schema, attributes, expiry).
    offered: HashMap<u64, (PublicIdentity, String, Profile, u32)>,
    /// Offers made to us, waiting for the user.
    offers: HashMap<u64, (CredentialOffer, IssuerKey)>,
    /// Offers we accepted: id -> (issuer peer, our secrets, its key).
    requested: HashMap<u64, (PublicIdentity, Request, IssuerKey)>,
    /// Presentations we asked for: id -> (peer, schema, keys, nonce).
    asked: HashMap<u64, (PublicIdentity, String, Vec<String>, [u8; 32])>,
    /// Requests for our presentations, waiting for the user.
    asks: HashMap<u64, (CredentialAsk, [u8; 32])>,
}

fn encode_held(held: &[Credential]) -> threnody_core::Result<Vec<u8>> {
    let raw = held
        .iter()
        .map(Credential::encode)
        .collect::<threnody_core::Result<Vec<_>>>()?;
    threnody_core::cbor::to_vec(64 + raw.iter().map(|r| r.len() + 8).sum::<usize>(), |e| {
        e.array_len(raw.len())?;
        for r in &raw {
            e.bytes(r)?;
        }
        Ok(())
    })
}

fn decode_held(b: &[u8]) -> threnody_core::Result<Vec<Credential>> {
    let mut d = const_cbor::Decoder::new(b);
    let n = d.array_len()?;
    let mut out = Vec::new();
    for _ in 0..n {
        if let Ok(c) = Credential::decode(d.bytes()?) {
            out.push(c);
        }
    }
    threnody_core::cbor::finish(&d)?;
    Ok(out)
}

fn new_id() -> u64 {
    u64::from_le_bytes(random_bytes())
}

impl Node {
    pub(crate) fn load_credentials(&self) {
        if let Ok(Some(b)) = self.shared.home.load_state(self.identity_ref(), STATE) {
            lock(&self.shared.creds).held = decode_held(&b).unwrap_or_default();
        }
    }

    fn save_credentials(&self) {
        let bytes = encode_held(&lock(&self.shared.creds).held);
        self.shared.save_state(STATE, bytes);
    }

    fn cred_send(&self, peer: &PublicIdentity, m: &CredMsg) -> Result<()> {
        if !self.supports(peer, FEATURE_CREDENTIALS) {
            return Err(NetError::NotAllowed(
                "their app doesn't support credentials yet".into(),
            ));
        }
        self.send(peer, AppMessage::Credential(m.encode()?))
    }

    /// Our issuer key (for this identity).
    pub fn issuer_key(&self) -> Result<IssuerKey> {
        Ok(Issuer::new(self.identity_ref(), 0)?.key().clone())
    }

    /// Offers `peer` a credential on `attributes`, issued by us, valid
    /// through `expires_day` (days since 1970). Returns the exchange id.
    pub fn offer_credential(
        &self,
        peer: &PublicIdentity,
        schema: &str,
        attributes: Profile,
        expires_day: u32,
    ) -> Result<u64> {
        check_profile(&attributes)?;
        let id = new_id();
        let key = self.issuer_key()?.encode()?;
        self.cred_send(
            peer,
            &CredMsg::Offer {
                id,
                key,
                schema: schema.to_owned(),
                attributes: attributes.clone(),
                expires_day,
            },
        )?;
        let mut st = lock(&self.shared.creds);
        if st.offered.len() >= MAX_PENDING {
            let oldest = st.offered.keys().next().copied();
            if let Some(k) = oldest {
                st.offered.remove(&k);
            }
        }
        st.offered
            .insert(id, (*peer, schema.to_owned(), attributes, expires_day));
        Ok(id)
    }

    /// Offers waiting for the user.
    pub fn credential_offers(&self) -> Vec<CredentialOffer> {
        lock(&self.shared.creds)
            .offers
            .values()
            .map(|(o, _)| o.clone())
            .collect()
    }

    /// Accepts an offer: asks the issuer to sign, with our secrets hidden.
    pub fn accept_credential_offer(&self, id: u64) -> Result<()> {
        let (offer, key) = lock(&self.shared.creds)
            .offers
            .remove(&id)
            .ok_or_else(|| NetError::NotAllowed("no such offer".into()))?;
        let req = Request::new()?;
        self.cred_send(
            &offer.peer,
            &CredMsg::Request {
                id,
                commitment: req.commitment.clone(),
            },
        )?;
        lock(&self.shared.creds)
            .requested
            .insert(id, (offer.peer, req, key));
        Ok(())
    }

    /// Declines an offer or a request for a presentation.
    pub fn decline_credential(&self, id: u64) -> Result<()> {
        let peer = {
            let mut st = lock(&self.shared.creds);
            st.offers
                .remove(&id)
                .map(|(o, _)| o.peer)
                .or_else(|| st.asks.remove(&id).map(|(a, _)| a.peer))
        };
        match peer {
            Some(p) => self.cred_send(&p, &CredMsg::Decline { id }),
            None => Err(NetError::NotAllowed("nothing to decline".into())),
        }
    }

    /// The credentials we hold.
    pub fn credentials(&self) -> Vec<CredentialInfo> {
        lock(&self.shared.creds)
            .held
            .iter()
            .map(|c| CredentialInfo {
                id: c.id(),
                issuer: c.key.identity.fingerprint(),
                schema: c.header.schema.clone(),
                attributes: c.attributes.clone(),
                expires_day: c.header.expires_day,
            })
            .collect()
    }

    pub fn delete_credential(&self, id: u64) -> bool {
        let removed = {
            let mut st = lock(&self.shared.creds);
            let before = st.held.len();
            st.held.retain(|c| c.id() != id);
            st.held.len() != before
        };
        if removed {
            self.save_credentials();
        }
        removed
    }

    /// Asks `peer` to prove the attributes `keys` of a `schema` credential.
    pub fn ask_credential(
        &self,
        peer: &PublicIdentity,
        schema: &str,
        keys: Vec<String>,
    ) -> Result<u64> {
        let id = new_id();
        let nonce: [u8; 32] = random_bytes();
        self.cred_send(
            peer,
            &CredMsg::Ask {
                id,
                schema: schema.to_owned(),
                keys: keys.clone(),
                nonce,
            },
        )?;
        lock(&self.shared.creds)
            .asked
            .insert(id, (*peer, schema.to_owned(), keys, nonce));
        Ok(id)
    }

    /// Requests for our presentations waiting for the user.
    pub fn credential_asks(&self) -> Vec<CredentialAsk> {
        lock(&self.shared.creds)
            .asks
            .values()
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// Answers request `id` with a proof of `disclose` from credential
    /// `credential` (by its id). Only keys the peer asked for are shown.
    pub fn present_credential(&self, id: u64, credential: u64, disclose: &[String]) -> Result<()> {
        let (ask, nonce) = lock(&self.shared.creds)
            .asks
            .remove(&id)
            .ok_or_else(|| NetError::NotAllowed("no such request".into()))?;
        let session = self
            .sessions()
            .into_iter()
            .find(|s| s.peer == ask.peer)
            .ok_or(NetError::Closed)?
            .session_id;
        let (key, presentation) = {
            let st = lock(&self.shared.creds);
            let cred = st
                .held
                .iter()
                .find(|c| c.id() == credential)
                .ok_or_else(|| NetError::NotAllowed("no such credential".into()))?;
            if cred.header.schema != ask.schema {
                return Err(NetError::NotAllowed(
                    "that credential has another schema".into(),
                ));
            }
            let shown: Vec<&str> = disclose
                .iter()
                .filter(|k| ask.keys.contains(k))
                .map(String::as_str)
                .collect();
            let p = cred.present(
                &shown,
                &credential::peer_context(&ask.peer),
                &credential::presentation_binding(&session, &nonce),
            )?;
            (cred.key.encode()?, p.encode()?)
        };
        self.cred_send(
            &ask.peer,
            &CredMsg::Proof {
                id,
                key,
                presentation,
            },
        )
    }

    /// A credential message from `peer` (a session).
    pub(crate) fn on_credential(&self, peer: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = CredMsg::decode(payload) else {
            return;
        };
        match msg {
            CredMsg::Offer {
                id,
                key,
                schema,
                attributes,
                expires_day,
            } => {
                // Only from the peer itself: an offer names the issuer.
                let Ok(key) = IssuerKey::decode(&key) else {
                    return;
                };
                if key.identity != peer {
                    return;
                }
                let offer = CredentialOffer {
                    id,
                    peer,
                    issuer: key.identity.fingerprint(),
                    schema,
                    attributes,
                    expires_day,
                };
                {
                    let mut st = lock(&self.shared.creds);
                    if st.offers.len() >= MAX_PENDING {
                        return;
                    }
                    st.offers.insert(id, (offer.clone(), key));
                }
                self.emit(Event::CredentialOffered { offer });
            }
            CredMsg::Request { id, commitment } => {
                let offer = {
                    let mut st = lock(&self.shared.creds);
                    match st.offered.get(&id) {
                        Some((p, ..)) if *p == peer => st.offered.remove(&id),
                        _ => None,
                    }
                };
                let Some((_, schema, attributes, expires)) = offer else {
                    return;
                };
                let issued = Issuer::new(self.identity_ref(), 0)
                    .and_then(|i| i.issue(&commitment, &schema, &attributes, expires))
                    .and_then(|i| i.encode());
                if let Ok(issued) = issued {
                    let _ = self.cred_send(&peer, &CredMsg::Issued { id, issued });
                }
            }
            CredMsg::Issued { id, issued } => {
                let pending = {
                    let mut st = lock(&self.shared.creds);
                    match st.requested.get(&id) {
                        Some((p, ..)) if *p == peer => st.requested.remove(&id),
                        _ => None,
                    }
                };
                let Some((_, req, key)) = pending else { return };
                let cred = Issued::decode(&issued).and_then(|i| req.finish(&key, &i));
                match cred {
                    Ok(cred) => {
                        let schema = cred.header.schema.clone();
                        lock(&self.shared.creds).held.push(cred);
                        self.save_credentials();
                        self.emit(Event::CredentialReceived { peer, schema });
                    }
                    Err(e) => self.emit(Event::CredentialFailed {
                        peer,
                        id,
                        reason: e.to_string(),
                    }),
                }
            }
            CredMsg::Decline { id } => {
                let mine = {
                    let mut st = lock(&self.shared.creds);
                    let a = st.offered.remove(&id).is_some();
                    let b = st.asked.remove(&id).is_some();
                    a || b
                };
                if mine {
                    self.emit(Event::CredentialFailed {
                        peer,
                        id,
                        reason: "declined".into(),
                    });
                }
            }
            CredMsg::Ask {
                id,
                schema,
                keys,
                nonce,
            } => {
                let ask = CredentialAsk {
                    id,
                    peer,
                    schema,
                    keys,
                };
                {
                    let mut st = lock(&self.shared.creds);
                    if st.asks.len() >= MAX_PENDING {
                        return;
                    }
                    st.asks.insert(id, (ask.clone(), nonce));
                }
                self.emit(Event::CredentialAsked { ask });
            }
            CredMsg::Proof {
                id,
                key,
                presentation,
            } => {
                let asked = {
                    let mut st = lock(&self.shared.creds);
                    match st.asked.get(&id) {
                        Some((p, ..)) if *p == peer => st.asked.remove(&id),
                        _ => None,
                    }
                };
                let Some((_, schema, keys, nonce)) = asked else {
                    return;
                };
                let result = self.check_proof(&peer, &key, &presentation, &schema, &keys, &nonce);
                match result {
                    Ok(verified) => self.emit(Event::CredentialPresented { peer, id, verified }),
                    Err(reason) => self.emit(Event::CredentialFailed { peer, id, reason }),
                }
            }
        }
    }

    fn check_proof(
        &self,
        peer: &PublicIdentity,
        key: &[u8],
        presentation: &[u8],
        schema: &str,
        keys: &[String],
        nonce: &[u8; 32],
    ) -> std::result::Result<Verified, String> {
        let key = IssuerKey::decode(key).map_err(|e| e.to_string())?;
        let p = Presentation::decode(presentation).map_err(|e| e.to_string())?;
        let session = self
            .sessions()
            .into_iter()
            .find(|s| s.peer == *peer)
            .ok_or("the session ended")?
            .session_id;
        let v = p
            .verify(
                &key,
                &credential::peer_context(&self.identity()),
                &credential::presentation_binding(&session, nonce),
                now_ms(),
            )
            .map_err(|e| format!("invalid proof: {e}"))?;
        if v.schema != schema {
            return Err("a credential of another schema".into());
        }
        if v.attributes.iter().any(|(k, _)| !keys.contains(k)) {
            return Err("showed attributes nobody asked for".into());
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_credentials_round_trip() {
        let issuer = Issuer::new(&threnody_core::Identity::generate(), 0).unwrap();
        let req = Request::new().unwrap();
        let attrs = vec![("member".to_owned(), "yes".to_owned())];
        let iss = issuer.issue(&req.commitment, "s", &attrs, 99_999).unwrap();
        let cred = req.finish(issuer.key(), &iss).unwrap();
        let id = cred.id();
        let back = decode_held(&encode_held(&[cred]).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id(), id);
        assert_eq!(back[0].attributes, attrs);
    }
}
