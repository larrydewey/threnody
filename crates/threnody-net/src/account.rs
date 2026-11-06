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
//! op 7 sibling-bundle { 1: shared prekey bundle }                own devices only
//! op 8 transcript   { 1: conversation, 2: history }              own devices only
//! op 9 folders      { 1: folders record }                        own devices only
//! op 10 list        { 1: list }                                  own devices only
//! op 11 file-want   { 1: conversation, 2: Want }                 own devices only
//! op 12 file-part   { 2: Part }                                  own devices only
//! op 13 file-missing { 2: Missing }                              own devices only
//! op 14 ticks       { 1: conversation, 2: Ticks }                own devices only
//! ```
//!
//! Ops 11-14 go only to devices whose `Hello` has `FEATURE_DEVICE_SYNC`
//! (see `filesync` and `sync`).

use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::account::{
    AccountBook, AccountChain, AccountId, Action, Link, LinkCode, Observed, link_proof,
};
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::history::ConversationId;
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use tokio::sync::oneshot;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// How long a link code stays valid.
pub const LINK_CODE_TTL: Duration = Duration::from_secs(600);
const LINK_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg_attr(test, derive(Debug, PartialEq))]
enum AccountMsg {
    Chain(Vec<u8>),
    LinkRequest {
        proof: [u8; 32],
        name: String,
    },
    LinkOffer {
        chain: Vec<u8>,
        link: Vec<u8>,
    },
    LinkConsent {
        link: Vec<u8>,
    },
    ContactSync(Vec<u8>),
    LinkRefused,
    SiblingBundle(Vec<u8>),
    /// History entries for one conversation (see `sync`).
    Transcript {
        conversation: Vec<u8>,
        history: Vec<u8>,
    },
    /// Folders, pinned and archived chats (see `folders`).
    Folders(Vec<u8>),
    /// A list, with whom it's shared, or that it's deleted (see `lists`).
    List(Vec<u8>),
    /// Asks for a file's contents (see `filesync`).
    FileWant {
        conversation: Vec<u8>,
        want: Vec<u8>,
    },
    /// Part of a file's contents.
    FilePart(Vec<u8>),
    /// The file asked for isn't here.
    FileMissing(Vec<u8>),
    /// Delivery and read ticks for one conversation (see `sync`).
    Ticks {
        conversation: Vec<u8>,
        ticks: Vec<u8>,
    },
}

/// An `AppMessage` asking our other device for a file.
pub(crate) fn file_want_message(conversation: Vec<u8>, want: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::FileWant { conversation, want }
        .encode()
        .ok()
        .map(AppMessage::Account)
}

/// An `AppMessage` carrying part of a file.
pub(crate) fn file_part_message(part: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::FilePart(part)
        .encode()
        .ok()
        .map(AppMessage::Account)
}

/// An `AppMessage` saying a file isn't here.
pub(crate) fn file_missing_message(body: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::FileMissing(body)
        .encode()
        .ok()
        .map(AppMessage::Account)
}

/// An `AppMessage` carrying ticks for `conversation`.
pub(crate) fn ticks_message(conversation: Vec<u8>, ticks: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::Ticks {
        conversation,
        ticks,
    }
    .encode()
    .ok()
    .map(AppMessage::Account)
}

/// An `AppMessage` carrying a list for our other devices.
pub(crate) fn list_message(payload: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::List(payload)
        .encode()
        .ok()
        .map(AppMessage::Account)
}

/// An `AppMessage` carrying our folders record.
pub(crate) fn folders_message(record: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::Folders(record)
        .encode()
        .ok()
        .map(AppMessage::Account)
}

/// An `AppMessage` carrying a transcript of `history` for `conversation`.
pub(crate) fn transcript(conversation: Vec<u8>, history: Vec<u8>) -> Option<AppMessage> {
    AccountMsg::Transcript {
        conversation,
        history,
    }
    .encode()
    .ok()
    .map(AppMessage::Account)
}

