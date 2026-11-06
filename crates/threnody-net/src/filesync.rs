//! File contents across our own devices (Appendix J, *Files*).
//!
//! Transcripts carry file records (name, size), not contents. A device
//! that lacks a file's contents asks one of our other devices for them:
//!
//! - the requester sends a `FileWant` (a [`FileRef`], an offset, and the
//!   content hash once it has one); the source answers with up to
//!   [`WINDOW`] `FilePart`s of [`PART`] bytes, or `FileMissing`;
//! - after the last part of a window the requester asks for the next one;
//!   once it has every byte it checks the BLAKE3 hash, keeps the file
//!   through the app's [`FileStore`] and records where in history;
//! - a source answers only from its own history and serves exactly what
//!   was sent (`media::as_sent`: the metadata-stripped form first);
//! - a fetch that stalls, or whose source goes away, starts again from
//!   where it got to with another source, or the same one later.
//!
//! New photos and voice or video messages come over by themselves (the
//! auto policy, [`wants_auto`]): not sensitive, not disappearing, at most
//! [`AUTO_DAYS`] old, while auto-fetch is on, not on mobile data, and only
//! from a sibling reached directly. Everything else waits for a tap
//! ([`Node::fetch_file`]). The queue of fetches is kept across restarts
//! (state `file-fetches`); the bytes are not.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, read_map, required};
use threnody_core::devsync::{FileMissing, FilePart, FileRef, FileWant, PART, WINDOW};
use threnody_core::history::{ConversationId, Entry, file_matches, file_ref};
use threnody_core::media::{as_sent, looks_like_image};
use threnody_core::message::{FEATURE_DEVICE_SYNC, MAX_FILE};
use threnody_core::{PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::node::{Event, Node, SessionInfo, lock};

/// Where the app keeps files. Called off the async runtime.
pub trait FileStore: Send + Sync {
    /// The contents at `location` (a path or URI this device recorded),
    /// or None if gone, unreadable, or larger than `max` bytes.
    fn read(&self, location: &str, max: u64) -> Option<Vec<u8>>;
    /// Keeps `data` (a file from another of our devices) privately;
    /// returns its location for history.
    fn keep(&self, name: &str, data: &[u8], clip: bool) -> Option<String>;
}

/// Where a file's contents are, for a history entry without them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchState {
    /// Nothing could bring it: no file store, or no other device.
    NotHere,
    /// Not asked for: tap to get it.
    Idle,
    /// Asked for; no other device that could serve it is connected.
    Waiting,
    /// Coming from another device.
    Fetching { received: u64, total: u64 },
    /// Every other device of the account said it doesn't have it.
    Unavailable,
}

/// A queued fetch and where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchProgress {
    pub file: FileRef,
    pub state: FetchState,
}

/// Fetches queued at once; auto fetches past it are dropped.
pub const MAX_FETCHES: usize = 64;
/// Fetches in flight at once.
pub const MAX_ACTIVE: usize = 2;
/// The largest original a source reads (an image is stripped after).
pub const MAX_READ: u64 = 4 * MAX_FILE as u64;
/// A fetch with no part for this long asks again.
pub const STALL: Duration = Duration::from_secs(60);
/// Contents a source keeps in memory for the windows still to come.
pub const SERVE_CACHE: usize = 2;
pub const SERVE_TTL: Duration = Duration::from_secs(120);
/// Files older than this (days) only come over on a tap.
pub const AUTO_DAYS: u64 = 30;
/// Where the queue is kept.
pub(crate) const STATE: &str = "file-fetches";
const VERSION: u8 = 1;
const DAY_MS: u64 = 86_400_000;

/// What a part did to a fetch.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Accept {
    /// Not for this fetch, or not the part it waits for.
    Reject,
    /// The window is done: ask from this offset.
    More(u64),
    /// More parts of this window are coming.
    Wait,
    /// Every byte, and the hash matches.
    Done(Vec<u8>),
    /// Start again from the beginning (the content changed, or the hash
    /// didn't match).
    Restart,
}

pub(crate) struct Fetch {
    pub(crate) conv: ConversationId,
    pub(crate) file: FileRef,
    /// Queued by the auto policy, not by the user.
    pub(crate) auto: bool,
    /// Random; fresh per source and per restart.
    request: u64,
    /// The device asked.
    source: Option<PublicIdentity>,
    /// Devices that said they don't have it (kept in memory only).
    refused: Vec<PublicIdentity>,
    /// The offset after which we ask again.
    window_end: u64,
    hash: Option<[u8; 32]>,
    data: Vec<u8>,
    last: Option<Instant>,
}

impl Fetch {
    pub(crate) fn new(conv: ConversationId, file: FileRef, auto: bool) -> Self {
        Self {
            conv,
            file,
            auto,
            request: 0,
            source: None,
            refused: Vec::new(),
            window_end: 0,
            hash: None,
            data: Vec::new(),
            last: None,
        }
    }

