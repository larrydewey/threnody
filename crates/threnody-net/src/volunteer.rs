//! Volunteer relays and relay directories (Appendix P).
//!
//! Three roles, any of which a node can take:
//!
//! - **Client.** Subscribes to directories by link, fetches their signed
//!   relay lists over anonymous links, and keeps a wallet of anonymous
//!   relay-access credentials (Appendix O) from them. Onion circuits then
//!   go through volunteer relays when contacts can't make a path (or to
//!   hide a persona's address), paying each volunteer hop with a token.
//! - **Volunteer relay.** Registers a descriptor with its directories and
//!   carries circuits for anyone presenting a valid token.
//! - **Directory.** Lists relays that registered and proved reachable,
//!   signs documents, and issues tokens, a few per network per day.
//!
//! Every exchange runs over anonymous links (`anon`): a client shows a
//! directory or relay an address, never an identity.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map};
use threnody_core::credential::{
    self, Credential, Issued, Issuer, IssuerKey, RELAY_SCHEMA, RELAY_SLOTS, RelayToken, Request,
};
use threnody_core::crypto::random_bytes;
use threnody_core::directory::{
    DirMsg, DirectoryDoc, DirectoryLink, RegisterStatus, RelayDescriptor, select,
};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity, now_ms};
use tokio::sync::oneshot;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// How often subscriptions are refreshed (documents and tokens).
const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);
/// How often a volunteer relay registers again.
const REGISTER_EVERY: Duration = Duration::from_secs(6 * 3600);
/// A descriptor's lifetime: long enough to survive a missed registration.
const DESCRIPTOR_LIFE_MS: u64 = 24 * 3_600_000;
/// A directory re-signs its document this often, valid this long.
const DOCUMENT_EVERY_MS: u64 = 30 * 60_000;
const DOCUMENT_LIFE_MS: u64 = 6 * 3_600_000;
/// Relay-access credentials a directory issues per network per day.
pub const TOKENS_PER_NETWORK: u32 = 8;
/// Registrations a directory accepts per network per day.
const REGISTRATIONS_PER_NETWORK: u32 = 8;
/// Directory requests wait this long (cover traffic slows every message).
const CALL_TIMEOUT: Duration = Duration::from_secs(40);
const STATE_SUBS: &str = "directories";
const STATE_WALLET: &str = "relay-wallet";
const STATE_SERVER: &str = "directory-server";

struct Subscription {
    link: DirectoryLink,
    key: IssuerKey,
    doc: Option<DirectoryDoc>,
}

/// A subscribed directory, as apps show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryInfo {
    pub link: String,
    pub issuer: [u8; 32],
    pub identity: Fingerprint,
    /// Relays its current document lists (0 without one).
    pub relays: usize,
    pub published_ms: Option<u64>,
    pub valid_until_ms: Option<u64>,
    /// Relay-access credentials held from it for today and later.
    pub tokens: usize,
}

/// A relay listed with (or waiting at) the directory this node runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedRelay {
    pub identity: PublicIdentity,
    pub addrs: Vec<String>,
    pub listed: bool,
    pub expires_ms: u64,
}

#[derive(Default)]
struct Wallet {
    /// `(directory, epoch day)` -> credential.
    creds: Vec<([u8; 32], u32, Credential)>,
    /// `(relay, epoch)` -> slots used (bit i = slot i).
    used: HashMap<(Fingerprint, u32), u64>,
}

struct DirServer {
    issuer: Issuer,
    review: bool,
    relays: HashMap<PublicIdentity, (RelayDescriptor, bool)>,
    /// `(epoch, network)` -> tokens or registrations issued.
    issued: HashMap<(u32, [u8; 16]), u32>,
    registered: HashMap<(u32, [u8; 16]), u32>,
    doc: Option<DirectoryDoc>,
    dirty: bool,
}

pub(crate) struct VolunteerState {
    subs: Mutex<Vec<Subscription>>,
    wallet: Mutex<Wallet>,
    pending: Mutex<HashMap<u64, (PublicIdentity, oneshot::Sender<DirMsg>)>>,
    /// Our relay descriptor's addresses while volunteering.
    relay: Mutex<Option<Vec<String>>>,
    /// Token pseudonyms seen, by epoch (each opens one circuit).
    seen: Mutex<HashMap<u32, HashSet<[u8; credential::PSEUDONYM_LEN]>>>,
    server: Mutex<Option<DirServer>>,
    /// Route through volunteer relays when contacts can't (on by default).
    use_volunteers: AtomicBool,
    /// How many subscribed directories must list a relay (0 = automatic).
    threshold: AtomicUsize,
    upkeep: AtomicBool,
    /// Tests only: how to (mis)pay relays.
    test_pay: Mutex<Option<TestPay>>,
}

/// Ways a test client can misbehave toward volunteer relays.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestPay {
    /// Send no tokens.
    Unpaid,
    /// Always use this slot (a replay after the first circuit).
    Slot(u16),
}

