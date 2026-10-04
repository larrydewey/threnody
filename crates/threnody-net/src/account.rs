//! Accounts and device linking over sessions (`docs/appendix-j-devices.md`).
//!
//! ```text
//! AccountMsg = { 0: op, ? 1: bytes, ? 2: bytes, ? 3: tstr }
//! op 1 chain        { 1: our account chain }
//! op 2 link-request { 1: proof (32), 3: device name }           new -> existing
//! op 3 link-offer   { 1: chain, 2: proposed link signed by us }  existing -> new
//! op 4 link-consent { 2: link signed by both }                   new -> existing
//! op 5 contact-sync { 1: contact snapshot }                      own devices only
//! op 6 link-refused {}                                           existing -> new
//! ```

use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::account::{
    AccountBook, AccountChain, AccountId, Action, Link, LinkCode, Observed, link_proof,
};
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use tokio::sync::oneshot;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// How long a link code stays valid.
pub const LINK_CODE_TTL: Duration = Duration::from_secs(600);
const LINK_TIMEOUT: Duration = Duration::from_secs(15);

enum AccountMsg {
    Chain(Vec<u8>),
    LinkRequest { proof: [u8; 32], name: String },
    LinkOffer { chain: Vec<u8>, link: Vec<u8> },
    LinkConsent { link: Vec<u8> },
    ContactSync(Vec<u8>),
    LinkRefused,
}