    /// Takes `part` from `from`.
    pub(crate) fn accept(&mut self, from: &PublicIdentity, part: &FilePart) -> Accept {
        if self.source.as_ref() != Some(from)
            || part.request != self.request
            || part.total != self.file.size
        {
            return Accept::Reject;
        }
        match self.hash {
            Some(h) if h != part.hash => {
                // The source's content changed: never stitch two versions.
                if part.offset != 0 {
                    return Accept::Restart;
                }
                self.data.clear();
                self.hash = Some(part.hash);
            }
            Some(_) => {}
            None => self.hash = Some(part.hash),
        }
        if part.offset != self.data.len() as u64 {
            return Accept::Reject;
        }
        self.data.extend_from_slice(&part.data);
        let len = self.data.len() as u64;
        if len == self.file.size {
            if Some(*blake3::hash(&self.data).as_bytes()) == self.hash {
                return Accept::Done(std::mem::take(&mut self.data));
            }
            self.data.clear();
            return Accept::Restart;
        }
        if len >= self.window_end {
            return Accept::More(len);
        }
        Accept::Wait
    }

    /// Asks `source` from where the data so far ends, under a fresh
    /// request id.
    fn start(&mut self, source: PublicIdentity) -> FileWant {
        self.source = Some(source);
        self.request = crate::history::local_id();
        self.last = Some(Instant::now());
        self.next_want(self.data.len() as u64)
    }

    /// The want for the window from `offset`, under the current request.
    fn next_want(&mut self, offset: u64) -> FileWant {
        self.window_end = (offset + (WINDOW * PART) as u64).min(self.file.size);
        FileWant {
            request: self.request,
            file: self.file.clone(),
            offset,
            hash: self.hash,
        }
    }

    /// Starts again from nothing (after a [`Accept::Restart`]).
    fn restart(&mut self) -> Option<FileWant> {
        let source = self.source?;
        self.data.clear();
        self.hash = None;
        Some(self.start(source))
    }
}

/// Whether `e`'s file comes over by itself: a photo or voice or video
/// message without its contents here, neither sensitive nor disappearing,
/// at most [`AUTO_DAYS`] old.
pub(crate) fn wants_auto(e: &Entry, now_ms: u64) -> bool {
    let Some(f) = &e.file else {
        return false;
    };
    f.location.is_none()
        && !f.sensitive
        && (f.clip.is_some() || looks_like_image(&f.name))
        && f.size <= MAX_FILE as u64
        && e.at_ms + AUTO_DAYS * DAY_MS >= now_ms
        && e.expires_at_ms.is_none()
}

/// Which of `candidates` (sessions with our own devices that take file
/// fetches, less those that refused) to ask for `f`: its author if it is
/// one of them, else the longest-standing. An auto fetch only asks a
/// device reached directly, never through a relay, an onion circuit or
/// Bluetooth.
pub(crate) fn pick_source(candidates: &[SessionInfo], f: &Fetch) -> Option<PublicIdentity> {
    let ok: Vec<&SessionInfo> = candidates
        .iter()
        .filter(|s| !f.refused.contains(&s.peer))
        .filter(|s| {
            !f.auto || (s.via.is_none() && !matches!(s.transport, "ble" | "relay" | "onion"))
        })
        .collect();
    ok.iter()
        .find(|s| s.peer.as_bytes() == &f.file.device)
        .or_else(|| ok.iter().min_by_key(|s| s.since_ms))
        .map(|s| s.peer)
}

/// Where `f` is, given `others`, every other device of our account.
pub(crate) fn state_of(f: &Fetch, others: &[PublicIdentity]) -> FetchState {
    if !others.is_empty() && others.iter().all(|d| f.refused.contains(d)) {
        FetchState::Unavailable
    } else if f.source.is_some() {
        FetchState::Fetching {
            received: f.data.len() as u64,
            total: f.file.size,
        }
    } else {
        FetchState::Waiting
    }
}

/// Contents read to serve, for the windows still to come.
struct Served {
    to: PublicIdentity,
    request: u64,
    hash: [u8; 32],
    data: Arc<Vec<u8>>,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct FileSync {
    store: Option<Arc<dyn FileStore>>,
    pub(crate) fetches: Vec<Fetch>,
    serving: VecDeque<Served>,
    /// Devices whose want we are reading for, with the latest want that
    /// came meanwhile.
    reading: HashMap<PublicIdentity, Option<(Vec<u8>, FileWant)>>,
}

impl FileSync {
    /// `VERSION (1) || cbor [* { 0: conversation, 1: bstr .cbor FileRef, ? 2: true (auto) }]`
    pub(crate) fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let body = cbor::to_vec(self.fetches.len() * 200 + 16, |e| {
            e.array_len(self.fetches.len())?;
            for f in &self.fetches {
                let r = f
                    .file
                    .encode()
                    .map_err(|_| const_cbor::Error::OUT_OF_SPACE)?;
                e.map_len(2 + usize::from(f.auto))?;
                e.u8(0)?.bytes(&f.conv.to_bytes())?;
                e.u8(1)?.bytes(&r)?;
                if f.auto {
                    e.u8(2)?.bool(true)?;
                }
            }
            Ok(())
        })?;
        Ok([&[VERSION][..], &body].concat())
    }