impl VolunteerState {
    pub(crate) fn new() -> Self {
        Self {
            subs: Mutex::new(Vec::new()),
            wallet: Mutex::new(Wallet::default()),
            pending: Mutex::new(HashMap::new()),
            relay: Mutex::new(None),
            seen: Mutex::new(HashMap::new()),
            server: Mutex::new(None),
            use_volunteers: AtomicBool::new(true),
            threshold: AtomicUsize::new(0),
            upkeep: AtomicBool::new(false),
            test_pay: Mutex::new(None),
        }
    }
}

/// The part of an address that limits issuance: a /24 or a /56.
fn network(ip: IpAddr) -> [u8; 16] {
    let mut out = [0u8; 16];
    match ip {
        IpAddr::V4(v4) => out[..3].copy_from_slice(&v4.octets()[..3]),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => out[..3].copy_from_slice(&v4.octets()[..3]),
            None => {
                out[0] = 6;
                out[1..8].copy_from_slice(&v6.octets()[..7]);
            }
        },
    }
    out
}

fn new_id() -> u64 {
    u64::from_le_bytes(random_bytes())
}

impl Node {
    fn vol(&self) -> &VolunteerState {
        &self.shared.volunteer
    }

    // ----- Loading and saving -----

    /// Loads subscriptions, the wallet and any directory this node runs.
    pub(crate) fn load_volunteer(&self) {
        let home = &self.shared.home;
        let id = self.identity_ref();
        if let Ok(Some(b)) = home.load_state(id, STATE_SUBS) {
            *lock(&self.vol().subs) = decode_subs(&b).unwrap_or_default();
        }
        if let Ok(Some(b)) = home.load_state(id, STATE_WALLET) {
            *lock(&self.vol().wallet) = decode_wallet(&b).unwrap_or_default();
        }
        if let Ok(Some(b)) = home.load_state(id, STATE_SERVER)
            && let Ok(issuer) = Issuer::new(id, 0)
            && let Ok((review, relays)) = decode_server(&b)
        {
            *lock(&self.vol().server) = Some(DirServer {
                issuer,
                review,
                relays,
                issued: HashMap::new(),
                registered: HashMap::new(),
                doc: None,
                dirty: true,
            });
        }
    }

    fn save_subs(&self) {
        let bytes = encode_subs(&lock(&self.vol().subs));
        self.shared.save_state(STATE_SUBS, bytes);
    }

    fn save_wallet(&self) {
        let bytes = encode_wallet(&lock(&self.vol().wallet));
        self.shared.save_state(STATE_WALLET, bytes);
    }

    fn save_server(&self) {
        let bytes = lock(&self.vol().server)
            .as_ref()
            .map(|s| encode_server(s.review, &s.relays));
        if let Some(b) = bytes {
            self.shared.save_state(STATE_SERVER, b);
        }
    }

    fn note(&self, note: impl Into<String>) {
        self.emit(Event::VolunteerNote { note: note.into() });
    }

    // ----- Settings -----

    pub fn use_volunteers(&self) -> bool {
        self.vol().use_volunteers.load(Ordering::Relaxed)
    }

    /// Whether onion circuits may go through volunteer relays from the
    /// subscribed directories (on by default).
    pub fn set_use_volunteers(&self, on: bool) {
        self.vol().use_volunteers.store(on, Ordering::Relaxed);
    }

    /// How many subscribed directories must list a relay before it is used
    /// (0: one with a single subscription, else two).
    pub fn set_directory_threshold(&self, k: usize) {
        self.vol().threshold.store(k, Ordering::Relaxed);
    }

    fn threshold(&self, subs: usize) -> usize {
        match self.vol().threshold.load(Ordering::Relaxed) {
            0 => subs.min(2),
            k => k,
        }
    }

    // ----- Client: subscriptions -----

