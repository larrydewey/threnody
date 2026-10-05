//! Anonymous identities and selective disclosure (spec §4.1, §4.3;
//! `docs/appendix-m-anonymity.md`).
//!
//! **Personas.** A persona is a separate identity, kept in its own
//! directory under the main home, with its own keys, contacts, history and
//! state. Nothing it sends is derived from the main identity, so the
//! people it talks to can't link it to the main identity or to other
//! personas. A persona can expire, and burning it deletes its directory:
//! everything in it was encrypted under the persona's own key, so this
//! leaves nothing readable. The list of personas is kept encrypted in the
//! main home.
//!
//! **Revealing.** A persona's user can later choose to show a peer who
//! they are: the main identity signs a statement naming the persona
//! ([`LinkProof`]), which the persona sends. The peer verifies it, and can
//! then add the main identity as a contact. Nothing links the two until
//! then.
//!
//! **Profiles.** An identity can describe itself with attributes (a name,
//! an email address, …) and choose, per contact, which of them that
//! contact sees. Nothing is shared until chosen.
//!
//! ```text
//! IdentityMsg = { 0: 1, 1: [* [key tstr, value tstr]] }            ; profile
//!             / { 0: 2, 2: identity bstr .size 32, 3: sig bstr .size 64,
//!                 ? 4: invite tstr }                               ; reveal
//! PersonaList = { 0: 1, 1: [* { 0: id tstr, 1: label tstr, 2: created uint,
//!                                ? 3: expires uint }] }
//! ```

use std::path::PathBuf;

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::store::Home;
use crate::{Error, Identity, PublicIdentity, Result};

/// Attributes one identity can describe itself with.
pub const MAX_ATTRIBUTES: usize = 16;
pub const MAX_KEY: usize = 32;
pub const MAX_VALUE: usize = 256;
/// The attribute apps show as the contact's name.
pub const NAME: &str = "name";

const LINK_DOMAIN: &[u8] = b"threnody persona link v1";
const LIST_STATE: &str = "personas";
/// Marks a home as a persona's, so the node there behaves as one.
const MARKER: &str = "persona";

/// Profile attributes: `(key, value)` pairs, keys unique.
pub type Profile = Vec<(String, String)>;

/// Checks a profile's size limits and that keys are unique.
pub fn check_profile(p: &Profile) -> Result<()> {
    if p.len() > MAX_ATTRIBUTES {
        return Err(Error::Malformed("too many profile attributes"));
    }
    for (i, (k, v)) in p.iter().enumerate() {
        if k.is_empty()
            || k.chars().count() > MAX_KEY
            || v.chars().count() > MAX_VALUE
            || k.chars().chain(v.chars()).any(char::is_control)
        {
            return Err(Error::Malformed("profile attribute"));
        }
        if p[..i].iter().any(|(o, _)| o == k) {
            return Err(Error::Malformed("duplicate profile attribute"));
        }
    }
    Ok(())
}

/// The subset of `profile` with keys in `shared`, in profile order.
pub fn disclose(profile: &Profile, shared: &[String]) -> Profile {
    profile
        .iter()
        .filter(|(k, _)| shared.contains(k))
        .cloned()
        .collect()
}

/// "The holder of `identity` is also `persona`", signed by `identity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkProof {
    pub identity: PublicIdentity,
    pub sig: [u8; 64],
}

impl LinkProof {
    fn message(persona: &PublicIdentity) -> Vec<u8> {
        [LINK_DOMAIN, persona.as_bytes()].concat()
    }

    /// Signs with the main identity that `persona` is the same person.
    pub fn sign(identity: &Identity, persona: &PublicIdentity) -> Self {
        Self {
            identity: identity.public(),
            sig: identity.sign(&Self::message(persona)),
        }
    }

    /// Checks the proof for `persona` (the peer that sent it).
    pub fn verify(&self, persona: &PublicIdentity) -> Result<()> {
        if self.identity == *persona {
            return Err(Error::Malformed("link proof names itself"));
        }
        self.identity.verify(&Self::message(persona), &self.sig)
    }
}

