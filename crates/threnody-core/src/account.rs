//! Accounts: hash-linked, signed device lists (`docs/appendix-j-devices.md`,
//! proved in `proofs/devices.spthy`).

use core::fmt;
use core::str::FromStr;

use const_cbor::{Decoder, Encoder};

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::kdf::{self, label};
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::{Fingerprint, Identity, PublicIdentity};

/// Device names are display labels; keep them short.
pub const MAX_DEVICE_NAME: usize = 64;
/// Upper bound on chain length accepted from the network.
pub const MAX_LINKS: usize = 1024;
const LINK_VERSION: u64 = 1;

/// Stable account identifier: a KDF of the genesis link body.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub [u8; 32]);

impl AccountId {
    /// Same presentation as device fingerprints, different derivation.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint(kdf::derive(label::ACCOUNT_FINGERPRINT, &[&self.0]))
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccountId({})", self.fingerprint())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Add {
        device: PublicIdentity,
        name: String,
    },
    Remove {
        device: PublicIdentity,
    },
    /// A new display name for a current device; membership is unchanged.
    Rename {
        device: PublicIdentity,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub seq: u64,
    pub prev: [u8; 32],
    pub threshold: u8,
    pub action: Action,
    pub sigs: Vec<(PublicIdentity, [u8; 64])>,
}

impl Link {
    /// The signed part of the link (everything but the signatures).
    pub fn body(&self) -> Result<Vec<u8>> {
        cbor::to_vec(256, |e| {
            e.map_len(5)?;
            e.u8(0)?.u64(LINK_VERSION)?;
            e.u8(1)?.u64(self.seq)?;
            e.u8(2)?.bytes(&self.prev)?;
            e.u8(3)?.u8(self.threshold)?;
            e.u8(4)?;
            encode_action(e, &self.action)
        })
    }