    /// Subscribes to a directory: checks that the node at the link's
    /// address holds the issuer key the link pins, and fetches its relays.
    pub async fn subscribe_directory(&self, link: &str) -> Result<DirectoryInfo> {
        let link: DirectoryLink = link.parse()?;
        let peer = self.anon_dial(&link.addr, None, true).await?;
        let key = match self
            .dir_call(&peer, DirMsg::GetKey { id: new_id() })
            .await?
        {
            DirMsg::Key { key, .. } => IssuerKey::decode(&key)?,
            DirMsg::Refused { reason, .. } => return Err(NetError::Refused(reason)),
            _ => return Err(NetError::Closed),
        };
        if key.id() != link.issuer || key.identity != peer {
            return Err(NetError::IdentityMismatch {
                expected: threnody_core::directory::short_id(&link.issuer),
                got: threnody_core::directory::short_id(&key.id()),
            });
        }
        let doc = match self.fetch_document(&peer, &key).await {
            Ok(d) => Some(d),
            Err(e) => {
                self.note(format!(
                    "directory {}: no relay list yet ({e}); trying again",
                    threnody_core::directory::short_id(&link.issuer)
                ));
                let again = async {
                    let peer = self
                        .anon_dial(&link.addr, Some(key.identity.fingerprint()), true)
                        .await?;
                    self.fetch_document(&peer, &key).await
                };
                match again.await {
                    Ok(d) => Some(d),
                    Err(e) => {
                        self.note(format!(
                            "directory {}: {e}; /dir refresh retries",
                            threnody_core::directory::short_id(&link.issuer)
                        ));
                        None
                    }
                }
            }
        };
        {
            let mut subs = lock(&self.vol().subs);
            subs.retain(|s| s.link.issuer != link.issuer);
            subs.push(Subscription {
                link: link.clone(),
                key,
                doc,
            });
        }
        self.save_subs();
        let _ = self.fetch_tokens(&link.issuer).await;
        self.start_directory_upkeep();
        // A volunteer registers with a new directory at once.
        if self.volunteering() {
            let node = self.clone();
            tokio::spawn(async move { node.register_relay().await });
        }
        self.directories()
            .into_iter()
            .find(|d| d.issuer == link.issuer)
            .ok_or(NetError::Closed)
    }

    /// Forgets a directory and its tokens.
    pub fn unsubscribe_directory(&self, issuer: &[u8; 32]) -> bool {
        let removed = {
            let mut subs = lock(&self.vol().subs);
            let before = subs.len();
            subs.retain(|s| s.link.issuer != *issuer);
            subs.len() != before
        };
        if removed {
            lock(&self.vol().wallet)
                .creds
                .retain(|(d, _, _)| d != issuer);
            self.save_subs();
            self.save_wallet();
        }
        removed
    }

    pub fn directories(&self) -> Vec<DirectoryInfo> {
        let today = credential::day(now_ms());
        let wallet = lock(&self.vol().wallet);
        lock(&self.vol().subs)
            .iter()
            .map(|s| DirectoryInfo {
                link: s.link.to_string(),
                issuer: s.link.issuer,
                identity: s.key.identity.fingerprint(),
                relays: s.doc.as_ref().map_or(0, |d| d.relays.len()),
                published_ms: s.doc.as_ref().map(|d| d.published_ms),
                valid_until_ms: s.doc.as_ref().map(|d| d.valid_until_ms),
                tokens: wallet
                    .creds
                    .iter()
                    .filter(|(d, e, _)| *d == s.link.issuer && *e >= today)
                    .count(),
            })
            .collect()
    }

    /// The volunteer relays usable now: live, and listed by enough of the
    /// subscribed directories.
    pub fn volunteer_relays(&self) -> Vec<RelayDescriptor> {
        let now = now_ms();
        let subs = lock(&self.vol().subs);
        let docs: Vec<DirectoryDoc> = subs
            .iter()
            .filter_map(|s| s.doc.clone())
            .filter(|d| d.valid_until_ms > now)
            .collect();
        let k = self.threshold(subs.len());
        let me = self.identity();
        select(&docs, k, now)
            .into_iter()
            .filter(|r| r.identity != me)
            .collect()
    }

    async fn fetch_document(&self, peer: &PublicIdentity, key: &IssuerKey) -> Result<DirectoryDoc> {
        match self
            .dir_call(peer, DirMsg::GetDocument { id: new_id() })
            .await?
        {
            DirMsg::Document { doc, .. } => {
                let doc = DirectoryDoc::decode(&doc)?;
                doc.verify(key, now_ms())?;
                Ok(doc)
            }
            DirMsg::Refused { reason, .. } => Err(NetError::Refused(reason)),
            _ => Err(NetError::Closed),
        }
    }

    /// Gets relay-access credentials for today and tomorrow from one
    /// directory, unless the wallet has them already.
    async fn fetch_tokens(&self, issuer: &[u8; 32]) -> Result<usize> {
        let (link, key) = {
            let subs = lock(&self.vol().subs);
            let s = subs
                .iter()
                .find(|s| s.link.issuer == *issuer)
                .ok_or(NetError::Closed)?;
            (s.link.clone(), s.key.clone())
        };
        let today = credential::day(now_ms());
        let mut got = 0;
        for epoch in [today, today + 1] {
            let have = lock(&self.vol().wallet)
                .creds
                .iter()
                .any(|(d, e, _)| d == issuer && *e == epoch);
            if have {
                continue;
            }
            let peer = self
                .anon_dial(&link.addr, Some(key.identity.fingerprint()), true)
                .await?;
            let req = Request::new()?;
            let ask = DirMsg::TokenRequest {
                id: new_id(),
                epoch,
                commitment: req.commitment.clone(),
            };
            match self.dir_call(&peer, ask).await? {
                DirMsg::Token { issued, .. } => {
                    let issued = Issued::decode(&issued)?;
                    let cred = req.finish(&key, &issued)?;
                    if cred.header.schema != RELAY_SCHEMA || cred.header.expires_day != epoch {
                        return Err(NetError::Protocol(threnody_core::Error::Malformed(
                            "directory issued the wrong credential",
                        )));
                    }
                    lock(&self.vol().wallet).creds.push((*issuer, epoch, cred));
                    got += 1;
                }
                DirMsg::Refused { reason, .. } => return Err(NetError::Refused(reason)),
                _ => return Err(NetError::Closed),
            }
        }
        if got > 0 {
            self.save_wallet();
        }
        Ok(got)
    }