/// Identity messages, carried as `AppMessage::Identity`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityMsg {
    /// The attributes the sender shares with us now (replacing earlier ones).
    Profile(Profile),
    /// The sender reveals its main identity, with an invite to reach it.
    Reveal {
        proof: Box<LinkProof>,
        invite: Option<String>,
    },
}

impl IdentityMsg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Profile(p) => {
                let size = p.iter().map(|(k, v)| k.len() + v.len() + 8).sum::<usize>() + 16;
                cbor::to_vec(size, |e| {
                    e.map_len(2)?.u8(0)?.u8(1)?;
                    e.u8(1)?.array_len(p.len())?;
                    for (k, v) in p {
                        e.array_len(2)?.str(k)?.str(v)?;
                    }
                    Ok(())
                })
            }
            Self::Reveal { proof, invite } => {
                cbor::to_vec(128 + invite.as_ref().map_or(0, String::len), |e| {
                    e.map_len(3 + usize::from(invite.is_some()))?;
                    e.u8(0)?.u8(2)?;
                    e.u8(2)?.bytes(proof.identity.as_bytes())?;
                    e.u8(3)?.bytes(&proof.sig)?;
                    if let Some(i) = invite {
                        e.u8(4)?.str(i)?;
                    }
                    Ok(())
                })
            }
        }
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(b);
        let (mut kind, mut profile, mut identity, mut sig, mut invite) =
            (None, None, None, None, None);
        read_map(&mut d, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => {
                    let n = d.array_len()?;
                    if n > MAX_ATTRIBUTES {
                        return Err(Error::Malformed("too many profile attributes"));
                    }
                    let mut p = Vec::with_capacity(n);
                    for _ in 0..n {
                        if d.array_len()? != 2 {
                            return Err(Error::Malformed("profile attribute"));
                        }
                        p.push((d.str()?.to_owned(), d.str()?.to_owned()));
                    }
                    profile = Some(p);
                }
                2 => identity = Some(fixed_bytes::<32>(d)?),
                3 => sig = Some(fixed_bytes::<64>(d)?),
                4 => invite = Some(d.str()?.to_owned()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&d)?;
        match required(kind, "identity message kind")? {
            1 => {
                let p = profile.unwrap_or_default();
                check_profile(&p)?;
                Ok(Self::Profile(p))
            }
            2 => Ok(Self::Reveal {
                proof: Box::new(LinkProof {
                    identity: PublicIdentity::from_bytes(&required(identity, "identity")?)?,
                    sig: required(sig, "link signature")?,
                }),
                invite: invite.filter(|i| i.len() <= 512),
            }),
            other => Err(Error::UnexpectedType(u64::from(other))),
        }
    }
}

/// A persona, as listed in the main home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersonaInfo {
    /// Random; names the persona's directory.
    pub id: String,
    /// The user's own label for it ("market seller"); never sent.
    pub label: String,
    pub created_ms: u64,
    /// When it burns itself, if ever.
    pub expires_ms: Option<u64>,
}

/// The personas kept under a main home.
pub struct Personas<'a> {
    main: &'a Home,
    identity: &'a Identity,
}

impl<'a> Personas<'a> {
    /// `identity` is the main identity, whose key protects the list.
    pub fn new(main: &'a Home, identity: &'a Identity) -> Self {
        Self { main, identity }
    }

    fn root(&self) -> PathBuf {
        self.main.dir().join("personas")
    }

    pub fn list(&self) -> Result<Vec<PersonaInfo>> {
        match self.main.load_state(self.identity, LIST_STATE)? {
            Some(b) => decode_list(&b),
            None => Ok(Vec::new()),
        }
    }

    fn save(&self, list: &[PersonaInfo]) -> Result<()> {
        self.main
            .save_state(self.identity, LIST_STATE, &encode_list(list)?)
    }