    fn hash(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&self.body()?).as_bytes())
    }

    fn sig_input(&self) -> Result<Vec<u8>> {
        let body = self.body()?;
        let mut v = Vec::with_capacity(body.len() + 64);
        for p in [label::SIG_ACCOUNT_LINK.as_bytes(), &body[..]] {
            v.extend_from_slice(&(p.len() as u64).to_le_bytes());
            v.extend_from_slice(p);
        }
        Ok(v)
    }

    /// Adds `identity`'s signature (replacing any earlier one by it).
    pub fn sign(&mut self, identity: &Identity) -> Result<()> {
        let sig = identity.sign(&self.sig_input()?);
        let me = identity.public();
        self.sigs.retain(|(d, _)| *d != me);
        self.sigs.push((me, sig));
        Ok(())
    }

    /// Encodes body and signatures (for proposals sent between devices).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let body = self.body()?;
        cbor::to_vec(body.len() + self.sigs.len() * 110 + 16, |e| {
            e.map_len(2)?;
            e.u8(0)?.bytes(&body)?;
            e.u8(1)?.array_len(self.sigs.len())?;
            for (d, s) in &self.sigs {
                e.array_len(2)?.bytes(d.as_bytes())?.bytes(s)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let link = decode_link(&mut dec)?;
        finish(&dec)?;
        Ok(link)
    }

    /// Devices whose signatures on this link verify.
    fn valid_signers(&self) -> Result<Vec<PublicIdentity>> {
        let input = self.sig_input()?;
        let mut out: Vec<PublicIdentity> = Vec::new();
        for (d, s) in &self.sigs {
            if !out.contains(d) && d.verify(&input, s).is_ok() {
                out.push(*d);
            }
        }
        Ok(out)
    }
}

fn encode_action(e: &mut Encoder<'_>, a: &Action) -> core::result::Result<(), const_cbor::Error> {
    match a {
        Action::Add { device, name } => {
            e.map_len(3)?.u8(0)?.u8(1)?;
            e.u8(1)?.bytes(device.as_bytes())?;
            e.u8(2)?.str(name)?;
        }
        Action::Remove { device } => {
            e.map_len(2)?.u8(0)?.u8(2)?;
            e.u8(1)?.bytes(device.as_bytes())?;
        }
        Action::Rename { device, name } => {
            e.map_len(3)?.u8(0)?.u8(3)?;
            e.u8(1)?.bytes(device.as_bytes())?;
            e.u8(2)?.str(name)?;
        }
    }
    Ok(())
}

fn decode_action(d: &mut Decoder<'_>) -> Result<Action> {
    let (mut op, mut device, mut name) = (None, None, None);
    read_map(d, |k, d| {
        match k {
            0 => op = Some(d.u8()?),
            1 => device = Some(fixed_bytes::<32>(d)?),
            2 => name = Some(d.str()?.to_owned()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let device = PublicIdentity::from_bytes(&required(device, "device key")?)?;
    let checked = |name: Option<String>| -> Result<String> {
        let name = required(name, "device name")?;
        if name.chars().count() > MAX_DEVICE_NAME || name.chars().any(char::is_control) {
            return Err(Error::Malformed("device name"));
        }
        Ok(name)
    };
    Ok(match required(op, "account action")? {
        1 => Action::Add {
            device,
            name: checked(name)?,
        },
        2 => Action::Remove { device },
        3 => Action::Rename {
            device,
            name: checked(name)?,
        },
        other => return Err(Error::UnexpectedType(u64::from(other))),
    })
}

/// The result of replaying a chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    pub id: AccountId,
    /// Current devices with their names, in order of addition.
    pub devices: Vec<(PublicIdentity, String)>,
    pub removed: Vec<PublicIdentity>,
    pub threshold: u8,
    pub seq: u64,
    pub head: [u8; 32],
}

impl AccountState {
    pub fn has(&self, d: &PublicIdentity) -> bool {
        self.devices.iter().any(|(x, _)| x == d)
    }

    pub fn name_of(&self, d: &PublicIdentity) -> Option<&str> {
        self.devices
            .iter()
            .find(|(x, _)| x == d)
            .map(|(_, n)| n.as_str())
    }
}

/// A verified account chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountChain {
    links: Vec<Link>,
    state: AccountState,
}

impl AccountChain {
    /// A new single-device account.
    pub fn genesis(identity: &Identity, name: &str) -> Result<Self> {
        let mut link = Link {
            seq: 0,
            prev: [0; 32],
            threshold: 1,
            action: Action::Add {
                device: identity.public(),
                name: clean_name(name),
            },
            sigs: vec![],
        };
        link.sign(identity)?;
        Self::from_links(vec![link])
    }

    /// Replays and verifies `links` (Appendix J, "Validity").
    pub fn from_links(links: Vec<Link>) -> Result<Self> {
        if links.is_empty() || links.len() > MAX_LINKS {
            return Err(Error::Malformed("account chain length"));
        }
        let g = &links[0];
        let Action::Add { device, name } = &g.action else {
            return Err(Error::Malformed("genesis must add a device"));
        };
        if g.seq != 0 || g.prev != [0; 32] || g.threshold == 0 {
            return Err(Error::Malformed("genesis link"));
        }
        if !g.valid_signers()?.contains(device) {
            return Err(Error::BadSignature);
        }
        let mut st = AccountState {
            id: AccountId(kdf::derive(label::ACCOUNT_ID, &[&g.body()?])),
            devices: vec![(*device, name.clone())],
            removed: vec![],
            threshold: g.threshold,
            seq: 0,
            head: g.hash()?,
        };
        for link in &links[1..] {
            apply(&mut st, link)?;
        }
        Ok(Self { links, state: st })
    }

    pub fn state(&self) -> &AccountState {
        &self.state
    }

    pub fn id(&self) -> AccountId {
        self.state.id
    }

    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// An unsigned next link carrying `action` (threshold unchanged).
    pub fn propose(&self, action: Action) -> Link {
        Link {
            seq: self.state.seq + 1,
            prev: self.state.head,
            threshold: self.state.threshold,
            action,
            sigs: vec![],
        }
    }

    /// Verifies `link` against the current state and appends it.
    pub fn append(&mut self, link: Link) -> Result<()> {
        let mut st = self.state.clone();
        apply(&mut st, &link)?;
        self.links.push(link);
        self.state = st;
        Ok(())
    }

    /// True when `self` is `older` followed by zero or more links.
    pub fn extends(&self, older: &AccountChain) -> bool {
        self.links.len() >= older.links.len() && self.links[..older.links.len()] == older.links[..]
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let parts: Vec<Vec<u8>> = self.links.iter().map(Link::encode).collect::<Result<_>>()?;
        cbor::to_vec(parts.iter().map(Vec::len).sum::<usize>() + 32, |e| {
            e.array_len(parts.len())?;
            for p in &parts {
                e.bytes(p)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let n = dec.array_len()?;
        if n == 0 || n > MAX_LINKS {
            return Err(Error::Malformed("account chain length"));
        }
        let mut links = Vec::with_capacity(n);
        for _ in 0..n {
            links.push(Link::decode(dec.bytes()?)?);
        }
        finish(&dec)?;
        Self::from_links(links)
    }
}

fn decode_link(dec: &mut Decoder<'_>) -> Result<Link> {
    let (mut body, mut sigs) = (None, Vec::new());
    read_map(dec, |k, d| {
        match k {
            0 => body = Some(d.bytes()?.to_vec()),
            1 => {
                let m = d.array_len()?;
                if m > 32 {
                    return Err(Error::Malformed("too many link signatures"));
                }
                for _ in 0..m {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("link signature"));
                    }
                    let dev = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?)?;
                    sigs.push((dev, fixed_bytes::<64>(d)?));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let body = required(body, "link body")?;
    let mut link = decode_body(&body)?;
    // The body must re-encode identically, so its hash is canonical.
    if link.body()? != body {
        return Err(Error::Malformed("non-canonical link body"));
    }
    link.sigs = sigs;
    Ok(link)
}

fn decode_body(b: &[u8]) -> Result<Link> {
    let mut dec = Decoder::new(b);
    let (mut ver, mut seq, mut prev, mut threshold, mut action) = (None, None, None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => ver = Some(d.u64()?),
            1 => seq = Some(d.u64()?),
            2 => prev = Some(fixed_bytes::<32>(d)?),
            3 => threshold = Some(d.u8()?),
            4 => action = Some(decode_action(d)?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    if ver != Some(LINK_VERSION) {
        return Err(Error::Malformed("account link version"));
    }
    Ok(Link {
        seq: required(seq, "seq")?,
        prev: required(prev, "prev")?,
        threshold: required(threshold, "threshold")?,
        action: required(action, "action")?,
        sigs: vec![],
    })
}

/// Applies one non-genesis link to `st` (Appendix J rules 2–6).
fn apply(st: &mut AccountState, link: &Link) -> Result<()> {
    if link.seq != st.seq + 1 || link.prev != st.head {
        return Err(Error::Malformed("account link out of order"));
    }
    if link.threshold == 0 {
        return Err(Error::Malformed("account threshold"));
    }
    let signers = link.valid_signers()?;
    let members = signers.iter().filter(|d| st.has(d)).count();
    if members < usize::from(st.threshold) {
        return Err(Error::BadSignature);
    }
    match &link.action {
        Action::Add { device, name } => {
            if st.has(device) || st.removed.contains(device) {
                return Err(Error::Malformed(
                    "device already in, or removed from, account",
                ));
            }
            if !signers.contains(device) {
                return Err(Error::BadSignature);
            }
            st.devices.push((*device, name.clone()));
        }
        Action::Remove { device } => {
            if !st.has(device) {
                return Err(Error::Malformed("removing a device not in the account"));
            }
            if st.devices.len() == 1 {
                return Err(Error::Malformed("account would have no devices"));
            }
            st.devices.retain(|(d, _)| d != device);
            st.removed.push(*device);
        }
        Action::Rename { device, name } => {
            if clean_name(name) != *name {
                return Err(Error::Malformed("device name"));
            }
            let Some((_, n)) = st.devices.iter_mut().find(|(d, _)| d == device) else {
                return Err(Error::Malformed("renaming a device not in the account"));
            };
            n.clone_from(name);
        }
    }
    st.threshold = link.threshold;
    st.seq = link.seq;
    st.head = link.hash()?;
    Ok(())
}

/// A device name as the chain stores it: no control characters, at most
/// [`MAX_DEVICE_NAME`] characters.
pub fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME)
        .collect()
}

// ----------------------------------------------------------- account book

/// What changed when a peer presented its chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// First time we see this account.
    New,
    Unchanged,
    /// The chain grew: devices added and removed since the stored version.
    Updated {
        added: Vec<PublicIdentity>,
        removed: Vec<PublicIdentity>,
    },
}

/// Peer accounts we know, pinned at first sight (persist with
/// `Home::save_state`).
#[derive(Clone, Debug, Default)]
pub struct AccountBook {
    chains: std::collections::HashMap<AccountId, AccountChain>,
}

impl AccountBook {
    /// Accepts `chain` as presented by `device` in an authenticated
    /// session. Rejects forks and chains that don't contain `device`.
    pub fn observe(&mut self, chain: AccountChain, device: &PublicIdentity) -> Result<Observed> {
        if !chain.state().has(device) {
            return Err(Error::Malformed("presenting device is not in its account"));
        }
        let id = chain.id();
        let Some(old) = self.chains.get(&id) else {
            self.chains.insert(id, chain);
            return Ok(Observed::New);
        };
        if old.extends(&chain) {
            // Same or older than what we hold: nothing new.
            return Ok(Observed::Unchanged);
        }
        if !chain.extends(old) {
            return Err(Error::Malformed("account chain fork"));
        }
        let (o, n) = (old.state(), chain.state());
        let added = n
            .devices
            .iter()
            .map(|(d, _)| *d)
            .filter(|d| !o.has(d))
            .collect();
        let removed = n
            .removed
            .iter()
            .copied()
            .filter(|d| !o.removed.contains(d))
            .collect();
        self.chains.insert(id, chain);
        Ok(Observed::Updated { added, removed })
    }

    pub fn get(&self, id: &AccountId) -> Option<&AccountChain> {
        self.chains.get(id)
    }

    /// The account `device` currently belongs to, if any.
    pub fn account_of(&self, device: &PublicIdentity) -> Option<&AccountChain> {
        self.chains.values().find(|c| c.state().has(device))
    }

    /// True if some known account has removed `device`.
    pub fn is_revoked(&self, device: &PublicIdentity) -> bool {
        self.chains
            .values()
            .any(|c| c.state().removed.contains(device))
    }

    pub fn iter(&self) -> impl Iterator<Item = &AccountChain> {
        self.chains.values()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let parts: Vec<Vec<u8>> = self
            .chains
            .values()
            .map(AccountChain::encode)
            .collect::<Result<_>>()?;
        cbor::to_vec(parts.iter().map(Vec::len).sum::<usize>() + 32, |e| {
            e.array_len(parts.len())?;
            for p in &parts {
                e.bytes(p)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut chains = std::collections::HashMap::new();
        for _ in 0..dec.array_len()? {
            let c = AccountChain::decode(dec.bytes()?)?;
            chains.insert(c.id(), c);
        }
        finish(&dec)?;
        Ok(Self { chains })
    }
}

// ------------------------------------------------------------- link codes

/// Out-of-band code an existing device shows to a new one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkCode {
    pub device: Fingerprint,
    pub addr: String,
    pub secret: [u8; 16],
}

const LINK_SCHEME: &str = "threnody-link://";

impl LinkCode {
    pub fn new(device: Fingerprint, addr: String) -> Self {
        Self {
            device,
            addr,
            secret: random_bytes(),
        }
    }

    /// Proof that the holder of `secret` is `device` on session `session_id`.
    pub fn proof(&self, session_id: &[u8; 32], device: &PublicIdentity) -> [u8; 32] {
        link_proof(&self.secret, session_id, device)
    }
}

pub fn link_proof(secret: &[u8; 16], session_id: &[u8; 32], device: &PublicIdentity) -> [u8; 32] {
    kdf::derive(label::LINK_PROOF, &[secret, session_id, device.as_bytes()])
}

impl fmt::Display for LinkCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hex: String = self.secret.iter().map(|b| format!("{b:02x}")).collect();
        write!(
            f,
            "{LINK_SCHEME}{}@{}#{hex}",
            self.device.compact(),
            self.addr
        )
    }
}

impl FromStr for LinkCode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let rest = s
            .trim()
            .strip_prefix(LINK_SCHEME)
            .ok_or(Error::Malformed("link code"))?;
        let (fp, rest) = rest.split_once('@').ok_or(Error::Malformed("link code"))?;
        let (addr, hex) = rest.rsplit_once('#').ok_or(Error::Malformed("link code"))?;
        if hex.len() != 32 || addr.is_empty() {
            return Err(Error::Malformed("link code"));
        }
        let mut secret = [0u8; 16];
        for (i, b) in secret.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .map_err(|_| Error::Malformed("link code"))?;
        }
        Ok(Self {
            device: fp.parse()?,
            addr: addr.to_owned(),
            secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(chain: &mut AccountChain, by: &[&Identity], new: &Identity, name: &str) -> Result<()> {
        let mut link = chain.propose(Action::Add {
            device: new.public(),
            name: name.into(),
        });
        for s in by {
            link.sign(s)?;
        }
        link.sign(new)?;
        chain.append(link)
    }

    #[test]
    fn devices_can_be_renamed_by_the_account_only() {
        let (a, b, outsider) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let mut chain = AccountChain::genesis(&a, "device").unwrap();
        add(&mut chain, &[&a], &b, "opaque").unwrap();
        let id = chain.id();
        let rename = |chain: &AccountChain, d: &Identity, name: &str| {
            chain.propose(Action::Rename {
                device: d.public(),
                name: name.into(),
            })
        };
        // Either device may rename either device.
        let mut l = rename(&chain, &a, "Pixel 8a");
        l.sign(&b).unwrap();
        chain.append(l).unwrap();
        assert_eq!(chain.state().name_of(&a.public()), Some("Pixel 8a"));
        assert_eq!(chain.state().devices.len(), 2, "membership unchanged");
        assert_eq!(chain.id(), id);
        // It survives the wire, and old links still verify.
        let again = AccountChain::decode(&chain.encode().unwrap()).unwrap();
        assert_eq!(again.state().name_of(&a.public()), Some("Pixel 8a"));
        assert!(again.extends(&chain) && chain.extends(&again));
        // Not by an outsider, not of an outsider, not with a bad name.
        let mut l = rename(&chain, &b, "evil");
        l.sign(&outsider).unwrap();
        assert!(chain.clone().append(l).is_err());
        let mut l = rename(&chain, &outsider, "ghost");
        l.sign(&a).unwrap();
        assert!(chain.clone().append(l).is_err());
        let mut l = rename(&chain, &b, "bad\nname");
        l.sign(&a).unwrap();
        assert!(chain.clone().append(l).is_err(), "control characters");
        assert_eq!(clean_name("bad\nname"), "badname");
    }

    #[test]
    fn genesis_add_remove_and_round_trip() {
        let (a, b, c) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let mut chain = AccountChain::genesis(&a, "laptop").unwrap();
        let id = chain.id();
        add(&mut chain, &[&a], &b, "phone").unwrap();
        // Equal peers: the phone can add the tablet.
        add(&mut chain, &[&b], &c, "tablet").unwrap();
        let mut rm = chain.propose(Action::Remove { device: a.public() });
        rm.sign(&c).unwrap();
        chain.append(rm).unwrap();

        let st = chain.state();
        assert_eq!(st.id, id, "account id survives device changes");
        assert_eq!(
            st.devices
                .iter()
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>(),
            ["phone", "tablet"]
        );
        assert!(st.removed.contains(&a.public()));

        let again = AccountChain::decode(&chain.encode().unwrap()).unwrap();
        assert_eq!(again, chain);
        assert_ne!(id.fingerprint(), a.public().fingerprint());
    }

    #[test]
    fn unauthorised_and_malformed_links_are_rejected() {
        let (a, b, x) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let mut chain = AccountChain::genesis(&a, "a").unwrap();
        // Outsider cannot add itself.
        assert!(add(&mut chain, &[], &x, "x").is_err());
        // An add needs the new device's own signature.
        let mut l = chain.propose(Action::Add {
            device: b.public(),
            name: "b".into(),
        });
        l.sign(&a).unwrap();
        assert!(chain.append(l).is_err());
        add(&mut chain, &[&a], &b, "b").unwrap();
        // No duplicate adds; no removing the last device; no re-adding removed devices.
        assert!(add(&mut chain, &[&a], &b, "b").is_err());
        let mut rm = chain.propose(Action::Remove { device: b.public() });
        rm.sign(&a).unwrap();
        chain.append(rm).unwrap();
        assert!(
            add(&mut chain, &[&a], &b, "b").is_err(),
            "removed device re-added"
        );
        let mut last = chain.propose(Action::Remove { device: a.public() });
        last.sign(&a).unwrap();
        assert!(chain.append(last).is_err());
        // Out-of-order links.
        let mut stale = chain.propose(Action::Remove { device: a.public() });
        stale.seq += 1;
        stale.sign(&a).unwrap();
        assert!(chain.append(stale).is_err());
    }

    #[test]
    fn thresholds_above_one_need_more_signatures() {
        let (a, b, c) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let mut chain = AccountChain::genesis(&a, "a").unwrap();
        add(&mut chain, &[&a], &b, "b").unwrap();
        let mut raise = chain.propose(Action::Add {
            device: c.public(),
            name: "c".into(),
        });
        raise.threshold = 2;
        raise.sign(&a).unwrap();
        raise.sign(&c).unwrap();
        chain.append(raise).unwrap();
        let mut rm = chain.propose(Action::Remove { device: b.public() });
        rm.sign(&a).unwrap();
        assert!(
            chain.clone().append(rm.clone()).is_err(),
            "one signature below threshold 2"
        );
        rm.sign(&c).unwrap();
        chain.append(rm).unwrap();
    }

    #[test]
    fn forks_are_detected_and_tampering_fails() {
        let (a, b, c) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let base = AccountChain::genesis(&a, "a").unwrap();
        let (mut left, mut right) = (base.clone(), base.clone());
        add(&mut left, &[&a], &b, "b").unwrap();
        add(&mut right, &[&a], &c, "c").unwrap();
        assert!(left.extends(&base) && !left.extends(&right) && !right.extends(&left));

        let mut bytes = left.encode().unwrap();
        let i = bytes.len() - 70;
        bytes[i] ^= 1;
        assert!(AccountChain::decode(&bytes).is_err());
    }

    #[test]
    fn account_book_pins_updates_and_rejects_forks() {
        let (a, b, c) = (
            Identity::generate(),
            Identity::generate(),
            Identity::generate(),
        );
        let base = AccountChain::genesis(&a, "a").unwrap();
        let mut book = AccountBook::default();
        assert_eq!(
            book.observe(base.clone(), &a.public()).unwrap(),
            Observed::New
        );
        assert!(
            book.observe(base.clone(), &b.public()).is_err(),
            "presenter not in account"
        );

        let mut grown = base.clone();
        add(&mut grown, &[&a], &b, "b").unwrap();
        assert_eq!(
            book.observe(grown.clone(), &b.public()).unwrap(),
            Observed::Updated {
                added: vec![b.public()],
                removed: vec![]
            }
        );
        assert_eq!(
            book.observe(base.clone(), &a.public()).unwrap(),
            Observed::Unchanged,
            "stale chain"
        );

        let mut fork = base.clone();
        add(&mut fork, &[&a], &c, "c").unwrap();
        assert!(book.observe(fork, &a.public()).is_err(), "fork accepted");

        let mut rm = grown.propose(Action::Remove { device: a.public() });
        rm.sign(&b).unwrap();
        let mut shrunk = grown.clone();
        shrunk.append(rm).unwrap();
        assert_eq!(
            book.observe(shrunk, &b.public()).unwrap(),
            Observed::Updated {
                added: vec![],
                removed: vec![a.public()]
            }
        );
        assert!(book.is_revoked(&a.public()));
        assert_eq!(book.account_of(&b.public()).unwrap().id(), base.id());
        let again = AccountBook::decode(&book.encode().unwrap()).unwrap();
        assert!(again.is_revoked(&a.public()));
    }

    #[test]
    fn link_codes_round_trip_and_proofs_bind_session_and_device() {
        let (e, n) = (Identity::generate(), Identity::generate());
        let code = LinkCode::new(e.public().fingerprint(), "192.0.2.1:7450".into());
        let parsed: LinkCode = code.to_string().parse().unwrap();
        assert_eq!(parsed, code);
        let sid = [1u8; 32];
        assert_ne!(
            code.proof(&sid, &n.public()),
            code.proof(&[2u8; 32], &n.public())
        );
        assert_ne!(code.proof(&sid, &n.public()), code.proof(&sid, &e.public()));
        assert!("threnody-link://x@y#00".parse::<LinkCode>().is_err());
    }
}
