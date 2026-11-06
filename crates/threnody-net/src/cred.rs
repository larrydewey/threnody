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
/// Of those waiting for the user, from one peer.
const MAX_PENDING_PER_PEER: usize = 8;

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
    /// Offers we accepted: id -> (the offer, our secrets, its key).
    requested: HashMap<u64, (CredentialOffer, Request, IssuerKey)>,
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
            .insert(id, (offer, req, key));
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
                let taken = self.may_wait(&peer) && {
                    let mut st = lock(&self.shared.creds);
                    let theirs = st.offers.values().filter(|(o, _)| o.peer == peer).count();
                    let room = st.offers.len() < MAX_PENDING && theirs < MAX_PENDING_PER_PEER;
                    if room {
                        st.offers.insert(id, (offer.clone(), key));
                    }
                    room
                };
                if !taken {
                    let _ = self.cred_send(&peer, &CredMsg::Decline { id });
                    return;
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
                        Some((o, ..)) if o.peer == peer => st.requested.remove(&id),
                        _ => None,
                    }
                };
                let Some((offer, req, key)) = pending else {
                    return;
                };
                let cred = Issued::decode(&issued).and_then(|i| req.finish(&key, &i));
                // Exactly what the user accepted: another expiry, schema or
                // a hidden extra attribute shows in every presentation, so
                // the issuer could tell this holder's apart.
                let cred = cred.and_then(|c| {
                    let same = c.header.schema == offer.schema
                        && c.header.expires_day == offer.expires_day
                        && c.attributes == offer.attributes;
                    same.then_some(c).ok_or(threnody_core::Error::Malformed(
                        "the issuer signed something other than its offer",
                    ))
                });
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
                let taken = self.may_wait(&peer) && {
                    let mut st = lock(&self.shared.creds);
                    let theirs = st.asks.values().filter(|(a, _)| a.peer == peer).count();
                    let room = st.asks.len() < MAX_PENDING && theirs < MAX_PENDING_PER_PEER;
                    if room {
                        st.asks.insert(id, (ask.clone(), nonce));
                    }
                    room
                };
                if !taken {
                    let _ = self.cred_send(&peer, &CredMsg::Decline { id });
                    return;
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

    /// Whether an offer or ask from `peer` may wait for the user: only
    /// from someone whose messages are accepted. A stranger's would show
    /// past the message-request gate, one notification each. Anything
    /// turned away is declined, so the peer hears of it.
    fn may_wait(&self, peer: &PublicIdentity) -> bool {
        self.is_accepted(peer)
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

    fn test_node(dir: &tempfile::TempDir) -> Node {
        let home = threnody_core::store::Home::new(dir.path().join("n"));
        let identity = home.create_identity(None).unwrap();
        Node::new(crate::NodeConfig {
            home,
            identity,
            policy: crate::AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap()
        .0
    }

    fn offer(from: &threnody_core::Identity, id: u64) -> Vec<u8> {
        CredMsg::Offer {
            id,
            key: Issuer::new(from, 0).unwrap().key().encode().unwrap(),
            schema: "example/member/1".into(),
            attributes: vec![("member".into(), "yes".into())],
            expires_day: 99_999,
        }
        .encode()
        .unwrap()
    }

    fn ask(id: u64) -> Vec<u8> {
        CredMsg::Ask {
            id,
            schema: "example/member/1".into(),
            keys: vec!["member".into()],
            nonce: [0; 32],
        }
        .encode()
        .unwrap()
    }

    async fn next(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
        pred: impl Fn(&Event) -> bool,
    ) -> Event {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let e = rx.recv().await.expect("event stream open");
                if pred(&e) {
                    return e;
                }
            }
        })
        .await
        .expect("expected event did not arrive")
    }

    #[tokio::test]
    async fn holders_take_only_what_was_offered() {
        let dir = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            let home = threnody_core::store::Home::new(dir.path().join(name));
            let identity = home.create_identity(None).unwrap();
            Node::new(crate::NodeConfig {
                home,
                identity,
                policy: crate::AcceptPolicy::Anyone,
                constant_rate: None,
                tunnel_port: None,
            })
            .unwrap()
        };
        let (issuer, mut irx) = open("issuer");
        let (holder, mut hrx) = open("holder");
        let addr = holder.listen("127.0.0.1:0").await.unwrap().to_string();
        let hid = issuer.connect(&addr, None).await.unwrap();
        holder.accept_contact(&issuer.identity());
        // Features are exchanged in the first messages: wait for them (a
        // fixed pause is too short on a busy machine).
        let iid = issuer.identity();
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !(issuer.supports(&hid, FEATURE_CREDENTIALS)
                && holder.supports(&iid, FEATURE_CREDENTIALS))
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("features exchanged");
        let attrs = vec![("member".to_owned(), "yes".to_owned())];
        let today = credential::day(now_ms());
        for tamper in [
            |o: &mut (PublicIdentity, String, Profile, u32)| o.3 += 1,
            |o: &mut (PublicIdentity, String, Profile, u32)| o.1.push('2'),
            |o: &mut (PublicIdentity, String, Profile, u32)| o.2.push(("x".into(), "1".into())),
        ] {
            let id = issuer
                .offer_credential(&hid, "example/member/1", attrs.clone(), today + 30)
                .unwrap();
            // The issuer signs something other than what it offered: a
            // different expiry or a hidden attribute could mark this
            // holder's presentations.
            tamper(lock(&issuer.shared.creds).offered.get_mut(&id).unwrap());
            next(&mut hrx, |e| matches!(e, Event::CredentialOffered { .. })).await;
            holder.accept_credential_offer(id).unwrap();
            next(&mut hrx, |e| {
                matches!(
                    e,
                    Event::CredentialFailed { .. } | Event::CredentialReceived { .. }
                )
            })
            .await;
            assert!(holder.credentials().is_empty());
        }
        let id = issuer
            .offer_credential(&hid, "example/member/1", attrs.clone(), today + 30)
            .unwrap();
        next(&mut hrx, |e| matches!(e, Event::CredentialOffered { .. })).await;
        holder.accept_credential_offer(id).unwrap();
        next(&mut hrx, |e| matches!(e, Event::CredentialReceived { .. })).await;
        assert_eq!(holder.credentials().len(), 1);
        let _ = &mut irx;
    }

    #[tokio::test]
    async fn offers_and_asks_wait_only_from_accepted_peers_and_only_so_many() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(&dir);
        // A stranger's offers and asks are message requests' business.
        let stranger = threnody_core::Identity::generate();
        node.on_credential(stranger.public(), &offer(&stranger, 1));
        node.on_credential(stranger.public(), &ask(2));
        assert!(node.credential_offers().is_empty());
        assert!(node.credential_asks().is_empty());
        // An accepted peer can't take every slot either.
        let friend = threnody_core::Identity::generate();
        node.accept_contact(&friend.public());
        for id in 0..100 {
            node.on_credential(friend.public(), &offer(&friend, 10 + id));
            node.on_credential(friend.public(), &ask(1000 + id));
        }
        assert_eq!(node.credential_offers().len(), MAX_PENDING_PER_PEER);
        assert_eq!(node.credential_asks().len(), MAX_PENDING_PER_PEER);
        let other = threnody_core::Identity::generate();
        node.accept_contact(&other.public());
        node.on_credential(other.public(), &offer(&other, 5000));
        node.on_credential(other.public(), &ask(5001));
        assert_eq!(node.credential_offers().len(), MAX_PENDING_PER_PEER + 1);
        assert_eq!(node.credential_asks().len(), MAX_PENDING_PER_PEER + 1);
    }

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