    /// Where persona `id` lives (it has its own identity there).
    pub fn home(&self, id: &str) -> Result<Home> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Malformed("persona id"));
        }
        Ok(Home::new(self.root().join(id)))
    }

    /// Makes a new persona with a fresh identity, sealed with `passphrase`
    /// if given. Returns it and its home.
    pub fn create(
        &self,
        label: &str,
        expires_ms: Option<u64>,
        passphrase: Option<&[u8]>,
        now_ms: u64,
    ) -> Result<(PersonaInfo, Home, Identity)> {
        let id: String = crate::crypto::random_bytes::<8>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let home = self.home(&id)?;
        let identity = home.create_identity(passphrase)?;
        mark(&home)?;
        let info = PersonaInfo {
            id,
            label: label.chars().take(64).collect(),
            created_ms: now_ms,
            expires_ms,
        };
        let mut list = self.list()?;
        list.push(info.clone());
        self.save(&list)?;
        Ok((info, home, identity))
    }

    pub fn rename(&self, id: &str, label: &str) -> Result<()> {
        let mut list = self.list()?;
        let p = list
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or(Error::Malformed("no such persona"))?;
        p.label = label.chars().take(64).collect();
        self.save(&list)
    }

    /// Deletes persona `id` and everything it kept.
    pub fn burn(&self, id: &str) -> Result<()> {
        let home = self.home(id)?;
        if home.dir().exists() {
            // The identity first: without it the rest is unreadable even
            // if removing the directory is interrupted.
            let _ = std::fs::remove_file(home.dir().join("identity.cbor"));
            std::fs::remove_dir_all(home.dir())?;
        }
        let mut list = self.list()?;
        list.retain(|p| p.id != id);
        self.save(&list)
    }

    /// Burns personas whose time is up; returns their ids.
    pub fn burn_expired(&self, now_ms: u64) -> Result<Vec<String>> {
        let due: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|p| p.expires_ms.is_some_and(|t| t <= now_ms))
            .map(|p| p.id)
            .collect();
        for id in &due {
            self.burn(id)?;
        }
        Ok(due)
    }
}

/// Marks `home` as a persona's.
fn mark(home: &Home) -> Result<()> {
    std::fs::write(home.dir().join(MARKER), b"")?;
    Ok(())
}

/// Whether `home` belongs to a persona.
pub fn is_persona(home: &Home) -> bool {
    home.dir().join(MARKER).exists()
}

fn encode_list(list: &[PersonaInfo]) -> Result<Vec<u8>> {
    cbor::to_vec(64 + list.len() * 96, |e| {
        e.map_len(2)?.u8(0)?.u8(1)?;
        e.u8(1)?.array_len(list.len())?;
        for p in list {
            e.map_len(3 + usize::from(p.expires_ms.is_some()))?;
            e.u8(0)?.str(&p.id)?;
            e.u8(1)?.str(&p.label)?;
            e.u8(2)?.u64(p.created_ms)?;
            if let Some(t) = p.expires_ms {
                e.u8(3)?.u64(t)?;
            }
        }
        Ok(())
    })
}