    /// Refreshes every subscription: a new document, and tokens. Also
    /// drops credentials that can't be used any more.
    pub async fn refresh_directories(&self) {
        let subs: Vec<(DirectoryLink, IssuerKey)> = lock(&self.vol().subs)
            .iter()
            .map(|s| (s.link.clone(), s.key.clone()))
            .collect();
        for (link, key) in subs {
            let fetched = async {
                let peer = self
                    .anon_dial(&link.addr, Some(key.identity.fingerprint()), true)
                    .await?;
                self.fetch_document(&peer, &key).await
            }
            .await;
            match fetched {
                Ok(doc) => {
                    if let Some(s) = lock(&self.vol().subs)
                        .iter_mut()
                        .find(|s| s.link.issuer == link.issuer)
                    {
                        s.doc = Some(doc);
                    }
                    self.save_subs();
                }
                Err(e) => self.note(format!(
                    "directory {}: {e}",
                    threnody_core::directory::short_id(&link.issuer)
                )),
            }
            if self.use_volunteers()
                && let Err(e) = self.fetch_tokens(&link.issuer).await
            {
                self.note(format!(
                    "directory {} tokens: {e}",
                    threnody_core::directory::short_id(&link.issuer)
                ));
            }
        }
        let today = credential::day(now_ms());
        {
            let mut w = lock(&self.vol().wallet);
            w.creds.retain(|(_, e, _)| *e + 1 >= today);
            w.used.retain(|(_, e), _| *e + 1 >= today);
        }
        self.save_wallet();
    }