    /// The queue as [`FileSync::encode`] kept it; damaged state gives an
    /// empty queue.
    pub(crate) fn decode(b: &[u8]) -> Self {
        let mut s = Self::default();
        let Some((&VERSION, body)) = b.split_first() else {
            return s;
        };
        let mut fetches = Vec::new();
        let mut dec = Decoder::new(body);
        let ok = (|| -> threnody_core::Result<()> {
            for _ in 0..dec.array_len()? {
                let (mut conv, mut file, mut auto) = (None, None, false);
                read_map(&mut dec, |k, d| {
                    match k {
                        0 => conv = ConversationId::from_bytes(d.bytes()?),
                        1 => file = Some(FileRef::decode(d.bytes()?)?),
                        2 => auto = d.bool()?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                if fetches.len() < MAX_FETCHES {
                    fetches.push(Fetch::new(
                        required(conv, "conversation")?,
                        required(file, "file")?,
                        auto,
                    ));
                }
            }
            Ok(())
        })();
        if ok.is_ok() {
            s.fetches = fetches;
        }
        s
    }

    fn position(&self, r: &FileRef) -> Option<usize> {
        self.fetches.iter().position(|f| f.file == *r)
    }

    /// Queues an auto fetch, unless queued already or the queue is full.
    pub(crate) fn add_auto(&mut self, conv: ConversationId, file: FileRef) -> bool {
        if self.position(&file).is_some() || self.fetches.len() >= MAX_FETCHES {
            return false;
        }
        self.fetches.push(Fetch::new(conv, file, true));
        true
    }

    /// Queues a user fetch, or makes a queued one the user's and asks
    /// every device again. The oldest auto fetch makes room if needed.
    pub(crate) fn add_user(&mut self, conv: ConversationId, file: FileRef) -> Result<()> {
        if let Some(i) = self.position(&file) {
            let f = &mut self.fetches[i];
            f.auto = false;
            f.refused.clear();
            return Ok(());
        }
        if self.fetches.len() >= MAX_FETCHES {
            let Some(i) = self.fetches.iter().position(|f| f.auto) else {
                return Err(NetError::NotAllowed("too many downloads".into()));
            };
            self.fetches.remove(i);
        }
        self.fetches.push(Fetch::new(conv, file, false));
        Ok(())
    }

    /// Contents served to `to` under `request`, if still kept.
    fn served(&mut self, to: &PublicIdentity, request: u64) -> Option<([u8; 32], Arc<Vec<u8>>)> {
        self.serving.retain(|s| s.at.elapsed() < SERVE_TTL);
        self.serving
            .iter()
            .find(|s| s.to == *to && s.request == request)
            .map(|s| (s.hash, s.data.clone()))
    }

    fn keep_served(&mut self, s: Served) {
        self.serving
            .retain(|x| !(x.to == s.to && x.request == s.request));
        while self.serving.len() >= SERVE_CACHE {
            self.serving.pop_front();
        }
        self.serving.push_back(s);
    }
}

/// Up to [`WINDOW`] parts of `data` from `offset`; one empty part for an
/// empty file.
fn parts(request: u64, hash: [u8; 32], data: &[u8], offset: u64) -> Vec<FilePart> {
    let total = data.len() as u64;
    if total == 0 {
        return vec![FilePart {
            request,
            offset: 0,
            total: 0,
            hash,
            data: Vec::new(),
        }];
    }
    let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(data.len());
    data[start..]
        .chunks(PART)
        .take(WINDOW)
        .enumerate()
        .map(|(i, c)| FilePart {
            request,
            offset: (start + i * PART) as u64,
            total,
            hash,
            data: c.to_vec(),
        })
        .collect()
}

impl Node {
    /// Installs where the app keeps files, so this device can fetch them
    /// from our other devices and serve them to those.
    pub fn set_file_store(&self, store: Arc<dyn FileStore>) {
        lock(&self.shared.file_sync).store = Some(store);
        self.pump_fetches();
    }

    fn file_store(&self) -> Option<Arc<dyn FileStore>> {
        lock(&self.shared.file_sync).store.clone()
    }

    /// Whether new photos and voice and video messages in history synced
    /// from our other devices come over by themselves (on by default;
    /// apps turn it off on metered networks too).
    pub fn auto_fetch(&self) -> bool {
        self.shared.auto_fetch.load(Ordering::Relaxed)
    }

    pub fn set_auto_fetch(&self, on: bool) {
        let was = self.shared.auto_fetch.swap(on, Ordering::Relaxed);
        if on && !was {
            self.pump_fetches();
        }
    }

    fn save_fetches(&self, fs: &FileSync) {
        self.shared.save_state(STATE, fs.encode());
    }

    /// Our other devices, by the account chain.
    fn other_devices(&self) -> Vec<PublicIdentity> {
        let me = self.identity();
        lock(&self.shared.account)
            .state()
            .devices
            .iter()
            .map(|(d, _)| *d)
            .filter(|d| *d != me)
            .collect()
    }

    /// Fetches the newest file entry of `conv` that `pick` chooses from
    /// our other devices; returns where it is. `Idle` if its contents are
    /// here already.
    pub fn fetch_file(
        &self,
        conv: ConversationId,
        pick: impl Fn(&Entry) -> bool,
    ) -> Result<FetchState> {
        let h = self.history(conv)?;
        let Some(e) = h
            .entries()
            .iter()
            .rev()
            .find(|e| e.file.is_some() && pick(e))
        else {
            return Err(NetError::NotFound("no such file".into()));
        };
        if e.file.as_ref().is_some_and(|f| f.location.is_some()) {
            return Ok(FetchState::Idle);
        }
        let Some(r) = file_ref(e) else {
            return Err(NetError::NotFound("no such file".into()));
        };
        {
            let mut fs = lock(&self.shared.file_sync);
            if fs.store.is_none() {
                return Err(NetError::NotAllowed("no file store".into()));
            }
            fs.add_user(conv, r.clone())?;
            self.save_fetches(&fs);
        }
        self.pump_fetches();
        Ok(self.fetch_state(&r))
    }

    /// Queues the files among `transcript` (just merged into `conv`) that
    /// the auto policy wants, newest first. Only references are queued:
    /// whether they are fetched now is up to [`Node::pump_fetches`].
    pub(crate) fn fetch_auto(&self, conv: ConversationId, transcript: &[Entry]) {
        if !transcript.iter().any(|e| e.file.is_some()) {
            return;
        }
        let Ok(h) = self.history(conv) else { return };
        let now = now_ms();
        let mut wanted: Vec<&Entry> = transcript
            .iter()
            .filter_map(file_ref)
            .filter_map(|r| h.entries().iter().rev().find(|e| file_matches(&r, e)))
            .filter(|e| wants_auto(e, now))
            .collect();
        if wanted.is_empty() {
            return;
        }
        wanted.sort_by_key(|e| std::cmp::Reverse(e.at_ms));
        let added = {
            let mut fs = lock(&self.shared.file_sync);
            let mut added = false;
            for r in wanted.into_iter().filter_map(file_ref) {
                added |= fs.add_auto(conv, r);
            }
            if added {
                self.save_fetches(&fs);
            }
            added
        };
        if added {
            self.pump_fetches();
        }
    }

    /// Where the contents of the file `r` names are, for an entry without
    /// them here.
    pub fn fetch_state(&self, r: &FileRef) -> FetchState {
        if lock(&self.shared.file_sync).store.is_none() {
            return FetchState::NotHere;
        }
        let others = self.other_devices();
        if others.is_empty() {
            return FetchState::NotHere;
        }
        lock(&self.shared.file_sync)
            .fetches
            .iter()
            .find(|f| f.file == *r)
            .map_or(FetchState::Idle, |f| state_of(f, &others))
    }

    /// Every queued fetch and where it is (cheap: no history is read).
    pub fn file_fetches(&self) -> Vec<FetchProgress> {
        let others = self.other_devices();
        lock(&self.shared.file_sync)
            .fetches
            .iter()
            .map(|f| FetchProgress {
                file: f.file.clone(),
                state: state_of(f, &others),
            })
            .collect()
    }

    /// Drops fetches no longer needed and starts those that can start.
    pub(crate) fn pump_fetches(&self) {
        // Which queued files still need fetching, one history load per
        // conversation (outside the lock).
        let queued: Vec<(ConversationId, FileRef, bool)> = lock(&self.shared.file_sync)
            .fetches
            .iter()
            .map(|f| (f.conv, f.file.clone(), f.auto))
            .collect();
        let now = now_ms();
        let mut by_conv: HashMap<ConversationId, Vec<Entry>> = HashMap::new();
        let mut stale: HashSet<FileRef> = HashSet::new();
        for (conv, r, auto) in &queued {
            let entries = by_conv.entry(*conv).or_insert_with(|| {
                self.history(*conv)
                    .map(|h| h.entries().to_vec())
                    .unwrap_or_default()
            });
            let needed = entries
                .iter()
                .rev()
                .find(|e| file_matches(r, e))
                .is_some_and(|e| {
                    e.file.as_ref().is_some_and(|f| f.location.is_none())
                        && (!*auto || e.at_ms + AUTO_DAYS * DAY_MS >= now)
                });
            if !needed {
                stale.insert(r.clone());
            }
        }
        let auto_ok = self.auto_fetch() && !self.on_mobile();
        let sessions: Vec<SessionInfo> = self
            .sessions()
            .into_iter()
            .filter(|s| self.is_own_device(&s.peer) && self.supports(&s.peer, FEATURE_DEVICE_SYNC))
            .collect();
        let others = self.other_devices();
        let mut sends = Vec::new();
        {
            let mut fs = lock(&self.shared.file_sync);
            let before = fs.fetches.len();
            fs.fetches.retain(|f| !stale.contains(&f.file));
            // An auto fetch nobody has goes; a user's stays to show so.
            fs.fetches
                .retain(|f| !(f.auto && state_of(f, &others) == FetchState::Unavailable));
            let changed = fs.fetches.len() != before;
            if fs.store.is_some() {
                let mut active = fs.fetches.iter().filter(|f| f.source.is_some()).count();
                for f in fs.fetches.iter_mut() {
                    if active >= MAX_ACTIVE {
                        break;
                    }
                    if f.source.is_some() || (f.auto && !auto_ok) {
                        continue;
                    }
                    if let Some(p) = pick_source(&sessions, f) {
                        let want = f.start(p);
                        sends.push((p, f.conv, want));
                        active += 1;
                    }
                }
            }
            if changed {
                self.save_fetches(&fs);
            }
        }
        for (p, conv, want) in sends {
            self.send_want(&p, conv, &want);
        }
    }

    fn send_want(&self, to: &PublicIdentity, conv: ConversationId, want: &FileWant) {
        if let Ok(b) = want.encode()
            && let Some(m) = crate::account::file_want_message(conv.to_bytes(), b)
        {
            let _ = self.send(to, m);
        }
    }

    /// Asks again where a fetch's source went away or went quiet.
    pub(crate) fn resume_fetches(&self) {
        let live: HashSet<PublicIdentity> = self.sessions().into_iter().map(|s| s.peer).collect();
        {
            let mut fs = lock(&self.shared.file_sync);
            for f in fs.fetches.iter_mut() {
                let gone = f.source.is_some_and(|s| !live.contains(&s));
                let quiet = f.last.is_none_or(|t| t.elapsed() >= STALL);
                if f.source.is_some() && (gone || quiet) {
                    f.source = None;
                }
            }
        }
        self.pump_fetches();
    }

    /// Handles a `FileWant` from `from`: answers from this device's own
    /// history, never from a location named by anyone else.
    pub(crate) fn on_file_want(&self, from: PublicIdentity, conv: &[u8], want: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let Ok(want) = FileWant::decode(want) else {
            return;
        };
        {
            let mut fs = lock(&self.shared.file_sync);
            if let Some(waiting) = fs.reading.get_mut(&from) {
                // Answered once the read in flight is done.
                *waiting = Some((conv.to_vec(), want));
                return;
            }
            if let Some((hash, data)) = fs.served(&from, want.request) {
                drop(fs);
                self.send_parts(&from, &want, hash, &data);
                return;
            }
        }
        let request = want.request;
        let missing = move |node: &Node| {
            if let Ok(b) = (FileMissing { request }).encode()
                && let Some(m) = crate::account::file_missing_message(b)
            {
                let _ = node.send(&from, m);
            }
        };
        let Some(store) = self.file_store() else {
            return missing(self);
        };
        let Some(conv) = ConversationId::from_bytes(conv) else {
            return missing(self);
        };
        let found = self
            .shared
            .home
            .find_file(self.identity_ref(), conv, &want.file, now_ms())
            .ok()
            .flatten();
        let Some((location, size)) = found
            .and_then(|e| e.file)
            .and_then(|f| f.location.map(|l| (l, f.size)))
        else {
            return missing(self);
        };
        lock(&self.shared.file_sync).reading.insert(from, None);
        let node = self.clone();
        let read = move || {
            let data = store
                .read(&location, MAX_READ)
                .and_then(|raw| as_sent(raw, size))
                .map(|d| (*blake3::hash(&d).as_bytes(), Arc::new(d)));
            let next = {
                let mut fs = lock(&node.shared.file_sync);
                if let Some((hash, d)) = &data {
                    fs.keep_served(Served {
                        to: from,
                        request: want.request,
                        hash: *hash,
                        data: d.clone(),
                        at: Instant::now(),
                    });
                }
                fs.reading.remove(&from).flatten()
            };
            match data {
                Some((hash, d)) => node.send_parts(&from, &want, hash, &d),
                None => missing(&node),
            }
            if let Some((conv, w)) = next
                && let Ok(b) = w.encode()
            {
                node.on_file_want(from, &conv, &b);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(read);
            }
            Err(_) => read(),
        }
    }

    fn send_parts(&self, to: &PublicIdentity, want: &FileWant, hash: [u8; 32], data: &[u8]) {
        // Another version than the one the requester has parts of: from
        // the start.
        let offset = if want.hash.is_some_and(|h| h != hash) || want.offset > data.len() as u64 {
            0
        } else {
            want.offset
        };
        for p in parts(want.request, hash, data, offset) {
            if let Ok(b) = p.encode()
                && let Some(m) = crate::account::file_part_message(b)
            {
                let _ = self.send(to, m);
            }
        }
    }

    /// Handles a `FilePart` from `from`.
    pub(crate) fn on_file_part(&self, from: PublicIdentity, part: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let Ok(part) = FilePart::decode(part) else {
            return;
        };
        let mut fs = lock(&self.shared.file_sync);
        let Some(i) = fs
            .fetches
            .iter()
            .position(|f| f.request == part.request && f.source.is_some())
        else {
            return;
        };
        let accepted = fs.fetches[i].accept(&from, &part);
        if accepted != Accept::Reject {
            fs.fetches[i].last = Some(Instant::now());
        }
        match accepted {
            Accept::Reject | Accept::Wait => {}
            Accept::More(o) => {
                let f = &mut fs.fetches[i];
                let (conv, want) = (f.conv, f.next_want(o));
                drop(fs);
                self.send_want(&from, conv, &want);
            }
            Accept::Restart => {
                let f = &mut fs.fetches[i];
                let conv = f.conv;
                let want = f.restart();
                drop(fs);
                if let Some(w) = want {
                    self.send_want(&from, conv, &w);
                }
            }
            Accept::Done(data) => {
                // Out of the queue first, so a duplicate part can't finish
                // it twice.
                let f = fs.fetches.remove(i);
                self.save_fetches(&fs);
                let store = fs.store.clone();
                drop(fs);
                let Some(store) = store else { return };
                let node = self.clone();
                let keep = move || {
                    let clip = node
                        .shared
                        .home
                        .find_file(node.identity_ref(), f.conv, &f.file, now_ms())
                        .ok()
                        .flatten()
                        .and_then(|e| e.file)
                        .is_some_and(|x| x.clip.is_some());
                    // An entry gone meanwhile leaves the kept file
                    // unreferenced: the app sweeps it.
                    if let Some(loc) = store.keep(&f.file.name, &data, clip)
                        && let Ok(true) = node.shared.home.set_file_location(
                            node.identity_ref(),
                            f.conv,
                            &f.file,
                            &loc,
                            now_ms(),
                        )
                    {
                        node.emit(Event::DeviceSync { from });
                    }
                    node.pump_fetches();
                };
                match tokio::runtime::Handle::try_current() {
                    Ok(h) => {
                        h.spawn_blocking(keep);
                    }
                    Err(_) => keep(),
                }
            }
        }
    }

    /// Handles a `FileMissing` from `from`: ask another device.
    pub(crate) fn on_file_missing(&self, from: PublicIdentity, body: &[u8]) {
        let Ok(m) = FileMissing::decode(body) else {
            return;
        };
        {
            let mut fs = lock(&self.shared.file_sync);
            let Some(f) = fs
                .fetches
                .iter_mut()
                .find(|f| f.request == m.request && f.source == Some(from))
            else {
                return;
            };
            f.refused.push(from);
            f.source = None;
        }
        self.pump_fetches();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use threnody_core::history::FileNote;

    fn file(size: u64) -> FileRef {
        FileRef {
            device: [1; 32],
            id: 5,
            name: "a.png".into(),
            size,
            at_ms: 1,
        }
    }

    fn fetching(size: u64, from: PublicIdentity) -> Fetch {
        let mut f = Fetch::new(ConversationId::Peer([2; 32]), file(size), false);
        f.start(from);
        f
    }

    fn peer() -> PublicIdentity {
        threnody_core::Identity::generate().public()
    }

    fn part_of(f: &Fetch, data: &[u8], offset: usize, len: usize) -> FilePart {
        FilePart {
            request: f.request,
            offset: offset as u64,
            total: data.len() as u64,
            hash: *blake3::hash(data).as_bytes(),
            data: data[offset..offset + len].to_vec(),
        }
    }

    #[test]
    fn accept_assembles_in_order_and_verifies_hash() {
        let p = peer();
        let data: Vec<u8> = (0..3 * PART).map(|i| i as u8).collect();
        let mut f = fetching(data.len() as u64, p);
        // Out of order.
        assert_eq!(
            f.accept(&p, &part_of(&f, &data, PART, PART)),
            Accept::Reject
        );
        assert_eq!(f.accept(&p, &part_of(&f, &data, 0, PART)), Accept::Wait);
        // The wrong sender or request.
        assert_eq!(
            f.accept(&peer(), &part_of(&f, &data, PART, PART)),
            Accept::Reject
        );
        let mut other = part_of(&f, &data, PART, PART);
        other.request ^= 1;
        assert_eq!(f.accept(&p, &other), Accept::Reject);
        assert_eq!(f.accept(&p, &part_of(&f, &data, PART, PART)), Accept::Wait);
        assert_eq!(
            f.accept(&p, &part_of(&f, &data, 2 * PART, PART)),
            Accept::Done(data.clone())
        );

        // A bad final hash: start again, nothing kept.
        let mut f = fetching(4, p);
        let mut bad = part_of(&f, &[1, 2, 3, 4], 0, 4);
        bad.hash = [9; 32];
        assert_eq!(f.accept(&p, &bad), Accept::Restart);
        assert!(f.data.is_empty());

        // An empty file is done on its one empty part.
        let mut f = fetching(0, p);
        assert_eq!(
            f.accept(&p, &part_of(&f, &[], 0, 0)),
            Accept::Done(Vec::new())
        );
    }

    #[test]
    fn a_changed_source_restarts_the_fetch() {
        let p = peer();
        let (old, new) = (vec![1u8; 2 * PART], vec![2u8; 2 * PART]);
        let mut f = fetching(old.len() as u64, p);
        assert_eq!(f.accept(&p, &part_of(&f, &old, 0, PART)), Accept::Wait);
        assert_eq!(
            f.accept(&p, &part_of(&f, &new, PART, PART)),
            Accept::Restart
        );
        let first = f.request;
        let want = f.restart().unwrap();
        assert_ne!(want.request, first, "a fresh request id");
        assert_eq!((want.offset, want.hash), (0, None));
        // At offset 0 a new version is simply taken.
        let mut f = fetching(old.len() as u64, p);
        assert_eq!(f.accept(&p, &part_of(&f, &old, 0, PART)), Accept::Wait);
        f.data.clear();
        assert_eq!(f.accept(&p, &part_of(&f, &new, 0, PART)), Accept::Wait);
        assert_eq!(f.hash, Some(*blake3::hash(&new).as_bytes()));
    }

    #[test]
    fn window_asks_for_more() {
        let p = peer();
        let data = vec![3u8; 8 * PART];
        let mut f = fetching(data.len() as u64, p);
        for i in 0..3 {
            assert_eq!(
                f.accept(&p, &part_of(&f, &data, i * PART, PART)),
                Accept::Wait
            );
        }
        let more = f.accept(&p, &part_of(&f, &data, 3 * PART, PART));
        assert_eq!(more, Accept::More((4 * PART) as u64));
        let want = f.next_want((4 * PART) as u64);
        assert_eq!(want.offset, (4 * PART) as u64);
        assert_eq!(want.hash, f.hash);
        assert_eq!(f.window_end, (8 * PART) as u64);
    }

    fn photo(at_ms: u64, name: &str) -> Entry {
        Entry {
            at_ms,
            outgoing: true,
            device: [1; 32],
            text: String::new(),
            offline: false,
            expires_at_ms: None,
            file: Some(FileNote {
                name: name.into(),
                size: 100,
                location: None,
                sensitive: false,
                album: 0,
                clip: None,
                forwarded: false,
            }),
            local_id: 9,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
            remote_id: 0,
            edited_ms: 0,
            read_ms: 0,
            reactions: Vec::new(),
            quote: None,
            mentions: Vec::new(),
            poll: None,
            forwarded: false,
            list: None,
            ticked_ms: 0,
        }
    }

    #[test]
    fn auto_policy() {
        let now = 100 * DAY_MS;
        let fresh = photo(now - DAY_MS, "a.png");
        assert!(wants_auto(&fresh, now));
        let with = |f: &dyn Fn(&mut Entry)| {
            let mut e = fresh.clone();
            f(&mut e);
            wants_auto(&e, now)
        };
        assert!(!with(&|e| e.file.as_mut().unwrap().sensitive = true));
        assert!(!with(&|e| e.expires_at_ms = Some(now + 1)));
        assert!(!with(&|e| e.at_ms = now - 31 * DAY_MS));
        assert!(!with(&|e| e.file.as_mut().unwrap().name = "a.pdf".into()));
        assert!(!with(
            &|e| e.file.as_mut().unwrap().location = Some("/x".into())
        ));
        assert!(!with(
            &|e| e.file.as_mut().unwrap().size = MAX_FILE as u64 + 1
        ));
        assert!(!with(&|e| e.file = None));
        assert!(with(&|e| {
            let f = e.file.as_mut().unwrap();
            f.name = "voice.ogg".into();
            f.clip = Some(threnody_core::message::Clip {
                video: false,
                duration_ms: 1000,
            });
        }));
    }

    #[test]
    fn fetch_queue_round_trips_and_is_bounded() {
        let conv = ConversationId::Group([3; 16]);
        let r = |i: u64| FileRef { id: i, ..file(10) };
        let mut fs = FileSync::default();
        fs.add_user(conv, r(1000)).unwrap();
        for i in 0..MAX_FETCHES as u64 {
            fs.add_auto(conv, r(i));
        }
        assert_eq!(
            fs.fetches.len(),
            MAX_FETCHES,
            "the 64th auto fetch is dropped"
        );
        assert!(!fs.add_auto(conv, r(5000)));

        let back = FileSync::decode(&fs.encode().unwrap());
        let summary = |fs: &FileSync| {
            fs.fetches
                .iter()
                .map(|f| (f.conv, f.file.clone(), f.auto))
                .collect::<Vec<_>>()
        };
        assert_eq!(summary(&back), summary(&fs));
        assert!(FileSync::decode(&[1, 0x9f, 0xff, 7]).fetches.is_empty());
        assert!(FileSync::decode(&[2]).fetches.is_empty());
        let bytes = fs.encode().unwrap();
        assert!(
            FileSync::decode(&bytes[..bytes.len() - 3])
                .fetches
                .is_empty()
        );

        // A user fetch evicts the oldest auto one.
        fs.add_user(conv, r(2000)).unwrap();
        assert_eq!(fs.fetches.len(), MAX_FETCHES);
        assert!(fs.position(&r(0)).is_none());
        assert!(fs.position(&r(1)).is_some());
        // Asking again for a queued auto fetch makes it the user's.
        fs.add_user(conv, r(1)).unwrap();
        assert!(!fs.fetches[fs.position(&r(1)).unwrap()].auto);

        // A queue of user fetches only refuses another.
        let mut users = FileSync::default();
        for i in 0..MAX_FETCHES as u64 {
            users.add_user(conv, r(i)).unwrap();
        }
        assert!(users.add_user(conv, r(9999)).is_err());
    }

    fn session(
        peer: PublicIdentity,
        since_ms: u64,
        transport: &'static str,
        via: bool,
    ) -> SessionInfo {
        SessionInfo {
            peer,
            addr: "127.0.0.1:1".parse().unwrap(),
            suite: threnody_core::crypto::aead::Suite::ChaCha20Poly1305,
            transport,
            since_ms,
            outbound: true,
            via: via.then(self::peer),
            session_id: [0; 32],
            remote: String::new(),
        }
    }

    #[test]
    fn pick_source_prefers_author_and_skips_refused_and_relays_for_auto() {
        let (a, b, author) = (peer(), peer(), peer());
        let mut f = Fetch::new(ConversationId::Peer([2; 32]), file(10), false);
        f.file.device = *author.as_bytes();
        let all = [
            session(a, 5, "tcp", false),
            session(b, 1, "tcp", false),
            session(author, 9, "tcp", false),
        ];
        assert_eq!(pick_source(&all, &f), Some(author));
        assert_eq!(pick_source(&all[..2], &f), Some(b), "oldest session");
        f.refused.push(b);
        assert_eq!(pick_source(&all[..2], &f), Some(a));
        f.refused.push(a);
        assert_eq!(pick_source(&all[..2], &f), None);

        let mut auto = Fetch::new(ConversationId::Peer([2; 32]), file(10), true);
        let indirect = [
            session(a, 1, "relay", true),
            session(b, 2, "onion", true),
            session(author, 3, "ble", false),
        ];
        assert_eq!(pick_source(&indirect, &auto), None);
        auto.auto = false;
        assert_eq!(
            pick_source(&indirect, &auto),
            Some(a),
            "a tap uses any path"
        );
    }

    #[test]
    fn unavailable_only_when_every_other_device_refused() {
        let (laptop, cli) = (peer(), peer());
        let mut f = Fetch::new(ConversationId::Peer([2; 32]), file(10), false);
        assert_eq!(state_of(&f, &[laptop, cli]), FetchState::Waiting);
        f.refused.push(cli);
        assert_eq!(state_of(&f, &[laptop, cli]), FetchState::Waiting);
        f.start(laptop);
        assert_eq!(
            state_of(&f, &[laptop, cli]),
            FetchState::Fetching {
                received: 0,
                total: 10
            }
        );
        f.source = None;
        f.refused.push(laptop);
        assert_eq!(state_of(&f, &[laptop, cli]), FetchState::Unavailable);
    }

    #[test]
    fn parts_cover_a_window_from_the_offset() {
        let data: Vec<u8> = (0..(5 * PART + 10)).map(|i| i as u8).collect();
        let ps = parts(1, [0; 32], &data, 0);
        assert_eq!(ps.len(), WINDOW);
        assert_eq!(ps[3].offset, (3 * PART) as u64);
        let rest = parts(1, [0; 32], &data, (4 * PART) as u64);
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[1].data.len(), 10);
        let empty = parts(1, [0; 32], &[], 0);
        assert_eq!(empty.len(), 1);
        assert!(empty[0].data.is_empty());
    }
}