fn decode_list(b: &[u8]) -> Result<Vec<PersonaInfo>> {
    let mut d = Decoder::new(b);
    let mut list = Vec::new();
    read_map(&mut d, |k, d| {
        match k {
            0 => {
                d.u64()?;
            }
            1 => {
                for _ in 0..d.array_len()? {
                    let (mut id, mut label, mut created, mut expires) = (None, None, 0, None);
                    read_map(d, |k, d| {
                        match k {
                            0 => id = Some(d.str()?.to_owned()),
                            1 => label = Some(d.str()?.to_owned()),
                            2 => created = d.u64()?,
                            3 => expires = Some(d.u64()?),
                            _ => return Ok(false),
                        }
                        Ok(true)
                    })?;
                    list.push(PersonaInfo {
                        id: required(id, "persona id")?,
                        label: label.unwrap_or_default(),
                        created_ms: created,
                        expires_ms: expires,
                    });
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&d)?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_messages_round_trip_and_check_limits() {
        let me = Identity::generate();
        let persona = Identity::generate().public();
        for m in [
            IdentityMsg::Profile(vec![]),
            IdentityMsg::Profile(vec![
                ("name".into(), "Larry".into()),
                ("email".into(), "l@x".into()),
            ]),
            IdentityMsg::Reveal {
                proof: Box::new(LinkProof::sign(&me, &persona)),
                invite: Some("threnody://X@1.2.3.4:7450".into()),
            },
            IdentityMsg::Reveal {
                proof: Box::new(LinkProof::sign(&me, &persona)),
                invite: None,
            },
        ] {
            assert_eq!(IdentityMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        let dup = IdentityMsg::Profile(vec![("a".into(), "1".into()), ("a".into(), "2".into())]);
        assert!(IdentityMsg::decode(&dup.encode().unwrap()).is_err());
        let long = IdentityMsg::Profile(vec![("a".into(), "x".repeat(MAX_VALUE + 1))]);
        assert!(IdentityMsg::decode(&long.encode().unwrap()).is_err());
        let ctl = IdentityMsg::Profile(vec![("name".into(), "a\nb".into())]);
        assert!(IdentityMsg::decode(&ctl.encode().unwrap()).is_err());
        assert!(IdentityMsg::decode(&[0xa1, 0, 9]).is_err());
    }

    #[test]
    fn link_proofs_bind_both_identities() {
        let me = Identity::generate();
        let persona = Identity::generate().public();
        let other = Identity::generate().public();
        let proof = LinkProof::sign(&me, &persona);
        proof.verify(&persona).unwrap();
        // Not transferable to another persona, and can't be forged.
        assert!(proof.verify(&other).is_err());
        let mut forged = proof.clone();
        forged.identity = other;
        assert!(forged.verify(&persona).is_err());
        // A persona can't "reveal" itself.
        let own = Identity::generate();
        assert!(
            LinkProof::sign(&own, &own.public())
                .verify(&own.public())
                .is_err()
        );
    }

    #[test]
    fn disclosure_picks_only_shared_attributes() {
        let p: Profile = vec![
            ("name".into(), "Larry".into()),
            ("email".into(), "l@x".into()),
            ("phone".into(), "555".into()),
        ];
        assert_eq!(disclose(&p, &[]), vec![]);
        assert_eq!(
            disclose(&p, &["phone".into(), "name".into()]),
            vec![
                ("name".into(), "Larry".into()),
                ("phone".into(), "555".into())
            ]
        );
    }

    #[test]
    fn personas_are_created_listed_and_burned() {
        let dir = tempfile::tempdir().unwrap();
        let main = Home::new(dir.path().join("main"));
        let me = main.create_identity(None).unwrap();
        let personas = Personas::new(&main, &me);
        assert!(personas.list().unwrap().is_empty());
        let (a, home_a, id_a) = personas.create("market", Some(1_000), None, 10).unwrap();
        let (b, home_b, _) = personas.create("forum", None, Some(b"pw"), 20).unwrap();
        assert_ne!(id_a.public(), me.public());
        assert!(is_persona(&home_a) && is_persona(&home_b) && !is_persona(&main));
        assert_eq!(home_a.load_identity(None).unwrap().public(), id_a.public());
        assert!(home_b.identity_is_sealed().unwrap());
        assert_eq!(personas.list().unwrap(), vec![a.clone(), b.clone()]);
        personas.rename(&b.id, "forum alias").unwrap();
        assert_eq!(personas.list().unwrap()[1].label, "forum alias");

        // The list is unreadable without the main identity.
        let stranger = Identity::generate();
        assert!(Personas::new(&main, &stranger).list().is_err());

        assert_eq!(personas.burn_expired(999).unwrap(), Vec::<String>::new());
        assert_eq!(personas.burn_expired(1_000).unwrap(), vec![a.id.clone()]);
        assert!(!home_a.dir().exists());
        assert_eq!(personas.list().unwrap().len(), 1);
        personas.burn(&b.id).unwrap();
        assert!(!home_b.dir().exists() && personas.list().unwrap().is_empty());
        assert!(personas.home("../main").is_err());
    }
}