    /// Starts the background refresh (and, while volunteering,
    /// registration) loop, once.
    pub(crate) fn start_directory_upkeep(&self) {
        if self.vol().upkeep.swap(true, Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.vol().upkeep.store(false, Ordering::Relaxed);
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            let mut refresh = tokio::time::interval(REFRESH_EVERY);
            let mut register = tokio::time::interval(REGISTER_EVERY);
            loop {
                tokio::select! {
                    _ = refresh.tick() => {
                        if !lock(&node.vol().subs).is_empty() {
                            node.refresh_directories().await;
                        }
                    }
                    _ = register.tick() => node.register_relay().await,
                    () = &mut closed => break,
                }
            }
        });
    }

    // ----- Client: tokens for circuits -----

    /// A token for one circuit through volunteer `relay`, bound to the
    /// hop's ephemeral key: from a credential of a directory that lists
    /// the relay, in a slot not used before.
    pub(crate) fn mint_token(&self, relay: &PublicIdentity, e_pub: &[u8]) -> Option<Vec<u8>> {
        let listing: Vec<[u8; 32]> = lock(&self.vol().subs)
            .iter()
            .filter(|s| {
                s.doc
                    .as_ref()
                    .is_some_and(|d| d.relays.iter().any(|r| r.identity == *relay))
            })
            .map(|s| s.link.issuer)
            .collect();
        let today = credential::day(now_ms());
        let test = *lock(&self.vol().test_pay);
        let token = {
            let mut w = lock(&self.vol().wallet);
            let w = &mut *w;
            let (_, epoch, cred) = w
                .creds
                .iter()
                .find(|(d, e, _)| listing.contains(d) && *e == today)?;
            let used = w.used.entry((relay.fingerprint(), *epoch)).or_insert(0);
            let slot = match test {
                Some(TestPay::Slot(s)) => s,
                _ => (0..RELAY_SLOTS).find(|s| *used & (1 << s) == 0)?,
            };
            *used |= 1 << slot;
            if test == Some(TestPay::Unpaid) {
                // A token that pays nothing: issued for another relay.
                let other = threnody_core::Identity::generate().public();
                return RelayToken::new(cred, &other, *epoch, slot, e_pub)
                    .ok()?
                    .encode()
                    .ok();
            }
            RelayToken::new(cred, relay, *epoch, slot, e_pub)
                .ok()?
                .encode()
                .ok()?
        };
        self.save_wallet();
        Some(token)
    }

    /// Tests only: makes this client pay relays wrongly.
    #[doc(hidden)]
    pub fn set_test_pay(&self, pay: Option<TestPay>) {
        *lock(&self.vol().test_pay) = pay;
    }

    /// Marks every slot for `relay` today as used (for tests).
    #[doc(hidden)]
    pub fn exhaust_tokens(&self, relay: &PublicIdentity) {
        let today = credential::day(now_ms());
        lock(&self.vol().wallet)
            .used
            .insert((relay.fingerprint(), today), u64::MAX);
    }

    /// Whether a circuit through `relay` can be paid for.
    pub(crate) fn can_pay(&self, relay: &PublicIdentity) -> bool {
        let today = credential::day(now_ms());
        let listing: Vec<[u8; 32]> = lock(&self.vol().subs)
            .iter()
            .filter(|s| {
                s.doc
                    .as_ref()
                    .is_some_and(|d| d.relays.iter().any(|r| r.identity == *relay))
            })
            .map(|s| s.link.issuer)
            .collect();
        let w = lock(&self.vol().wallet);
        let used = w
            .used
            .get(&(relay.fingerprint(), today))
            .copied()
            .unwrap_or(0);
        used.count_ones() < u32::from(RELAY_SLOTS)
            && w.creds
                .iter()
                .any(|(d, e, _)| listing.contains(d) && *e == today)
    }

    // ----- Volunteer relay -----

    pub fn volunteering(&self) -> bool {
        lock(&self.vol().relay).is_some()
    }

    /// Volunteers as a relay reachable at `addrs` (public `host:port`s), or
    /// stops (`None`). Registers with every subscribed directory now and
    /// every few hours. Tokens are accepted from those directories.
    pub fn set_volunteer(&self, addrs: Option<Vec<String>>) -> Result<()> {
        if addrs.is_some() && self.shared.persona {
            return Err(NetError::NotAllowed(
                "an anonymous identity can't volunteer: its descriptor would publish it with this address".into(),
            ));
        }
        let on = addrs.is_some();
        *lock(&self.vol().relay) = addrs;
        if on {
            self.start_directory_upkeep();
            let node = self.clone();
            if let Ok(h) = tokio::runtime::Handle::try_current() {
                h.spawn(async move { node.register_relay().await });
            }
        }
        Ok(())
    }

    async fn register_relay(&self) {
        let Some(addrs) = lock(&self.vol().relay).clone() else {
            return;
        };
        let desc =
            match RelayDescriptor::new(self.identity_ref(), addrs, now_ms(), DESCRIPTOR_LIFE_MS)
                .and_then(|d| d.encode())
            {
                Ok(d) => d,
                Err(e) => return self.note(format!("relay descriptor: {e}")),
            };
        let subs: Vec<(DirectoryLink, IssuerKey)> = lock(&self.vol().subs)
            .iter()
            .map(|s| (s.link.clone(), s.key.clone()))
            .collect();
        for (link, key) in subs {
            let short = threnody_core::directory::short_id(&link.issuer);
            let result = async {
                // As ourselves: the descriptor names us anyway.
                let peer = self
                    .anon_dial(&link.addr, Some(key.identity.fingerprint()), false)
                    .await?;
                self.dir_call(
                    &peer,
                    DirMsg::Register {
                        id: new_id(),
                        descriptor: desc.clone(),
                    },
                )
                .await
            }
            .await;
            match result {
                Ok(DirMsg::Registered { status, .. }) => self.note(match status {
                    RegisterStatus::Listed => {
                        format!("listed as a volunteer relay by directory {short}")
                    }
                    RegisterStatus::Pending => {
                        format!("directory {short} will review this relay before listing it")
                    }
                }),
                Ok(DirMsg::Refused { reason, .. }) => {
                    self.note(format!("directory {short} refused this relay: {reason}"))
                }
                Ok(_) => {}
                Err(e) => self.note(format!("registering with directory {short}: {e}")),
            }
        }
    }

    /// Checks a token presented for a circuit through us: from one of our
    /// directories, for us and this circuit, and not used before.
    pub(crate) fn accept_token(&self, token: &[u8], e_pub: &[u8]) -> bool {
        if !self.volunteering() {
            return false;
        }
        let Ok(token) = RelayToken::decode(token) else {
            return false;
        };
        let keys: Vec<IssuerKey> = lock(&self.vol().subs)
            .iter()
            .map(|s| s.key.clone())
            .collect();
        let now = now_ms();
        let Ok(nym) = token.verify(&keys, &self.identity(), e_pub, now) else {
            return false;
        };
        let mut seen = lock(&self.vol().seen);
        let today = credential::day(now);
        seen.retain(|e, _| *e + 1 >= today);
        seen.entry(token.epoch).or_default().insert(nym)
    }

    // ----- Directory server -----

    /// Runs a directory on this node's identity: lists relays that register
    /// and prove reachable (after the operator's review when `review`),
    /// and issues relay tokens. Returns the link to share, for `addr`.
    pub fn serve_directory(&self, addr: &str, review: bool) -> Result<String> {
        if self.shared.persona {
            return Err(NetError::NotAllowed(
                "an anonymous identity can't run a directory".into(),
            ));
        }
        let issuer = Issuer::new(self.identity_ref(), 0)?;
        let link = DirectoryLink {
            issuer: issuer.key().id(),
            addr: addr.to_owned(),
        };
        {
            let mut server = lock(&self.vol().server);
            match server.as_mut() {
                Some(s) => s.review = review,
                None => {
                    *server = Some(DirServer {
                        issuer,
                        review,
                        relays: HashMap::new(),
                        issued: HashMap::new(),
                        registered: HashMap::new(),
                        doc: None,
                        dirty: true,
                    })
                }
            }
        }
        self.save_server();
        Ok(link.to_string())
    }

    pub fn serving_directory(&self) -> bool {
        lock(&self.vol().server).is_some()
    }

    /// The relays registered with our directory.
    pub fn directory_relays(&self) -> Vec<ListedRelay> {
        lock(&self.vol().server)
            .as_ref()
            .map_or_else(Vec::new, |s| {
                s.relays
                    .values()
                    .map(|(d, listed)| ListedRelay {
                        identity: d.identity,
                        addrs: d.addrs.clone(),
                        listed: *listed,
                        expires_ms: d.expires_ms,
                    })
                    .collect()
            })
    }

    /// Lists (`true`) or unlists a registered relay.
    pub fn set_relay_listed(&self, relay: &Fingerprint, listed: bool) -> bool {
        let changed = lock(&self.vol().server).as_mut().is_some_and(|s| {
            s.relays
                .iter_mut()
                .find(|(id, _)| id.fingerprint() == *relay)
                .map(|(_, entry)| {
                    entry.1 = listed;
                    s.dirty = true;
                })
                .is_some()
        });
        if changed {
            self.save_server();
        }
        changed
    }

    /// The current signed document, re-signed when stale or changed.
    fn current_document(&self) -> Option<Vec<u8>> {
        let now = now_ms();
        let mut server = lock(&self.vol().server);
        let s = server.as_mut()?;
        s.relays.retain(|_, (d, _)| d.expires_ms > now);
        let stale = s
            .doc
            .as_ref()
            .is_none_or(|d| now.saturating_sub(d.published_ms) > DOCUMENT_EVERY_MS);
        if s.dirty || stale {
            let listed: Vec<RelayDescriptor> = s
                .relays
                .values()
                .filter(|(_, l)| *l)
                .map(|(d, _)| d.clone())
                .collect();
            s.doc = DirectoryDoc::sign(&s.issuer, &listed, now, DOCUMENT_LIFE_MS).ok();
            s.dirty = false;
        }
        s.doc.as_ref().and_then(|d| d.encode().ok())
    }

    fn dir_reply(&self, peer: &PublicIdentity, m: &DirMsg) {
        if let Ok(b) = m.encode() {
            self.anon_send(peer, AppMessage::Directory(b));
        }
    }

    /// Answers a directory request from `peer` (an anonymous link).
    fn serve_request(&self, peer: PublicIdentity, msg: DirMsg) {
        let id = msg.id();
        let refuse = |reason: &str| DirMsg::Refused {
            id,
            reason: reason.to_owned(),
        };
        if !self.serving_directory() {
            return self.dir_reply(&peer, &refuse("not a directory"));
        }
        let net = self.anon_addr(&peer).map_or([0; 16], |a| network(a.ip()));
        let today = credential::day(now_ms());
        let answer = match msg {
            DirMsg::GetKey { .. } => {
                let key = lock(&self.vol().server)
                    .as_ref()
                    .and_then(|s| s.issuer.key().encode().ok());
                key.map_or_else(|| refuse("no key"), |key| DirMsg::Key { id, key })
            }
            DirMsg::GetDocument { .. } => self
                .current_document()
                .map_or_else(|| refuse("no document"), |doc| DirMsg::Document { id, doc }),
            DirMsg::TokenRequest {
                epoch, commitment, ..
            } => {
                let mut server = lock(&self.vol().server);
                match server.as_mut() {
                    _ if epoch != today && epoch != today + 1 => {
                        refuse("tokens are for today or tomorrow")
                    }
                    Some(s) => {
                        let n = s.issued.entry((epoch, net)).or_insert(0);
                        if *n >= TOKENS_PER_NETWORK {
                            refuse("this network has had its tokens for that day")
                        } else {
                            match s
                                .issuer
                                .issue(&commitment, RELAY_SCHEMA, &Vec::new(), epoch)
                                .and_then(|i| i.encode())
                            {
                                Ok(issued) => {
                                    *n += 1;
                                    s.issued.retain(|(e, _), _| *e + 1 >= today);
                                    DirMsg::Token { id, issued }
                                }
                                Err(_) => refuse("bad request"),
                            }
                        }
                    }
                    None => refuse("not a directory"),
                }
            }
            DirMsg::Register { descriptor, .. } => {
                let allowed = lock(&self.vol().server).as_mut().is_some_and(|s| {
                    s.registered.retain(|(e, _), _| *e == today);
                    let n = s.registered.entry((today, net)).or_insert(0);
                    *n += 1;
                    *n <= REGISTRATIONS_PER_NETWORK
                });
                match RelayDescriptor::decode(&descriptor) {
                    _ if !allowed => refuse("too many registrations from this network"),
                    Err(_) => refuse("bad descriptor"),
                    Ok(d) if !d.live(now_ms()) => refuse("descriptor expired"),
                    Ok(d) => {
                        // Answered once the relay proves reachable.
                        let node = self.clone();
                        tokio::spawn(async move { node.probe_and_list(peer, id, d).await });
                        return;
                    }
                }
            }
            // Answers aren't for us to serve.
            _ => return,
        };
        self.dir_reply(&peer, &answer);
    }

    /// Lists a registering relay once one of its addresses answers with its
    /// identity.
    async fn probe_and_list(&self, peer: PublicIdentity, id: u64, d: RelayDescriptor) {
        let mut reachable = false;
        for addr in &d.addrs {
            if self
                .anon_dial(addr, Some(d.identity.fingerprint()), true)
                .await
                .is_ok()
            {
                reachable = true;
                break;
            }
        }
        let answer = if reachable {
            let status = {
                let mut server = lock(&self.vol().server);
                let Some(s) = server.as_mut() else { return };
                let listed = s.relays.get(&d.identity).is_some_and(|(_, l)| *l) || !s.review;
                s.relays.insert(d.identity, (d, listed));
                s.dirty = true;
                if listed {
                    RegisterStatus::Listed
                } else {
                    RegisterStatus::Pending
                }
            };
            self.save_server();
            DirMsg::Registered { id, status }
        } else {
            DirMsg::Refused {
                id,
                reason: "unreachable at the addresses given".into(),
            }
        };
        self.dir_reply(&peer, &answer);
    }

    // ----- Messages -----

    async fn dir_call(&self, peer: &PublicIdentity, msg: DirMsg) -> Result<DirMsg> {
        let id = msg.id();
        let (tx, rx) = oneshot::channel();
        lock(&self.vol().pending).insert(id, (*peer, tx));
        if !self.anon_send(peer, AppMessage::Directory(msg.encode()?)) {
            lock(&self.vol().pending).remove(&id);
            return Err(NetError::Closed);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(m)) => Ok(m),
            Ok(Err(_)) => Err(NetError::Closed),
            Err(_) => {
                lock(&self.vol().pending).remove(&id);
                Err(NetError::Timeout)
            }
        }
    }

    /// A directory message on an anonymous link: an answer we wait for, or
    /// a request for the directory we run.
    pub(crate) fn on_directory(&self, peer: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = DirMsg::decode(payload) else {
            return;
        };
        match msg {
            DirMsg::GetKey { .. }
            | DirMsg::GetDocument { .. }
            | DirMsg::TokenRequest { .. }
            | DirMsg::Register { .. } => {
                self.serve_request(peer, msg);
            }
            answer => {
                let mut pending = lock(&self.vol().pending);
                if pending.get(&answer.id()).is_some_and(|(p, _)| *p == peer)
                    && let Some((_, tx)) = pending.remove(&answer.id())
                {
                    let _ = tx.send(answer);
                }
            }
        }
    }

    pub(crate) fn dir_busy(&self, peer: &PublicIdentity) -> bool {
        lock(&self.vol().pending).values().any(|(p, _)| p == peer)
    }

    pub(crate) fn dir_link_closed(&self, peer: &PublicIdentity) {
        lock(&self.vol().pending).retain(|_, (p, _)| p != peer);
    }
}