impl AccountMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(4096, |e| {
            match self {
                Self::Chain(c) => {
                    e.map_len(2)?.u8(0)?.u8(1)?.u8(1)?.bytes(c)?;
                }
                Self::LinkRequest { proof, name } => {
                    e.map_len(3)?.u8(0)?.u8(2)?.u8(1)?.bytes(proof)?;
                    e.u8(3)?.str(name)?;
                }
                Self::LinkOffer { chain, link } => {
                    e.map_len(3)?.u8(0)?.u8(3)?.u8(1)?.bytes(chain)?;
                    e.u8(2)?.bytes(link)?;
                }
                Self::LinkConsent { link } => {
                    e.map_len(2)?.u8(0)?.u8(4)?.u8(2)?.bytes(link)?;
                }
                Self::ContactSync(s) => {
                    e.map_len(2)?.u8(0)?.u8(5)?.u8(1)?.bytes(s)?;
                }
                Self::LinkRefused => {
                    e.map_len(1)?.u8(0)?.u8(6)?;
                }
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut one, mut two, mut name) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => one = Some(d.bytes()?.to_vec()),
                2 => two = Some(d.bytes()?.to_vec()),
                3 => name = Some(d.str()?.to_owned()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(op, "account op")? {
            1 => Self::Chain(required(one, "chain")?),
            2 => Self::LinkRequest {
                proof: required(one, "proof")?
                    .try_into()
                    .map_err(|_| threnody_core::Error::Malformed("proof length"))?,
                name: required(name, "device name")?,
            },
            3 => Self::LinkOffer {
                chain: required(one, "chain")?,
                link: required(two, "link")?,
            },
            4 => Self::LinkConsent {
                link: required(two, "link")?,
            },
            5 => Self::ContactSync(required(one, "snapshot")?),
            6 => Self::LinkRefused,
            other => return Err(threnody_core::Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// Linking state on both sides.
#[derive(Default)]
pub(crate) struct LinkState {
    /// Codes we issued, with their expiry.
    codes: Vec<(LinkCode, Instant)>,
    /// Proposals we offered, awaiting consent: (new device, link).
    offered: Vec<(PublicIdentity, Link)>,
    /// New-device side: the device we are linking through, and who to tell.
    joining: Option<(PublicIdentity, oneshot::Sender<Result<AccountId>>)>,
}

impl Node {
    fn account_send(&self, to: &PublicIdentity, m: &AccountMsg) -> bool {
        m.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Account(b)).is_ok())
    }

    /// Our account chain.
    pub fn account(&self) -> AccountChain {
        lock(&self.shared.account).clone()
    }

    /// The account `device` belongs to, if known (ours included).
    pub fn account_of(&self, device: &PublicIdentity) -> Option<AccountChain> {
        let own = self.account();
        if own.state().has(device) {
            return Some(own);
        }
        lock(&self.shared.accounts).account_of(device).cloned()
    }

    /// True for our own other devices.
    pub fn is_own_device(&self, device: &PublicIdentity) -> bool {
        *device != self.identity() && lock(&self.shared.account).state().has(device)
    }

    /// True for devices removed from their account (ours or a peer's).
    pub(crate) fn is_revoked(&self, device: &PublicIdentity) -> bool {
        lock(&self.shared.account).state().removed.contains(device)
            || lock(&self.shared.accounts).is_revoked(device)
    }

    /// Messages to send when a session with `peer` starts.
    pub(crate) fn account_hello(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let mut out = Vec::new();
        if let Ok(b) = self.account().encode() {
            out.extend(AccountMsg::Chain(b).encode().ok().map(AppMessage::Account));
        }
        if self.is_own_device(peer) {
            out.extend(self.contact_sync_message());
        }
        out
    }

    fn contact_sync_message(&self) -> Option<AppMessage> {
        let snap = lock(&self.shared.contacts).sync_snapshot().ok()?;
        AccountMsg::ContactSync(snap)
            .encode()
            .ok()
            .map(AppMessage::Account)
    }

    /// Sends our contact book to every connected own device.
    pub(crate) fn push_contact_sync(&self) {
        let own: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .map(|s| s.peer)
            .filter(|p| self.is_own_device(p))
            .collect();
        if own.is_empty() {
            return;
        }
        if let Some(m) = self.contact_sync_message() {
            for p in own {
                let _ = self.send(&p, m.clone());
            }
        }
    }

    fn broadcast_chain(&self) {
        let Ok(b) = self.account().encode() else {
            return;
        };
        let Ok(m) = AccountMsg::Chain(b).encode() else {
            return;
        };
        for s in self.sessions() {
            let _ = self.send(&s.peer, AppMessage::Account(m.clone()));
        }
    }

    fn persist_account(&self) {
        let chain = lock(&self.shared.account).encode();
        self.shared.save_state("account", chain);
    }

    fn persist_accounts(&self) {
        let book = lock(&self.shared.accounts).encode();
        self.shared.save_state("accounts", book);
    }

    pub(crate) fn on_account(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = AccountMsg::decode(payload) else {
            return;
        };
        match msg {
            AccountMsg::Chain(b) => {
                if let Ok(chain) = AccountChain::decode(&b) {
                    self.on_chain(from, chain);
                }
            }
            AccountMsg::LinkRequest { proof, name } => self.on_link_request(from, proof, &name),
            AccountMsg::LinkOffer { chain, link } => self.on_link_offer(from, &chain, &link),
            AccountMsg::LinkConsent { link } => self.on_link_consent(from, &link),
            AccountMsg::LinkRefused => {
                let mut st = lock(&self.shared.linking);
                if st.joining.as_ref().is_some_and(|(e, _)| *e == from)
                    && let Some((_, tx)) = st.joining.take()
                {
                    let _ = tx.send(Err(NetError::Refused("link code rejected".into())));
                }
            }
            AccountMsg::ContactSync(snap) => {
                if self.is_own_device(&from) {
                    let changed = {
                        let mut c = lock(&self.shared.contacts);
                        let mut changed = c.merge_snapshot(&snap, now_ms()).unwrap_or(false);
                        // Our other device's record of *us* is not a contact.
                        changed |= c.remove(&self.identity());
                        if changed {
                            self.shared.save_contacts(&c);
                        }
                        changed
                    };
                    if changed {
                        self.emit(Event::ContactsSynced { from });
                    }
                }
            }
        }
    }

    fn on_chain(&self, from: PublicIdentity, chain: AccountChain) {
        let own = self.account();
        if chain.id() == own.id() {
            self.on_own_chain(from, own, chain);
            return;
        }
        if !chain.state().has(&from) {
            return;
        }
        let observed = lock(&self.shared.accounts).observe(chain.clone(), &from);
        let id = chain.id();
        match observed {
            Err(_) => self.emit(Event::AccountFork {
                device: from,
                account: id,
            }),
            Ok(Observed::Unchanged) => self.tag_devices(&chain),
            Ok(Observed::New) => {
                self.persist_accounts();
                self.tag_devices(&chain);
                self.emit(Event::AccountChanged {
                    account: id,
                    added: vec![],
                    removed: vec![],
                });
            }
            Ok(Observed::Updated { added, removed }) => {
                self.persist_accounts();
                self.tag_devices(&chain);
                for d in &removed {
                    self.revoke_device(d);
                }
                self.emit(Event::AccountChanged {
                    account: id,
                    added,
                    removed,
                });
            }
        }
    }

    /// Records the account of every device in `chain` and gives devices we
    /// haven't met the account's approval (from any device we know).
    fn tag_devices(&self, chain: &AccountChain) {
        let id = chain.id();
        let mut contacts = lock(&self.shared.contacts);
        let approved = chain
            .state()
            .devices
            .iter()
            .filter_map(|(d, _)| contacts.get(d))
            .any(|c| c.local_approved);
        let mut changed = false;
        for (d, _) in &chain.state().devices {
            if contacts.get(d).is_none() {
                contacts.observe(*d, None, now_ms());
                if let Some(c) = contacts.get_mut(d) {
                    c.local_approved = approved;
                    c.approval_changed_ms = now_ms();
                }
                changed = true;
            }
            if let Some(c) = contacts.get_mut(d)
                && c.account != Some(id)
            {
                c.account = Some(id);
                changed = true;
            }
        }
        if changed {
            self.shared.save_contacts(&contacts);
        }
    }

    /// A device left its account: stop trusting it everywhere.
    fn revoke_device(&self, device: &PublicIdentity) {
        {
            let mut contacts = lock(&self.shared.contacts);
            if let Some(c) = contacts.get_mut(device) {
                c.local_approved = false;
                c.remote_approved = false;
                c.discovery_key = None;
                c.approval_changed_ms = now_ms();
            }
            self.shared.save_contacts(&contacts);
        }
        self.disconnect(device);
    }

    fn on_own_chain(&self, from: PublicIdentity, own: AccountChain, chain: AccountChain) {
        if !chain.state().has(&from) && !own.state().has(&from) {
            return;
        }
        if chain.extends(&own) && chain != own {
            let removed: Vec<PublicIdentity> = chain
                .state()
                .removed
                .iter()
                .copied()
                .filter(|d| !own.state().removed.contains(d))
                .collect();
            *lock(&self.shared.account) = chain.clone();
            self.persist_account();
            self.tag_own_devices();
            if removed.contains(&self.identity()) {
                self.emit(Event::ThisDeviceRemoved);
            }
            for d in &removed {
                self.revoke_device(d);
            }
            self.emit(Event::AccountChanged {
                account: chain.id(),
                added: vec![],
                removed,
            });
            // Pass it on so every own device converges.
            self.broadcast_chain();
        } else if own.extends(&chain) {
            if own != chain
                && let Ok(b) = own.encode()
            {
                self.account_send(&from, &AccountMsg::Chain(b));
            }
        } else {
            self.emit(Event::AccountFork {
                device: from,
                account: own.id(),
            });
        }
        if self.is_own_device(&from) {
            self.tag_own_devices();
        }
    }

    /// Own devices approve each other automatically.
    fn tag_own_devices(&self) {
        let own = self.account();
        let me = self.identity();
        let mut to_approve = Vec::new();
        {
            let mut contacts = lock(&self.shared.contacts);
            for (d, name) in &own.state().devices {
                if *d == me {
                    continue;
                }
                contacts.observe(*d, None, now_ms());
                if let Some(c) = contacts.get_mut(d) {
                    c.account = Some(own.id());
                    if c.petname.is_none() {
                        c.petname = Some(format!("my {name}"));
                    }
                    if !c.local_approved {
                        to_approve.push(*d);
                    }
                }
            }
            self.shared.save_contacts(&contacts);
        }
        // Through the normal path, so the peer device hears it too.
        for d in to_approve {
            let _ = self.set_approval(&d, true);
        }
    }

    // ------------------------------------------------- linking: existing side

    /// Issues a one-time link code for a new device to join our account.
    pub fn create_link_code(&self, addr: String) -> LinkCode {
        let code = LinkCode::new(self.identity().fingerprint(), addr);
        let mut st = lock(&self.shared.linking);
        st.codes.retain(|(_, t)| t.elapsed() < LINK_CODE_TTL);
        st.codes.push((code.clone(), Instant::now()));
        code
    }

    fn on_link_request(&self, from: PublicIdentity, proof: [u8; 32], name: &str) {
        let Some(session_id) = self
            .sessions()
            .into_iter()
            .find(|s| s.peer == from && s.via.is_none())
            .map(|s| s.session_id)
        else {
            return;
        };
        let matched = {
            let mut st = lock(&self.shared.linking);
            st.codes.retain(|(_, t)| t.elapsed() < LINK_CODE_TTL);
            let pos = st
                .codes
                .iter()
                .position(|(c, _)| link_proof(&c.secret, &session_id, &from) == proof);
            pos.map(|p| st.codes.remove(p))
        };
        if matched.is_none() || self.is_revoked(&from) {
            self.account_send(&from, &AccountMsg::LinkRefused);
            self.emit(Event::LinkRejected { device: from });
            return;
        }
        let own = self.account();
        let mut link = own.propose(Action::Add {
            device: from,
            name: name.chars().filter(|c| !c.is_control()).take(64).collect(),
        });
        if link.sign(self.identity_ref()).is_err() {
            return;
        }
        let (Ok(chain), Ok(l)) = (own.encode(), link.encode()) else {
            return;
        };
        lock(&self.shared.linking).offered.push((from, link));
        self.account_send(&from, &AccountMsg::LinkOffer { chain, link: l });
    }

    fn on_link_consent(&self, from: PublicIdentity, link: &[u8]) {
        let Ok(link) = Link::decode(link) else { return };
        let offered = {
            let mut st = lock(&self.shared.linking);
            let pos = st
                .offered
                .iter()
                .position(|(d, l)| *d == from && l.body().ok() == link.body().ok());
            pos.map(|p| st.offered.remove(p))
        };
        if offered.is_none() {
            return;
        }
        let appended = lock(&self.shared.account).append(link).is_ok();
        if !appended {
            self.emit(Event::LinkRejected { device: from });
            return;
        }
        self.persist_account();
        self.tag_own_devices();
        self.broadcast_chain();
        if let Some(m) = self.contact_sync_message() {
            let _ = self.send(&from, m);
        }
        self.emit(Event::DeviceLinked { device: from });
    }

    // ------------------------------------------------------ linking: new side

    /// Joins the account of the device that issued `code`. This device's
    /// own single-device account is replaced. Returns the account id.
    pub async fn link_with(&self, code: &LinkCode) -> Result<AccountId> {
        let existing = self.connect(&code.addr, Some(code.device)).await?;
        let session_id = self
            .sessions()
            .into_iter()
            .find(|s| s.peer == existing && s.via.is_none())
            .map(|s| s.session_id)
            .ok_or(NetError::Closed)?;
        let (tx, rx) = oneshot::channel();
        lock(&self.shared.linking).joining = Some((existing, tx));
        let me = self.identity();
        let name = self
            .account()
            .state()
            .name_of(&me)
            .unwrap_or("device")
            .to_owned();
        let proof = code.proof(&session_id, &me);
        if !self.account_send(&existing, &AccountMsg::LinkRequest { proof, name }) {
            return Err(NetError::Closed);
        }
        tokio::time::timeout(LINK_TIMEOUT, rx)
            .await
            .map_err(|_| NetError::Timeout)?
            .map_err(|_| NetError::Closed)?
    }

    fn on_link_offer(&self, from: PublicIdentity, chain: &[u8], link: &[u8]) {
        let joining = lock(&self.shared.linking)
            .joining
            .as_ref()
            .is_some_and(|(e, _)| *e == from);
        if !joining {
            return;
        }
        let me = self.identity();
        let result = (|| -> Result<(AccountChain, Link)> {
            let chain = AccountChain::decode(chain)?;
            let mut link = Link::decode(link)?;
            // The offer must add exactly us, on top of exactly this chain,
            // signed by the device we are linking through.
            let expected = chain.propose(Action::Add {
                device: me,
                name: match &link.action {
                    Action::Add { name, .. } => name.clone(),
                    Action::Remove { .. } => return Err(NetError::Refused("not an add".into())),
                },
            });
            if expected.body()? != link.body()? || !chain.state().has(&from) {
                return Err(NetError::Refused("unexpected link offer".into()));
            }
            link.sign(self.identity_ref())?;
            let mut joined = chain.clone();
            joined.append(link.clone())?;
            Ok((joined, link))
        })();
        let finish = |r: Result<AccountId>| {
            if let Some((_, tx)) = lock(&self.shared.linking).joining.take() {
                let _ = tx.send(r);
            }
        };
        match result {
            Ok((joined, link)) => {
                let Ok(l) = link.encode() else { return };
                if !self.account_send(&from, &AccountMsg::LinkConsent { link: l }) {
                    finish(Err(NetError::Closed));
                    return;
                }
                // Adopt the account: we verified the chain and our own add.
                *lock(&self.shared.account) = joined.clone();
                self.persist_account();
                self.tag_own_devices();
                finish(Ok(joined.id()));
            }
            Err(e) => finish(Err(e)),
        }
    }

    // --------------------------------------------------------- management

    /// Removes one of our devices (possibly this one) from the account.
    pub fn remove_device(&self, device: &PublicIdentity) -> Result<()> {
        {
            let mut own = lock(&self.shared.account);
            let mut link = own.propose(Action::Remove { device: *device });
            link.sign(self.identity_ref())?;
            own.append(link)?;
        }
        self.persist_account();
        self.broadcast_chain();
        if *device == self.identity() {
            self.emit(Event::ThisDeviceRemoved);
        } else {
            self.revoke_device(device);
        }
        Ok(())
    }

    /// Approves or revokes every device of `account`.
    pub fn set_account_approval(&self, account: &AccountId, approved: bool) -> Result<usize> {
        let devices: Vec<PublicIdentity> = match lock(&self.shared.accounts).get(account) {
            Some(c) => c.state().devices.iter().map(|(d, _)| *d).collect(),
            None => return Err(NetError::Refused("unknown account".into())),
        };
        for d in &devices {
            self.set_approval(d, approved)?;
        }
        Ok(devices.len())
    }

    /// Known peer accounts.
    pub fn known_accounts(&self) -> AccountBook {
        lock(&self.shared.accounts).clone()
    }
}