impl AccountMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let hint = match self {
            Self::Transcript { history, .. } => history.len() + 64,
            Self::FilePart(p) => p.len() + 64,
            Self::Ticks { ticks, .. } => ticks.len() + 96,
            _ => 4096,
        };
        cbor::to_vec(hint, |e| {
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
                Self::SiblingBundle(b) => {
                    e.map_len(2)?.u8(0)?.u8(7)?.u8(1)?.bytes(b)?;
                }
                Self::Folders(f) => {
                    e.map_len(2)?.u8(0)?.u8(9)?.u8(1)?.bytes(f)?;
                }
                Self::List(l) => {
                    e.map_len(2)?.u8(0)?.u8(10)?.u8(1)?.bytes(l)?;
                }
                Self::Transcript {
                    conversation,
                    history,
                } => {
                    e.map_len(3)?.u8(0)?.u8(8)?.u8(1)?.bytes(conversation)?;
                    e.u8(2)?.bytes(history)?;
                }
                Self::FileWant { conversation, want } => {
                    e.map_len(3)?.u8(0)?.u8(11)?.u8(1)?.bytes(conversation)?;
                    e.u8(2)?.bytes(want)?;
                }
                Self::FilePart(p) => {
                    e.map_len(2)?.u8(0)?.u8(12)?.u8(2)?.bytes(p)?;
                }
                Self::FileMissing(m) => {
                    e.map_len(2)?.u8(0)?.u8(13)?.u8(2)?.bytes(m)?;
                }
                Self::Ticks {
                    conversation,
                    ticks,
                } => {
                    e.map_len(3)?.u8(0)?.u8(14)?.u8(1)?.bytes(conversation)?;
                    e.u8(2)?.bytes(ticks)?;
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
            7 => Self::SiblingBundle(required(one, "bundle")?),
            9 => Self::Folders(required(one, "folders")?),
            10 => Self::List(required(one, "list")?),
            8 => Self::Transcript {
                conversation: required(one, "conversation")?,
                history: required(two, "history")?,
            },
            11 => Self::FileWant {
                conversation: required(one, "conversation")?,
                want: required(two, "want")?,
            },
            12 => Self::FilePart(required(two, "part")?),
            13 => Self::FileMissing(required(two, "missing")?),
            14 => Self::Ticks {
                conversation: required(one, "conversation")?,
                ticks: required(two, "ticks")?,
            },
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
    pub fn is_revoked(&self, device: &PublicIdentity) -> bool {
        lock(&self.shared.account).state().removed.contains(device)
            || lock(&self.shared.accounts).is_revoked(device)
    }

    /// Whether `peer` may see our account chain: the devices of our
    /// account and their names (a host name or phone model, often the
    /// owner's name). Our own devices and contacts we accepted may; a
    /// stranger only once accepted.
    pub(crate) fn shows_account_to(&self, peer: &PublicIdentity) -> bool {
        self.is_accepted(peer)
    }

    /// Messages to send when a session with `peer` starts.
    pub(crate) fn account_hello(&self, peer: &PublicIdentity) -> Vec<AppMessage> {
        let mut out = Vec::new();
        if self.shows_account_to(peer)
            && let Ok(b) = self.account().encode()
        {
            out.extend(AccountMsg::Chain(b).encode().ok().map(AppMessage::Account));
        }
        if self.is_own_device(peer) {
            out.extend(self.contact_sync_message());
            out.extend(self.folders_hello());
            out.extend(self.lists_hello());
            out.extend(self.transcripts_for(peer));
            if let Some(b) = self.shared_bundle() {
                out.extend(
                    AccountMsg::SiblingBundle(b)
                        .encode()
                        .ok()
                        .map(AppMessage::Account),
                );
            }
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
            for p in &own {
                let _ = self.send(p, m.clone());
            }
        }
        if let Some(m) = self.folders_hello() {
            for p in &own {
                let _ = self.send(p, m.clone());
            }
        }
        for m in self.lists_hello() {
            for p in &own {
                let _ = self.send(p, m.clone());
            }
        }
        // A device just linked (in a session that started before it was
        // ours) gets our history now.
        for p in own {
            if !self.has_synced(&p) {
                for m in self.transcripts_for(&p) {
                    let _ = self.send(&p, m);
                }
                // Its Hello came before it was ours: ticks and file
                // fetches start now rather than next session.
                self.device_sync_hello(&p);
            }
        }
    }

    fn broadcast_chain(&self) {
        let peers: Vec<PublicIdentity> = self.sessions().into_iter().map(|s| s.peer).collect();
        self.send_chain(&peers);
    }

    /// Sends our account chain to those of `peers` with a session that may
    /// see it.
    pub(crate) fn send_chain(&self, peers: &[PublicIdentity]) {
        let Ok(b) = self.account().encode() else {
            return;
        };
        let Ok(m) = AccountMsg::Chain(b).encode() else {
            return;
        };
        for p in peers.iter().filter(|p| self.shows_account_to(p)) {
            let _ = self.send(p, AppMessage::Account(m.clone()));
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

    /// Handles an account message from `from`. False for a chain that
    /// waits until `from` is accepted (see [`Node::chain_wanted`]): the
    /// session offers it again then.
    pub(crate) fn on_account(&self, from: PublicIdentity, payload: &[u8]) -> bool {
        let Ok(msg) = AccountMsg::decode(payload) else {
            return true;
        };
        match msg {
            AccountMsg::Chain(b) => {
                if !self.chain_wanted(&from, &b) {
                    return false;
                }
                if let Ok(chain) = AccountChain::decode(&b) {
                    self.on_chain(from, chain);
                }
            }
            AccountMsg::LinkRequest { proof, name } => self.on_link_request(from, proof, &name),
            AccountMsg::LinkOffer { chain, link } => self.on_link_offer(from, &chain, &link),
            AccountMsg::LinkConsent { link } => self.on_link_consent(from, &link),
            AccountMsg::SiblingBundle(b) => self.on_sibling_bundle(from, &b),
            AccountMsg::Transcript {
                conversation,
                history,
            } => self.on_transcript(from, &conversation, &history),
            AccountMsg::Folders(f) => self.on_folders(from, &f),
            AccountMsg::List(l) => self.on_list_sync(from, &l),
            AccountMsg::FileWant { conversation, want } => {
                self.on_file_want(from, &conversation, &want);
            }
            AccountMsg::FilePart(p) => self.on_file_part(from, &p),
            AccountMsg::FileMissing(m) => self.on_file_missing(from, &m),
            AccountMsg::Ticks {
                conversation,
                ticks,
            } => self.on_ticks(from, &conversation, &ticks),
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
                    // Conversations of contacts the sync deletes go too.
                    // (The contacts lock must be released before
                    // conversation_for, which takes it again.)
                    let keys: Vec<(PublicIdentity, u64)> = lock(&self.shared.contacts)
                        .iter()
                        .map(|c| (c.key, c.cleared_ms))
                        .collect();
                    let cleared_before: std::collections::HashMap<PublicIdentity, u64> =
                        keys.iter().copied().collect();
                    // (approved, blocked) before the merge.
                    let trust_before: std::collections::HashMap<PublicIdentity, (bool, bool)> =
                        lock(&self.shared.contacts)
                            .iter()
                            .map(|c| (c.key, (c.local_approved, c.blocked)))
                            .collect();
                    let keys: Vec<PublicIdentity> = keys.into_iter().map(|(k, _)| k).collect();
                    let before: Vec<(PublicIdentity, ConversationId)> = keys
                        .into_iter()
                        .map(|k| (k, self.conversation_for(&k)))
                        .collect();
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
                    // Whatever the merge took mutual approval from loses its
                    // tunnel (most also go through `withdraw_approval` below).
                    if changed {
                        self.shared.prune_tunnels();
                    }
                    let left: Vec<(PublicIdentity, ConversationId)> = {
                        let c = lock(&self.shared.contacts);
                        before
                            .iter()
                            .filter(|(k, _)| c.get(k).is_none() && c.forgotten_at(k).is_some())
                            .copied()
                            .collect()
                    };
                    for (k, conv) in left {
                        self.withdraw_approval(&k);
                        self.disconnect(&k);
                        self.forget_peer(&k);
                        let _ = self.shared.home.delete_history(self.identity_ref(), conv);
                    }
                    // Contacts revoked or blocked on our other device lose
                    // their tunnel here too; blocked ones (including ones
                    // we first hear of already blocked) also lose their
                    // sessions and conversation, as on that device.
                    let withdrawn: Vec<(PublicIdentity, bool)> = lock(&self.shared.contacts)
                        .iter()
                        .filter_map(|c| {
                            let (approved, blocked) =
                                trust_before.get(&c.key).copied().unwrap_or((false, false));
                            let newly_blocked = c.blocked && !blocked;
                            (newly_blocked || (approved && !c.local_approved))
                                .then_some((c.key, newly_blocked))
                        })
                        .collect();
                    if !withdrawn.is_empty() {
                        for (k, newly_blocked) in withdrawn {
                            self.withdraw_approval(&k);
                            if newly_blocked {
                                let conv = before
                                    .iter()
                                    .find(|(b, _)| *b == k)
                                    .map_or_else(|| self.conversation_for(&k), |(_, c)| *c);
                                self.disconnect(&k);
                                self.forget_peer(&k);
                                let _ = self.shared.home.delete_history(self.identity_ref(), conv);
                            }
                        }
                        self.enforce_policy();
                    }
                    // Chats cleared on our other device.
                    let cleared: Vec<(PublicIdentity, u64)> = lock(&self.shared.contacts)
                        .iter()
                        .filter(|c| c.cleared_ms > cleared_before.get(&c.key).copied().unwrap_or(0))
                        .map(|c| (c.key, c.cleared_ms))
                        .collect();
                    for (k, t) in cleared {
                        self.clear_before(self.conversation_for(&k), t);
                    }
                    if changed {
                        self.emit(Event::ContactsSynced { from });
                    }
                }
            }
        }
        true
    }

    /// Whether to take `from`'s chain `b` now. Anyone can present a chain,
    /// and a stored one stays (with every device in it a contact), so only
    /// chains from devices the user accepted, or about our own account or
    /// one we already know (a contact's new device), are checked and kept.
    /// A stranger's waits until the user accepts it; until then it costs
    /// neither checking nor storage.
    fn chain_wanted(&self, from: &PublicIdentity, b: &[u8]) -> bool {
        if self.is_accepted(from) {
            return true;
        }
        // Malformed: decoding will fail at once anyway.
        let Ok(id) = AccountChain::claimed_id(b) else {
            return true;
        };
        id == lock(&self.shared.account).id() || lock(&self.shared.accounts).get(&id).is_some()
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
        drop(contacts);
        // History recorded per device before the account was known joins
        // the account's conversation.
        for (d, _) in &chain.state().devices {
            let _ = self.shared.home.merge_history(
                self.identity_ref(),
                ConversationId::Peer(*d.as_bytes()),
                ConversationId::Peer(id.0),
                now_ms(),
            );
        }
    }

    /// A device left its account: stop trusting it everywhere, and seal
    /// nothing more to it (its prekeys go).
    fn revoke_device(&self, device: &PublicIdentity) {
        {
            let mut contacts = lock(&self.shared.contacts);
            if let Some(c) = contacts.get_mut(device) {
                c.local_approved = false;
                c.remote_approved = false;
                c.clear_discovery_keys();
                c.approval_changed_ms = now_ms();
            }
            self.shared.save_contacts(&contacts);
        }
        {
            let mut bundles = lock(&self.shared.bundles);
            if bundles.remove(device) {
                self.shared.persist_bundles(&bundles);
            }
        }
        // It may have held a tunnel with us: no longer.
        self.shared.prune_tunnels();
        self.disconnect(device);
        self.forget_peer(device);
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
            // Devices linked through another of ours: the app tells the
            // user here too, as the linking device does (`DeviceLinked`).
            let me = self.identity();
            let added: Vec<PublicIdentity> = chain
                .state()
                .devices
                .iter()
                .map(|(d, _)| *d)
                .filter(|d| *d != me && !own.state().has(d))
                .collect();
            *lock(&self.shared.account) = chain.clone();
            self.persist_account();
            self.tag_own_devices();
            if removed.contains(&self.identity()) {
                self.emit(Event::ThisDeviceRemoved);
                // Contacts no longer take this device as us.
                self.fail_all_scheduled(crate::schedule::Failure::Removed);
            }
            for d in &removed {
                self.revoke_device(d);
            }
            self.emit(Event::AccountChanged {
                account: chain.id(),
                added,
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
        // Siblings need our shared bundle to forward to their contacts.
        if let Some(b) = self.shared_bundle()
            && let Ok(m) = AccountMsg::SiblingBundle(b).encode()
        {
            for s in self.sessions() {
                if self.is_own_device(&s.peer) {
                    let _ = self.send(&s.peer, AppMessage::Account(m.clone()));
                }
            }
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

    /// Whether a link code we issued is still out (see `Node::admit`).
    pub(crate) fn link_code_out(&self) -> bool {
        !self.shared.persona
            && lock(&self.shared.linking)
                .codes
                .iter()
                .any(|(_, t)| t.elapsed() < LINK_CODE_TTL)
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
        if matched.is_none() || self.is_revoked(&from) || self.shared.persona {
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
        if self.shared.persona {
            return Err(NetError::NotAllowed(
                "an anonymous identity has no other devices".into(),
            ));
        }
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
                    Action::Remove { .. } | Action::Rename { .. } => {
                        return Err(NetError::Refused("not an add".into()));
                    }
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
        if self.shared.persona {
            return Err(NetError::NotAllowed(
                "an anonymous identity has no other devices".into(),
            ));
        }
        {
            let mut own = lock(&self.shared.account);
            let state = own.state();
            if !state.has(device) {
                return Err(NetError::NotAllowed("not a device of this account".into()));
            }
            if state.devices.len() < 2 {
                return Err(NetError::NotAllowed(
                    "it's the account's only device".into(),
                ));
            }
            let mut link = own.propose(Action::Remove { device: *device });
            link.sign(self.identity_ref())?;
            own.append(link)?;
        }
        self.persist_account();
        self.broadcast_chain();
        if *device == self.identity() {
            self.emit(Event::ThisDeviceRemoved);
            self.fail_all_scheduled(crate::schedule::Failure::Removed);
        } else {
            self.revoke_device(device);
            // As our other devices see it when the chain reaches them, so
            // the groups this one runs drop the device too.
            self.emit(Event::AccountChanged {
                account: self.account().id(),
                added: vec![],
                removed: vec![*device],
            });
        }
        Ok(())
    }

    /// Renames a device of our account (any of them, including this one)
    /// and tells every peer; contacts see the new name too.
    pub fn rename_device(&self, device: &PublicIdentity, name: &str) -> Result<()> {
        if self.shared.persona {
            return Err(NetError::NotAllowed(
                "an anonymous identity has no other devices".into(),
            ));
        }
        let name = threnody_core::account::clean_name(name.trim());
        if name.is_empty() {
            return Err(NetError::Refused("empty device name".into()));
        }
        {
            let mut own = lock(&self.shared.account);
            if !own.state().has(device) {
                return Err(NetError::Refused("not a device of this account".into()));
            }
            let mut link = own.propose(Action::Rename {
                device: *device,
                name,
            });
            link.sign(self.identity_ref())?;
            own.append(link)?;
        }
        self.persist_account();
        self.broadcast_chain();
        self.emit(Event::AccountChanged {
            account: self.account().id(),
            added: vec![],
            removed: vec![],
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{AcceptPolicy, NodeConfig};
    use threnody_core::Identity;
    use threnody_core::account::Action;
    use threnody_core::store::Home;

    #[test]
    fn device_sync_ops_round_trip() {
        for m in [
            AccountMsg::FileWant {
                conversation: vec![0; 33],
                want: vec![1, 2],
            },
            AccountMsg::FilePart(vec![3; 1000]),
            AccountMsg::FileMissing(vec![4]),
            AccountMsg::Ticks {
                conversation: vec![1; 17],
                ticks: vec![5, 6],
            },
        ] {
            assert_eq!(AccountMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        let unknown = cbor::to_vec(16, |e| {
            e.map_len(1)?.u8(0)?.u8(99)?;
            Ok(())
        })
        .unwrap();
        assert!(AccountMsg::decode(&unknown).is_err());
    }

    fn add(chain: &mut AccountChain, by: &Identity, new: &Identity) {
        let mut link = chain.propose(Action::Add {
            device: new.public(),
            name: "device".into(),
        });
        link.sign(by).unwrap();
        link.sign(new).unwrap();
        chain.append(link).unwrap();
    }

    /// A stranger's chain is neither stored nor turned into contacts until
    /// the user accepts the stranger; a known account's new device is
    /// recognised at once.
    #[tokio::test]
    async fn a_strangers_chain_waits_until_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("n"));
        let identity = home.create_identity(None).unwrap();
        let (node, _rx) = Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap();
        let stranger = Identity::generate();
        let genesis = Identity::generate();
        let mut chain = AccountChain::genesis(&genesis, "genesis").unwrap();
        add(&mut chain, &genesis, &stranger);
        let fakes: Vec<Identity> = (0..20).map(|_| Identity::generate()).collect();
        for f in &fakes {
            add(&mut chain, &stranger, f);
        }
        let msg = AccountMsg::Chain(chain.encode().unwrap()).encode().unwrap();
        let contacts = || node.contacts().iter().count();
        let before = contacts();

        assert!(!node.on_account(stranger.public(), &msg), "set aside");
        assert!(node.known_accounts().get(&chain.id()).is_none());
        assert_eq!(contacts(), before, "no contacts from a stranger's chain");

        node.accept_contact(&stranger.public());
        assert!(node.on_account(stranger.public(), &msg));
        assert!(node.known_accounts().get(&chain.id()).is_some());
        assert!(node.contacts().get(&fakes[0].public()).is_some());

        // Another device of that account, not accepted itself, is taken.
        let other = Identity::generate();
        add(&mut chain, &stranger, &other);
        let msg = AccountMsg::Chain(chain.encode().unwrap()).encode().unwrap();
        assert!(node.on_account(other.public(), &msg));
        assert!(node.account_of(&other.public()).is_some());
        node.shutdown();
    }
}