// ----- Persistence -----

fn encode_subs(subs: &[Subscription]) -> threnody_core::Result<Vec<u8>> {
    let parts: Vec<(String, Vec<u8>, Option<Vec<u8>>)> = subs
        .iter()
        .map(|s| {
            Ok((
                s.link.to_string(),
                s.key.encode()?,
                s.doc.as_ref().map(DirectoryDoc::encode).transpose()?,
            ))
        })
        .collect::<threnody_core::Result<_>>()?;
    let size = 64
        + parts
            .iter()
            .map(|(l, k, d)| l.len() + k.len() + d.as_ref().map_or(0, Vec::len) + 16)
            .sum::<usize>();
    cbor::to_vec(size, |e| {
        e.array_len(parts.len())?;
        for (l, k, d) in &parts {
            e.array_len(3)?.str(l)?.bytes(k)?;
            match d {
                Some(d) => e.bytes(d)?,
                None => e.null()?,
            };
        }
        Ok(())
    })
}

fn decode_subs(b: &[u8]) -> threnody_core::Result<Vec<Subscription>> {
    let mut d = Decoder::new(b);
    let n = d.array_len()?;
    let mut out = Vec::new();
    for _ in 0..n {
        if d.array_len()? != 3 {
            return Err(threnody_core::Error::Malformed("subscription"));
        }
        let link: DirectoryLink = d.str()?.parse()?;
        let key = IssuerKey::decode(d.bytes()?)?;
        let doc = if d.is_null()? {
            None
        } else {
            DirectoryDoc::decode(d.bytes()?).ok()
        };
        out.push(Subscription { link, key, doc });
    }
    finish(&d)?;
    Ok(out)
}

fn encode_wallet(w: &Wallet) -> threnody_core::Result<Vec<u8>> {
    let creds: Vec<([u8; 32], u32, Vec<u8>)> = w
        .creds
        .iter()
        .map(|(d, e, c)| Ok((*d, *e, c.encode()?)))
        .collect::<threnody_core::Result<_>>()?;
    let size = 64 + creds.iter().map(|(_, _, c)| c.len() + 48).sum::<usize>() + w.used.len() * 48;
    cbor::to_vec(size, |e| {
        e.map_len(2)?.u8(0)?.array_len(creds.len())?;
        for (d, ep, c) in &creds {
            e.array_len(3)?.bytes(d)?.u32(*ep)?.bytes(c)?;
        }
        e.u8(1)?.array_len(w.used.len())?;
        for ((fp, ep), bits) in &w.used {
            e.array_len(3)?.bytes(&fp.0)?.u32(*ep)?.u64(*bits)?;
        }
        Ok(())
    })
}

fn decode_wallet(b: &[u8]) -> threnody_core::Result<Wallet> {
    let mut dec = Decoder::new(b);
    let mut w = Wallet::default();
    read_map(&mut dec, |k, d| {
        match k {
            0 => {
                for _ in 0..d.array_len()? {
                    d.array_len()?;
                    let dir = fixed_bytes::<32>(d)?;
                    let epoch = d.u32()?;
                    if let Ok(c) = Credential::decode(d.bytes()?) {
                        w.creds.push((dir, epoch, c));
                    }
                }
            }
            1 => {
                for _ in 0..d.array_len()? {
                    d.array_len()?;
                    let fp = Fingerprint(fixed_bytes::<20>(d)?);
                    let epoch = d.u32()?;
                    w.used.insert((fp, epoch), d.u64()?);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    Ok(w)
}

type ServerRelays = HashMap<PublicIdentity, (RelayDescriptor, bool)>;

fn encode_server(review: bool, relays: &ServerRelays) -> threnody_core::Result<Vec<u8>> {
    let raw: Vec<(Vec<u8>, bool)> = relays
        .values()
        .map(|(d, l)| Ok((d.encode()?, *l)))
        .collect::<threnody_core::Result<_>>()?;
    cbor::to_vec(
        64 + raw.iter().map(|(d, _)| d.len() + 8).sum::<usize>(),
        |e| {
            e.map_len(2)?
                .u8(0)?
                .bool(review)?
                .u8(1)?
                .array_len(raw.len())?;
            for (d, l) in &raw {
                e.array_len(2)?.bytes(d)?.bool(*l)?;
            }
            Ok(())
        },
    )
}

fn decode_server(b: &[u8]) -> threnody_core::Result<(bool, ServerRelays)> {
    let mut dec = Decoder::new(b);
    let (mut review, mut relays) = (false, HashMap::new());
    read_map(&mut dec, |k, d| {
        match k {
            0 => review = d.bool()?,
            1 => {
                for _ in 0..d.array_len()? {
                    d.array_len()?;
                    let desc = d.bytes()?.to_vec();
                    let listed = d.bool()?;
                    if let Ok(desc) = RelayDescriptor::decode(&desc) {
                        relays.insert(desc.identity, (desc, listed));
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    Ok((review, relays))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks_group_addresses() {
        let a: IpAddr = "192.0.2.7".parse().unwrap();
        let b: IpAddr = "192.0.2.200".parse().unwrap();
        let c: IpAddr = "192.0.3.7".parse().unwrap();
        assert_eq!(network(a), network(b));
        assert_ne!(network(a), network(c));
        let m: IpAddr = "::ffff:192.0.2.9".parse().unwrap();
        assert_eq!(network(a), network(m));
        let v6a: IpAddr = "2001:db8:1:2:3::1".parse().unwrap();
        let v6b: IpAddr = "2001:db8:1:2:4::1".parse().unwrap();
        assert_eq!(network(v6a), network(v6b));
    }

    #[test]
    fn state_round_trips() {
        let id = threnody_core::Identity::generate();
        let relay =
            RelayDescriptor::new(&id, vec!["192.0.2.1:7450".into()], now_ms(), 3_600_000).unwrap();
        let mut relays = HashMap::new();
        relays.insert(relay.identity, (relay.clone(), true));
        let (review, back) = decode_server(&encode_server(true, &relays).unwrap()).unwrap();
        assert!(review);
        assert_eq!(back.get(&relay.identity).unwrap(), &(relay, true));

        let mut w = Wallet::default();
        w.used.insert((id.public().fingerprint(), 5), 0b101);
        let back = decode_wallet(&encode_wallet(&w).unwrap()).unwrap();
        assert_eq!(back.used.get(&(id.public().fingerprint(), 5)), Some(&0b101));
    }
}
