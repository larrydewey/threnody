//! Files too large for one message (`docs/appendix-u-large-files.md`,
//! phase 1): offered to a contact and pulled by it in pieces.
//!
//! - **Sending.** [`Node::send_file_at`] reads a file through the app's
//!   [`LargeFileStore`]. One that fits a message, or for a peer without
//!   [`FEATURE_LARGE_FILES`], goes inline as before. A larger one is
//!   encrypted piece by piece under a fresh key ([`tx::prepare`]), and its
//!   piece hashes are kept (state `xfer-<root>`) while an [`Offer`] goes
//!   to the peer as a tracked message.
//! - **Serving.** The sender answers `Want`s from the conversation it
//!   offered the file to: the hash list, and pieces made again from the
//!   file as it is now, each sent only if its hash is the one committed
//!   to. Until the peer says `Have` (then it forgets the offer), for at
//!   most [`SERVE_DAYS`], and while the history entry exists.
//! - **Receiving.** An offer is recorded in history without contents.
//!   The download starts by itself under the auto policy, or when the
//!   user asks, and only with [`SPACE_MARGIN`] left after it. Verified
//!   pieces, still encrypted under the file key, go to a partial file in
//!   the node's home; on restart that file is checked piece by piece, so a
//!   download goes on where it was. Once every piece is in, the file is
//!   decrypted a piece at a time into the app's store, the partial file
//!   and hash list are deleted, and the sender hears `Have`.
//!
//! Nothing here holds a whole file in memory, except a photo whose
//! metadata must be removed: that is read whole (at most
//! [`MAX_STRIP_IN_MEMORY`]), since stripping can change its size. A
//! video's metadata is removed as it is read: the boxes that hold it
//! become [`media::Edits`], found from the video's headers and movie box
//! and applied to every piece.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, fixed_bytes, read_map, required};
use threnody_core::devsync::FileRef;
use threnody_core::history::{ConversationId, FileNote};
use threnody_core::media::{self, Stripped};
use threnody_core::message::{AppMessage, Clip, FEATURE_LARGE_FILES};
use threnody_core::transfer::{
    self as tx, FileKey, HASHES_PER_PART, MAX_WANT, Offer, PIECE, PIECE_OVERHEAD, TransferMsg,
};
use threnody_core::{PublicIdentity, now_ms};
use zeroize::Zeroizing;

use crate::delivery::Tag;
use crate::error::{NetError, Result};
use crate::history::{OutgoingFile, local_id};
use crate::node::{Event, Node, lock};

/// Where the app keeps files, for files too large for one message.
/// Called off the async runtime.
pub trait LargeFileStore: Send + Sync {
    /// How many bytes the file at `location` (a path or URI) holds, as
    /// its contents read; None if it is gone.
    fn size(&self, location: &str) -> Option<u64>;
    /// Up to `len` bytes of the file at `location` from `offset` (fewer
    /// only at its end); None if it can't be read.
    fn read_at(&self, location: &str, offset: u64, len: usize) -> Option<Vec<u8>>;
    /// Keeps what `src` reads (a received file called `name`) privately,
    /// sealed as the app keeps received media; its location.
    fn keep_stream(&self, name: &str, clip: bool, src: &mut dyn Read) -> Option<String>;
    /// Bytes free where received files are kept.
    fn free_space(&self) -> Option<u64>;
}

/// Pieces asked for at once.
pub const WINDOW: u64 = 8;
/// A download with nothing for this long asks again.
pub const STALL: Duration = Duration::from_secs(60);
/// How long a sender serves a file its peer hasn't finished.
pub const SERVE_DAYS: u64 = 30;
/// Largest photo whose metadata is removed before offering it (it is
/// read whole to strip). Videos have no limit but their movie box's,
/// [`media::MAX_HELD`].
pub const MAX_STRIP_IN_MEMORY: u64 = 64 * 1024 * 1024;
/// Space a download must leave free.
pub const SPACE_MARGIN: u64 = 512 * 1024 * 1024;
/// Largest automatic download on Wi-Fi, and on mobile data.
pub const AUTO_LIMIT: u64 = 25_000_000;
pub const AUTO_LIMIT_MOBILE: u64 = 8_000_000;
/// Offers kept for downloads; past it the oldest not under way go.
pub const MAX_INCOMING: usize = 256;
/// Files we serve at once; past it the oldest go.
pub const MAX_OUTGOING: usize = 1024;
/// Downloads under way at once.
pub const MAX_ACTIVE: usize = 2;
use threnody_core::transfer::{HASHES_STATE_PREFIX as HASHES_PREFIX, STATE_IN, STATE_OUT};
/// Subdirectory of the home holding partial downloads.
const PARTIAL_DIR: &str = "transfers";
const VERSION: u8 = 1;
const DAY_MS: u64 = 86_400_000;
/// Stripped files kept in memory for the windows still to come.
const SERVE_CACHE: usize = 2;
const SERVE_TTL: Duration = Duration::from_secs(120);

/// Where a received offer's download is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    /// Not asked for: tap to download.
    Idle,
    /// Asked for, waiting for the sender to be reachable.
    Waiting,
    /// Coming.
    Fetching { received: u64, total: u64 },
    /// Not enough space left: free some and ask again.
    NoSpace,
    /// The sender no longer has it.
    Unavailable,
    /// What came didn't match what was offered, or couldn't be kept.
    Failed,
    /// Here: the history entry has its location now (only in events).
    Done,
}

/// The status a received offer keeps across restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Idle = 0,
    Wanted = 1,
    NoSpace = 2,
    Unavailable = 3,
    Failed = 4,
}

impl Status {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Wanted,
            2 => Self::NoSpace,
            3 => Self::Unavailable,
            4 => Self::Failed,
            _ => Self::Idle,
        }
    }
}

/// A file we offered and serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Outgoing {
    root: [u8; 32],
    key: [u8; 32],
    conv: ConversationId,
    local_id: u64,
    name: String,
    location: String,
    size: u64,
    /// Served with its metadata removed, as it was offered.
    strip: bool,
    at_ms: u64,
}

/// A download under way (in memory only).
struct Run {
    request: u64,
    /// The piece hashes, once all came and match the root.
    hashes: Vec<[u8; 32]>,
    verified: bool,
    have: Vec<bool>,
    received: u64,
    /// Pieces (or hash parts) asked for and not yet in.
    inflight: Vec<u64>,
    last: Instant,
    file: Option<std::fs::File>,
}

/// An offer we received.
pub(crate) struct Incoming {
    conv: ConversationId,
    from: PublicIdentity,
    offer: Offer,
    at_ms: u64,
    status: Status,
    /// Asked for by the user (not only by the auto policy).
    user: bool,
    run: Option<Run>,
}

impl Incoming {
    fn state(&self) -> TransferState {
        match (self.status, &self.run) {
            (Status::NoSpace, _) => TransferState::NoSpace,
            (Status::Unavailable, _) => TransferState::Unavailable,
            (Status::Failed, _) => TransferState::Failed,
            (Status::Idle, _) => TransferState::Idle,
            (Status::Wanted, Some(r)) if r.verified || !r.hashes.is_empty() => {
                TransferState::Fetching {
                    received: r.received,
                    total: self.offer.size,
                }
            }
            (Status::Wanted, _) => TransferState::Waiting,
        }
    }

    fn file_ref(&self) -> FileRef {
        FileRef {
            device: *self.from.as_bytes(),
            id: self.offer.id,
            name: self.offer.name.clone(),
            size: self.offer.size,
            at_ms: self.at_ms,
        }
    }
}

/// A file's piece hashes, shared while serving.
type HashList = Arc<Vec<[u8; 32]>>;

#[derive(Default)]
pub(crate) struct Transfers {
    store: Option<Arc<dyn LargeFileStore>>,
    partial: Option<PathBuf>,
    pub(crate) outgoing: Vec<Outgoing>,
    pub(crate) incoming: Vec<Incoming>,
    /// Hash lists recently read for serving.
    hash_cache: VecDeque<([u8; 32], HashList)>,
    /// How files recently served are stripped.
    stripped: VecDeque<([u8; 32], Stripping, Instant)>,
    /// Wants being read for, so a requester gets one answer at a time.
    serving: HashSet<(PublicIdentity, [u8; 32])>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hashes_state(root: &[u8; 32]) -> String {
    format!("{HASHES_PREFIX}{}", hex(&root[..16]))
}

fn encode_hashes(h: &[[u8; 32]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + h.len() * 32);
    out.push(VERSION);
    for x in h {
        out.extend_from_slice(x);
    }
    out
}

fn decode_hashes(b: &[u8]) -> Option<Vec<[u8; 32]>> {
    let (&VERSION, body) = b.split_first()? else {
        return None;
    };
    let (chunks, rest) = body.as_chunks::<32>();
    rest.is_empty().then(|| chunks.to_vec())
}

impl Outgoing {
    fn write(
        &self,
        e: &mut const_cbor::Encoder<'_>,
    ) -> core::result::Result<(), const_cbor::Error> {
        e.map_len(9)?;
        e.u8(0)?.bytes(&self.root)?;
        e.u8(1)?.bytes(&self.key)?;
        e.u8(2)?.bytes(&self.conv.to_bytes())?;
        e.u8(3)?.u64(self.local_id)?;
        e.u8(4)?.str(&self.name)?;
        e.u8(5)?.str(&self.location)?;
        e.u8(6)?.u64(self.size)?;
        e.u8(7)?.bool(self.strip)?;
        e.u8(8)?.u64(self.at_ms)?;
        Ok(())
    }

    fn read(d: &mut Decoder<'_>) -> threnody_core::Result<Self> {
        let (
            mut root,
            mut key,
            mut conv,
            mut id,
            mut name,
            mut location,
            mut size,
            mut strip,
            mut at,
        ) = (None, None, None, None, None, None, None, false, 0);
        read_map(d, |k, d| {
            match k {
                0 => root = Some(fixed_bytes::<32>(d)?),
                1 => key = Some(fixed_bytes::<32>(d)?),
                2 => conv = ConversationId::from_bytes(d.bytes()?),
                3 => id = Some(d.u64()?),
                4 => name = Some(d.str()?.to_owned()),
                5 => location = Some(d.str()?.to_owned()),
                6 => size = Some(d.u64()?),
                7 => strip = d.bool()?,
                8 => at = d.u64()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(Self {
            root: required(root, "root")?,
            key: required(key, "key")?,
            conv: required(conv, "conversation")?,
            local_id: required(id, "id")?,
            name: required(name, "name")?,
            location: required(location, "location")?,
            size: required(size, "size")?,
            strip,
            at_ms: at,
        })
    }
}

impl Transfers {
    /// `VERSION || cbor [* Outgoing]`
    pub(crate) fn encode_out(&self) -> threnody_core::Result<Vec<u8>> {
        let body = cbor::to_vec(self.outgoing.len() * 300 + 16, |e| {
            e.array_len(self.outgoing.len())?;
            for o in &self.outgoing {
                o.write(e)?;
            }
            Ok(())
        })?;
        Ok([&[VERSION][..], &body].concat())
    }

    /// `VERSION || cbor [* { 0: conversation, 1: device (32), 2: bstr .cbor Offer,
    /// 3: at_ms, 4: status, ? 5: true (user) }]`
    pub(crate) fn encode_in(&self) -> threnody_core::Result<Vec<u8>> {
        let offers = self
            .incoming
            .iter()
            .map(|i| i.offer.encode())
            .collect::<threnody_core::Result<Vec<_>>>()?;
        let body = cbor::to_vec(
            offers.iter().map(|o| o.len() + 120).sum::<usize>() + 16,
            |e| {
                e.array_len(self.incoming.len())?;
                for (i, o) in self.incoming.iter().zip(&offers) {
                    e.map_len(5 + usize::from(i.user))?;
                    e.u8(0)?.bytes(&i.conv.to_bytes())?;
                    e.u8(1)?.bytes(i.from.as_bytes())?;
                    e.u8(2)?.bytes(o)?;
                    e.u8(3)?.u64(i.at_ms)?;
                    e.u8(4)?.u8(i.status as u8)?;
                    if i.user {
                        e.u8(5)?.bool(true)?;
                    }
                }
                Ok(())
            },
        )?;
        Ok([&[VERSION][..], &body].concat())
    }

    /// The state [`Transfers::encode_out`] and [`Transfers::encode_in`]
    /// kept; damaged state gives empty lists.
    pub(crate) fn decode(out: Option<&[u8]>, inc: Option<&[u8]>) -> Self {
        let mut t = Self::default();
        if let Some((&VERSION, body)) = out.and_then(|b| b.split_first()) {
            let mut dec = Decoder::new(body);
            let mut list = Vec::new();
            let ok = (|| -> threnody_core::Result<()> {
                for _ in 0..dec.array_len()? {
                    let o = Outgoing::read(&mut dec)?;
                    if list.len() < MAX_OUTGOING {
                        list.push(o);
                    }
                }
                Ok(())
            })();
            if ok.is_ok() {
                t.outgoing = list;
            }
        }
        if let Some((&VERSION, body)) = inc.and_then(|b| b.split_first()) {
            let mut dec = Decoder::new(body);
            let mut list = Vec::new();
            let ok = (|| -> threnody_core::Result<()> {
                for _ in 0..dec.array_len()? {
                    let (mut conv, mut from, mut offer, mut at, mut status, mut user) =
                        (None, None, None, 0, 0, false);
                    read_map(&mut dec, |k, d| {
                        match k {
                            0 => conv = ConversationId::from_bytes(d.bytes()?),
                            1 => from = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?).ok(),
                            2 => offer = Some(Offer::decode(d.bytes()?)?),
                            3 => at = d.u64()?,
                            4 => status = d.u8()?,
                            5 => user = d.bool()?,
                            _ => return Ok(false),
                        }
                        Ok(true)
                    })?;
                    if list.len() < MAX_INCOMING {
                        list.push(Incoming {
                            conv: required(conv, "conversation")?,
                            from: required(from, "device")?,
                            offer: required(offer, "offer")?,
                            at_ms: at,
                            status: Status::from_u8(status),
                            user,
                            run: None,
                        });
                    }
                }
                Ok(())
            })();
            if ok.is_ok() {
                t.incoming = list;
            }
        }
        t
    }

    fn find_in(&self, conv: ConversationId, id: u64) -> Option<usize> {
        self.incoming
            .iter()
            .position(|i| i.conv == conv && i.offer.id == id)
    }
}

/// The file at a store location, read in order from the start.
struct StoreReader<'a> {
    store: &'a dyn LargeFileStore,
    location: &'a str,
    pos: u64,
    size: u64,
}

impl Seek for StoreReader<'_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = pos.ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        Ok(self.pos)
    }
}

impl Read for StoreReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let n = buf.len().min(PIECE).min((self.size - self.pos) as usize);
        let data = self
            .store
            .read_at(self.location, self.pos, n)
            .ok_or_else(|| std::io::Error::other("file unreadable"))?;
        if data.is_empty() || data.len() > n {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf[..data.len()].copy_from_slice(&data);
        self.pos += data.len() as u64;
        Ok(data.len())
    }
}

/// How the photo or video at `location`, `size` bytes starting `head`,
/// loses its metadata: a video by edits found from its boxes, whatever
/// its length; a photo stripped whole, up to [`MAX_STRIP_IN_MEMORY`].
/// Anything else goes as it is; a photo whose metadata can't be removed
/// (HEIC, AVIF, TIFF) is refused.
fn stripping(
    store: &dyn LargeFileStore,
    location: &str,
    size: u64,
    head: &[u8],
) -> Result<Stripping> {
    if media::kind(head) == Some(media::Kind::Bmff) {
        let mut r = StoreReader {
            store,
            location,
            pos: 0,
            size,
        };
        let edits = media::video_edits(&mut r, size).map_err(|e| match e {
            threnody_core::Error::Io(_) => NetError::NotFound("file unreadable".into()),
            e => NetError::NotAllowed(format!(
                "this video's location and camera details can't be removed ({e}): \
                 turn metadata removal off to send it"
            )),
        })?;
        return Ok(Stripping::Edits(Arc::new(edits)));
    }
    if size > MAX_STRIP_IN_MEMORY {
        return Err(NetError::NotAllowed(
            "this photo is too large to remove its location and camera details: \
             turn metadata removal off to send it"
                .into(),
        ));
    }
    let raw = read_whole(store, location, size)?;
    match media::strip(raw)? {
        Stripped::Image(d) => Ok(Stripping::Whole(Arc::new(d))),
        Stripped::Unsupported(d) if !media::needs_converting(&d) => {
            Ok(Stripping::Whole(Arc::new(d)))
        }
        Stripped::Unsupported(_) => Err(cant_strip()),
    }
}

fn cant_strip() -> NetError {
    NetError::NotAllowed(
        "this photo's location and camera details can't be removed: \
         send it as a JPEG, or turn metadata removal off"
            .into(),
    )
}

/// The whole file at `location`, `size` bytes.
fn read_whole(store: &dyn LargeFileStore, location: &str, size: u64) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    StoreReader {
        store,
        location,
        pos: 0,
        size,
    }
    .read_to_end(&mut out)
    .map_err(|_| NetError::NotFound("file unreadable".into()))?;
    if out.len() as u64 != size {
        return Err(NetError::NotFound("file changed while reading".into()));
    }
    Ok(out)
}

/// A finished download's partial file, decrypted piece by piece.
struct PieceReader {
    file: std::fs::File,
    key: FileKey,
    size: u64,
    index: u64,
    count: u64,
    buf: Zeroizing<Vec<u8>>,
    pos: usize,
}

impl Read for PieceReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos == self.buf.len() {
            if self.index == self.count {
                return Ok(0);
            }
            let len = tx::piece_len(self.size, self.index).expect("index < count");
            let mut ct = vec![0u8; len + PIECE_OVERHEAD];
            self.file
                .seek(SeekFrom::Start(slot(self.index)))
                .and_then(|_| self.file.read_exact(&mut ct))?;
            self.buf = Zeroizing::new(
                tx::open_piece(&self.key, self.size, self.index, &ct)
                    .map_err(|_| std::io::Error::other("piece doesn't open"))?,
            );
            self.pos = 0;
            self.index += 1;
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Where piece `i` lies in a partial file.
fn slot(i: u64) -> u64 {
    i * (PIECE + PIECE_OVERHEAD) as u64
}

/// Whether a file of `size` called `name` comes by itself: a photo, voice
/// or video message, not sensitive, from a contact, within the limit.
fn wants_auto(offer: &Offer, contact: bool, mobile: bool) -> bool {
    let limit = if mobile {
        AUTO_LIMIT_MOBILE
    } else {
        AUTO_LIMIT
    };
    contact
        && !offer.sensitive
        && (offer.clip.is_some() || media::looks_like_image(&offer.name))
        && offer.size <= limit
}

/// How a file sent without its metadata is read.
#[derive(Clone)]
enum Stripping {
    /// A photo, stripped in memory.
    Whole(Arc<Vec<u8>>),
    /// A video, through the store, edited as it is read.
    Edits(Arc<media::Edits>),
}

/// A [`StoreReader`] that strips a video as it reads.
struct EditedReader<'a> {
    inner: StoreReader<'a>,
    edits: &'a media::Edits,
}

impl Read for EditedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let at = self.inner.pos;
        let n = self.inner.read(buf)?;
        self.edits.apply(at, &mut buf[..n]);
        Ok(n)
    }
}

/// Options for [`Node::send_file_at`], as for an inline file.
#[derive(Clone, Debug, Default)]
pub struct SendOptions {
    pub sensitive: bool,
    pub caption: String,
    pub album: u64,
    pub clip: Option<Clip>,
    pub forwarded: bool,
}

/// Runs `f` off the async runtime if there is one.
fn blocking(f: impl FnOnce() + Send + 'static) {
    match tokio::runtime::Handle::try_current() {
        Ok(h) => {
            h.spawn_blocking(f);
        }
        Err(_) => f(),
    }
}

impl Node {
    /// Installs where the app keeps files for transfers too large for one
    /// message, and from then on tells contacts this node takes them
    /// ([`FEATURE_LARGE_FILES`]). Call before `listen`/`connect`: sessions
    /// already up keep what they said.
    pub fn set_large_file_store(&self, store: Arc<dyn LargeFileStore>) {
        let partial = self.shared.home.dir().join(PARTIAL_DIR);
        {
            let mut t = lock(&self.shared.transfers);
            t.store = Some(store);
            t.partial = Some(partial);
        }
        self.shared
            .features
            .fetch_or(FEATURE_LARGE_FILES, Ordering::Relaxed);
        self.pump_transfers();
    }

    fn large_store(&self) -> Option<Arc<dyn LargeFileStore>> {
        lock(&self.shared.transfers).store.clone()
    }

    /// Sends the file at `location` (read through the large-file store)
    /// to `peer`, recording it in history with that location. One that
    /// fits a message, or for a peer that doesn't take offers, goes
    /// inline; a larger one is offered. Reads the whole file once: call
    /// off the main thread.
    pub fn send_file_at(
        &self,
        peer: &PublicIdentity,
        name: String,
        location: String,
        opts: SendOptions,
    ) -> Result<()> {
        let store = self
            .large_store()
            .ok_or_else(|| NetError::NotAllowed("no file store".into()))?;
        let size = store
            .size(&location)
            .ok_or_else(|| NetError::NotFound("file not found".into()))?;
        let threshold = self.shared.large_threshold.load(Ordering::Relaxed);
        // A peer away since we started is judged by its last session: the
        // offer may go to its mailboxes.
        let large = self.known_to_support(peer, FEATURE_LARGE_FILES);
        let head = store.read_at(&location, 0, 64).unwrap_or_default();
        let strip = self.strip_metadata() && media::kind(&head).is_some();
        if size > threshold && !large {
            return Err(NetError::NotAllowed(
                "their app needs an update to receive a file this large".into(),
            ));
        }
        // Small enough to go inline (stripping never grows a file).
        if size <= threshold {
            let data = read_whole(&*store, &location, size)?;
            return self.send_file(
                peer,
                OutgoingFile {
                    name,
                    data,
                    location: Some(location),
                    sensitive: opts.sensitive,
                    caption: opts.caption,
                    album: opts.album,
                    clip: opts.clip,
                    forwarded: opts.forwarded,
                },
            );
        }
        if size > tx::MAX_OFFERED {
            return Err(NetError::NotAllowed("file too large".into()));
        }
        if self.strip_metadata() && media::needs_converting(&head) {
            return Err(cant_strip());
        }
        let key = FileKey::generate();
        let reader = StoreReader {
            store: &*store,
            location: &location,
            pos: 0,
            size,
        };
        let (stripping, size, (hashes, root)) = if strip {
            match stripping(&*store, &location, size, &head)? {
                Stripping::Whole(data) => {
                    let n = data.len() as u64;
                    let prepared = tx::prepare(&key, n, &data[..])?;
                    (Some(Stripping::Whole(data)), n, prepared)
                }
                Stripping::Edits(edits) => {
                    let edited = EditedReader {
                        inner: reader,
                        edits: &edits,
                    };
                    let prepared = tx::prepare(&key, size, edited)?;
                    (Some(Stripping::Edits(edits)), size, prepared)
                }
            }
        } else {
            (None, size, tx::prepare(&key, size, reader)?)
        };
        let name = self.prepare_name(name, &head);
        self.accept_contact(peer);
        let conv = self.conversation_for(peer);
        let local_id = local_id();
        let at_ms = now_ms();
        let offer = Offer {
            sent_ms: at_ms,
            id: local_id,
            name: name.clone(),
            size,
            root,
            key: *key.as_bytes(),
            sensitive: opts.sensitive,
            forwarded: opts.forwarded,
            clip: opts.clip,
            caption: opts.caption.clone(),
            album: opts.album,
            thumbnail: Vec::new(),
        };
        let bytes = offer.encode()?;
        self.shared
            .save_state(&hashes_state(&root), Ok(encode_hashes(&hashes)));
        {
            let mut t = lock(&self.shared.transfers);
            if let Some(s) = stripping {
                keep_stripped(&mut t, root, s);
            }
            t.outgoing.push(Outgoing {
                root,
                key: *key.as_bytes(),
                conv,
                local_id,
                name: name.clone(),
                location: location.clone(),
                size,
                strip,
                at_ms,
            });
            while t.outgoing.len() > MAX_OUTGOING {
                let o = t.outgoing.remove(0);
                let _ = self.shared.home.remove_state(&hashes_state(&o.root));
            }
            self.save_outgoing(&t);
        }
        let msg = AppMessage::Offer(bytes);
        let tag = Tag {
            local_id,
            ..Tag::NONE
        };
        if let Err(e) = self.send_tagged(peer, msg.clone(), tag) {
            if !matches!(e, NetError::Closed) {
                return Err(e);
            }
            // Away: sealed for its mailboxes, as a text is, so it learns
            // of the file before we next meet; it waits here should every
            // mailbox decline it. The pieces still come from us.
            self.seek(peer);
            if !(self.can_send_offline(peer) && self.send_offline_or_hold(peer, &msg, tag).is_ok())
            {
                self.hold(peer, msg, tag);
            }
        }
        self.append_file(
            peer,
            true,
            FileNote {
                name,
                size,
                location: Some(location),
                sensitive: opts.sensitive,
                album: opts.album,
                clip: opts.clip,
                forwarded: opts.forwarded,
            },
            &opts.caption,
            local_id,
            0,
        );
        Ok(())
    }

    fn save_outgoing(&self, t: &Transfers) {
        self.shared.save_state(STATE_OUT, t.encode_out());
    }

    fn save_incoming(&self, t: &Transfers) {
        self.shared.save_state(STATE_IN, t.encode_in());
    }

    /// Handles an offer from `peer`: recorded in history without its
    /// contents, kept for a download, and started if the auto policy
    /// wants it. True if it was new.
    pub(crate) fn on_file_offer(&self, peer: &PublicIdentity, bytes: &[u8]) -> bool {
        let Ok(offer) = Offer::decode(bytes) else {
            return false;
        };
        if self.large_store().is_none() {
            return false;
        }
        let conv = self.conversation_for(peer);
        let contact = self.is_accepted(peer);
        self.rekey_transfers();
        {
            let t = lock(&self.shared.transfers);
            if t.find_in(conv, offer.id).is_some() {
                return false;
            }
        }
        self.append_file(
            peer,
            false,
            FileNote {
                name: offer.name.clone(),
                size: offer.size,
                location: None,
                sensitive: offer.sensitive,
                album: offer.album,
                clip: offer.clip,
                forwarded: offer.forwarded,
            },
            &offer.caption,
            0,
            offer.id,
        );
        let auto = wants_auto(&offer, contact, self.on_mobile());
        {
            let mut t = lock(&self.shared.transfers);
            if t.incoming.len() >= MAX_INCOMING {
                // The oldest not under way makes room.
                if let Some(i) = t.incoming.iter().position(|i| i.run.is_none()) {
                    let gone = t.incoming.remove(i);
                    self.drop_partial(&t, &gone);
                } else {
                    return true;
                }
            }
            t.incoming.push(Incoming {
                conv,
                from: *peer,
                offer,
                at_ms: now_ms(),
                status: if auto { Status::Wanted } else { Status::Idle },
                user: false,
                run: None,
            });
            self.save_incoming(&t);
        }
        if auto {
            // It may have come through a mailbox: look for its sender.
            self.seek(peer);
            self.pump_transfers();
        }
        true
    }

    /// Where the download of message `id` in `conv` is; None if it isn't
    /// an offer still kept (contents here already, or never offered).
    pub fn transfer_state(&self, conv: ConversationId, id: u64) -> Option<TransferState> {
        self.rekey_transfers();
        let t = lock(&self.shared.transfers);
        t.find_in(conv, id).map(|i| t.incoming[i].state())
    }

    /// Downloads the offered file of message `id` in `conv` (again, after
    /// it ran out of space or failed).
    pub fn download(&self, conv: ConversationId, id: u64) -> Result<TransferState> {
        self.rekey_transfers();
        let from;
        {
            let mut t = lock(&self.shared.transfers);
            let i = t
                .find_in(conv, id)
                .ok_or_else(|| NetError::NotFound("no such file".into()))?;
            let inc = &mut t.incoming[i];
            inc.user = true;
            if inc.status != Status::Wanted {
                inc.status = Status::Wanted;
                inc.run = None;
            }
            from = inc.from;
            self.save_incoming(&t);
        }
        // The pieces come from its sender: reach it if we haven't.
        self.seek(&from);
        self.pump_transfers();
        self.transfer_state(conv, id)
            .ok_or_else(|| NetError::NotFound("no such file".into()))
    }

    /// Stops downloading message `id` in `conv`, deleting what came.
    pub fn cancel_download(&self, conv: ConversationId, id: u64) {
        self.rekey_transfers();
        let mut t = lock(&self.shared.transfers);
        if let Some(i) = t.find_in(conv, id) {
            let inc = &mut t.incoming[i];
            inc.status = Status::Idle;
            inc.user = false;
            inc.run = None;
            let root = inc.offer.root;
            self.remove_partial(&t, &root);
            self.save_incoming(&t);
        }
        drop(t);
        self.pump_transfers();
    }

    fn partial_path(t: &Transfers, root: &[u8; 32]) -> Option<PathBuf> {
        t.partial
            .as_ref()
            .map(|d| d.join(format!("{}.part", hex(&root[..16]))))
    }

    fn remove_partial(&self, t: &Transfers, root: &[u8; 32]) {
        if let Some(p) = Self::partial_path(t, root) {
            let _ = std::fs::remove_file(p);
        }
        let _ = self.shared.home.remove_state(&hashes_state(root));
    }

    fn drop_partial(&self, t: &Transfers, inc: &Incoming) {
        self.remove_partial(t, &inc.offer.root);
    }

    /// A peer's conversation moves from its device to its account once
    /// the account is known (see `conversation_for`), and its history
    /// with it: offers recorded before follow, so they are still found,
    /// served and recorded where their entries are. Cheap: once per peer.
    fn rekey_transfers(&self) {
        let (mut froms, mut convs) = {
            let t = lock(&self.shared.transfers);
            let f: Vec<PublicIdentity> = t.incoming.iter().map(|i| i.from).collect();
            let c: Vec<ConversationId> = t.outgoing.iter().map(|o| o.conv).collect();
            (f, c)
        };
        froms.sort_by_key(|p| *p.as_bytes());
        froms.dedup();
        convs.sort_by_key(ConversationId::to_bytes);
        convs.dedup();
        let ins: HashMap<PublicIdentity, ConversationId> = froms
            .into_iter()
            .map(|p| (p, self.conversation_for(&p)))
            .collect();
        // Ours were recorded under the device they went to, until its
        // account was known.
        let outs: HashMap<ConversationId, ConversationId> = convs
            .into_iter()
            .filter_map(|c| match c {
                ConversationId::Peer(b) => PublicIdentity::from_bytes(&b)
                    .ok()
                    .map(|p| (c, self.conversation_for(&p))),
                ConversationId::Group(_) => None,
            })
            .filter(|(a, b)| a != b)
            .collect();
        let mut t = lock(&self.shared.transfers);
        let mut moved_in = false;
        for i in &mut t.incoming {
            if let Some(c) = ins.get(&i.from)
                && *c != i.conv
            {
                i.conv = *c;
                moved_in = true;
            }
        }
        let mut moved_out = false;
        for o in &mut t.outgoing {
            if let Some(c) = outs.get(&o.conv) {
                o.conv = *c;
                moved_out = true;
            }
        }
        if moved_in {
            self.save_incoming(&t);
        }
        if moved_out {
            self.save_outgoing(&t);
        }
    }

    fn emit_transfer(&self, inc: &Incoming) {
        self.emit(Event::Transfer {
            conv: inc.conv,
            peer: inc.from,
            id: inc.offer.id,
            state: inc.state(),
        });
    }

    /// Starts the downloads that may start: wanted, their sender
    /// reachable, at most [`MAX_ACTIVE`] at once, and only with room.
    pub(crate) fn pump_transfers(&self) {
        let Some(store) = self.large_store() else {
            return;
        };
        let mobile = self.on_mobile();
        let sessions: HashSet<PublicIdentity> =
            self.sessions().into_iter().map(|s| s.peer).collect();
        let mut start = Vec::new();
        {
            let t = lock(&self.shared.transfers);
            let mut active = t.incoming.iter().filter(|i| i.run.is_some()).count();
            for (i, inc) in t.incoming.iter().enumerate() {
                if active >= MAX_ACTIVE {
                    break;
                }
                if inc.status != Status::Wanted || inc.run.is_some() {
                    continue;
                }
                // An automatic download waits for Wi-Fi past the mobile limit.
                if !inc.user && mobile && inc.offer.size > AUTO_LIMIT_MOBILE {
                    continue;
                }
                if !sessions.contains(&inc.from) || !self.supports(&inc.from, FEATURE_LARGE_FILES) {
                    continue;
                }
                start.push(i);
                active += 1;
            }
        }
        if start.is_empty() {
            return;
        }
        // Free space is asked of the app, off the lock.
        let free = store.free_space();
        let mut sends = Vec::new();
        {
            let mut t = lock(&self.shared.transfers);
            let partial = t.partial.clone();
            let mut changed = Vec::new();
            for i in start {
                let Some(inc) = t.incoming.get(i) else {
                    continue;
                };
                let root = inc.offer.root;
                let have_bytes = Self::partial_path(&t, &root)
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len());
                let need = inc
                    .offer
                    .size
                    .saturating_add(tx::piece_count(inc.offer.size) * PIECE_OVERHEAD as u64)
                    .saturating_sub(have_bytes)
                    // Room for both the partial file and the kept copy.
                    .saturating_add(inc.offer.size)
                    .saturating_add(SPACE_MARGIN);
                if free.is_some_and(|f| f < need) {
                    t.incoming[i].status = Status::NoSpace;
                    changed.push(i);
                    continue;
                }
                let Some(dir) = &partial else { continue };
                let hashes = self
                    .shared
                    .home
                    .load_state(self.identity_ref(), &hashes_state(&root))
                    .ok()
                    .flatten()
                    .and_then(|b| decode_hashes(&b))
                    .filter(|h| tx::root(t.incoming[i].offer.size, h) == Some(root));
                let file = open_partial(dir, &root);
                let inc = &mut t.incoming[i];
                let count = tx::piece_count(inc.offer.size);
                let mut run = Run {
                    request: local_id(),
                    hashes: Vec::new(),
                    verified: false,
                    have: vec![false; count as usize],
                    received: 0,
                    inflight: Vec::new(),
                    last: Instant::now(),
                    file,
                };
                if let Some(h) = hashes {
                    run.hashes = h;
                    run.verified = true;
                    scan_partial(&mut run, inc.offer.size);
                }
                let want = next_want(&mut run, inc.offer.root, inc.offer.size);
                inc.run = Some(run);
                changed.push(i);
                if let Some(w) = want {
                    sends.push((inc.from, w));
                }
            }
            for i in changed {
                if let Some(inc) = t.incoming.get(i) {
                    self.emit_transfer(inc);
                }
            }
            self.save_incoming(&t);
        }
        for (to, w) in sends {
            self.send_transfer(&to, &w);
        }
        // A download whose pieces were all here already.
        self.finish_ready();
    }

    fn send_transfer(&self, to: &PublicIdentity, m: &TransferMsg) {
        if let Ok(b) = m.encode() {
            let _ = self.send(to, AppMessage::Transfer(b));
        }
    }

    /// Handles a transfer message from `from`.
    pub(crate) fn on_transfer(&self, from: PublicIdentity, bytes: &[u8]) {
        let Ok(m) = TransferMsg::decode(bytes) else {
            return;
        };
        self.rekey_transfers();
        match m {
            TransferMsg::Want { .. } => self.on_want(from, m),
            TransferMsg::Have { root } => self.on_have(&from, &root),
            TransferMsg::Hashes {
                request,
                first,
                data,
            } => self.on_hashes(&from, request, first, &data),
            TransferMsg::Piece {
                request,
                index,
                data,
            } => self.on_piece(&from, request, index, &data),
            TransferMsg::Missing { request, .. } => self.on_missing(&from, request),
        }
    }

    /// Answers a `Want` from the conversation we offered the file to.
    fn on_want(&self, from: PublicIdentity, want: TransferMsg) {
        let TransferMsg::Want {
            request,
            root,
            first,
            count,
            hashes,
        } = want
        else {
            return;
        };
        let conv = self.conversation_for(&from);
        let missing = move |node: &Node, pieces: Vec<u64>| {
            node.send_transfer(&from, &TransferMsg::Missing { request, pieces });
        };
        let (o, store) = {
            let mut t = lock(&self.shared.transfers);
            let Some(o) = t
                .outgoing
                .iter()
                .find(|o| o.root == root && o.conv == conv)
                .cloned()
            else {
                drop(t);
                return missing(self, Vec::new());
            };
            if !t.serving.insert((from, root)) {
                // Still answering the last one: it asks again on a stall.
                return;
            }
            (o, t.store.clone())
        };
        let Some(store) = store else {
            lock(&self.shared.transfers).serving.remove(&(from, root));
            return missing(self, Vec::new());
        };
        let node = self.clone();
        blocking(move || {
            let answer = node.serve(&*store, &o, request, first, count, hashes);
            lock(&node.shared.transfers).serving.remove(&(from, root));
            match answer {
                Some(msgs) => {
                    for m in msgs {
                        node.send_transfer(&from, &m);
                    }
                }
                None => missing(&node, Vec::new()),
            }
        });
    }

    /// What answers a want for `o`: hash parts, or pieces (and `Missing`
    /// for those that aren't what was offered). None if the file is gone.
    fn serve(
        &self,
        store: &dyn LargeFileStore,
        o: &Outgoing,
        request: u64,
        first: u64,
        count: u64,
        hashes: bool,
    ) -> Option<Vec<TransferMsg>> {
        let list = self.hashes_of(&o.root)?;
        let n = list.len() as u64;
        let mut out = Vec::new();
        if hashes {
            for k in 0..count.min(MAX_WANT) {
                let start = first.checked_add(k * HASHES_PER_PART as u64)?;
                if start >= n {
                    break;
                }
                let end = (start + HASHES_PER_PART as u64).min(n);
                let data: Vec<u8> = list[start as usize..end as usize]
                    .iter()
                    .flatten()
                    .copied()
                    .collect();
                out.push(TransferMsg::Hashes {
                    request,
                    first: start,
                    data,
                });
            }
            if out.is_empty() {
                return None;
            }
            return Some(out);
        }
        let key = FileKey::from_bytes(o.key);
        let stripped = if o.strip {
            Some(self.stripping_for(store, o)?)
        } else {
            None
        };
        let mut missing = Vec::new();
        for i in first..first.saturating_add(count.min(MAX_WANT)).min(n) {
            let len = tx::piece_len(o.size, i)?;
            let start = i * PIECE as u64;
            let plain = match &stripped {
                Some(Stripping::Whole(d)) => d
                    .get(start as usize..start as usize + len)
                    .map(<[u8]>::to_vec),
                Some(Stripping::Edits(e)) => store.read_at(&o.location, start, len).map(|mut p| {
                    e.apply(start, &mut p);
                    p
                }),
                None => store.read_at(&o.location, start, len),
            };
            let piece = plain
                .filter(|p| p.len() == len)
                .and_then(|p| {
                    let p = Zeroizing::new(p);
                    tx::seal_piece(&key, o.size, i, &p).ok()
                })
                // Never a piece other than the one committed to.
                .filter(|p| tx::piece_hash(p) == list[i as usize]);
            match piece {
                Some(data) => out.push(TransferMsg::Piece {
                    request,
                    index: i,
                    data,
                }),
                None => missing.push(i),
            }
        }
        if !missing.is_empty() {
            out.push(TransferMsg::Missing {
                request,
                pieces: missing,
            });
        }
        Some(out)
    }

    fn hashes_of(&self, root: &[u8; 32]) -> Option<HashList> {
        if let Some((_, h)) = lock(&self.shared.transfers)
            .hash_cache
            .iter()
            .find(|(r, _)| r == root)
        {
            return Some(h.clone());
        }
        let h = Arc::new(
            self.shared
                .home
                .load_state(self.identity_ref(), &hashes_state(root))
                .ok()
                .flatten()
                .and_then(|b| decode_hashes(&b))?,
        );
        let mut t = lock(&self.shared.transfers);
        t.hash_cache.retain(|(r, _)| r != root);
        while t.hash_cache.len() >= 4 {
            t.hash_cache.pop_front();
        }
        t.hash_cache.push_back((*root, h.clone()));
        Some(h)
    }

    /// How `o` is stripped, remembered or worked out again. The pieces
    /// served are checked against the hash list, so a file changed since
    /// is never served as the one offered.
    fn stripping_for(&self, store: &dyn LargeFileStore, o: &Outgoing) -> Option<Stripping> {
        {
            let mut t = lock(&self.shared.transfers);
            t.stripped.retain(|(_, _, at)| at.elapsed() < SERVE_TTL);
            if let Some((_, s, _)) = t.stripped.iter().find(|(r, ..)| *r == o.root) {
                return Some(s.clone());
            }
        }
        let size = store.size(&o.location)?;
        let head = store.read_at(&o.location, 0, 64)?;
        let s = stripping(store, &o.location, size, &head).ok()?;
        let n = match &s {
            Stripping::Whole(d) => d.len() as u64,
            Stripping::Edits(_) => size,
        };
        if n != o.size {
            return None;
        }
        keep_stripped(&mut lock(&self.shared.transfers), o.root, s.clone());
        Some(s)
    }

    /// `from` has the whole file: a 1:1 offer is done with.
    fn on_have(&self, from: &PublicIdentity, root: &[u8; 32]) {
        let conv = self.conversation_for(from);
        let mut t = lock(&self.shared.transfers);
        let before = t.outgoing.len();
        t.outgoing.retain(|o| !(o.root == *root && o.conv == conv));
        if t.outgoing.len() != before {
            t.stripped.retain(|(r, ..)| r != root);
            t.hash_cache.retain(|(r, _)| r != root);
            let _ = self.shared.home.remove_state(&hashes_state(root));
            self.save_outgoing(&t);
        }
    }

    /// The download `from` answers under `request`, if any.
    fn running(t: &mut Transfers, from: &PublicIdentity, request: u64) -> Option<usize> {
        t.incoming
            .iter()
            .position(|i| i.from == *from && i.run.as_ref().is_some_and(|r| r.request == request))
    }

    fn on_hashes(&self, from: &PublicIdentity, request: u64, first: u64, data: &[u8]) {
        let mut want = None;
        let mut failed = false;
        {
            let mut t = lock(&self.shared.transfers);
            let Some(i) = Self::running(&mut t, from, request) else {
                return;
            };
            let inc = &mut t.incoming[i];
            let (size, root) = (inc.offer.size, inc.offer.root);
            let run = inc.run.as_mut().expect("running");
            if run.verified || first != run.hashes.len() as u64 {
                return;
            }
            run.last = Instant::now();
            let n = tx::piece_count(size);
            let (chunks, _) = data.as_chunks::<32>();
            for h in chunks {
                if (run.hashes.len() as u64) < n {
                    run.hashes.push(*h);
                }
            }
            run.inflight.retain(|x| *x != first);
            if run.hashes.len() as u64 == n {
                if tx::root(size, &run.hashes) == Some(root) {
                    run.verified = true;
                    self.shared
                        .save_state(&hashes_state(&root), Ok(encode_hashes(&run.hashes)));
                    want = next_want(run, root, size);
                } else {
                    failed = true;
                }
            } else if run.inflight.is_empty() {
                want = next_want(run, root, size);
            }
            if failed {
                inc.status = Status::Failed;
                inc.run = None;
                let gone_root = inc.offer.root;
                self.emit_transfer(&t.incoming[i]);
                self.remove_partial(&t, &gone_root);
                self.save_incoming(&t);
            }
        }
        if let Some(w) = want {
            self.send_transfer(from, &w);
        }
        if failed {
            self.pump_transfers();
        }
    }

    fn on_piece(&self, from: &PublicIdentity, request: u64, index: u64, data: &[u8]) {
        let mut want = None;
        let mut done = false;
        {
            let mut t = lock(&self.shared.transfers);
            let Some(i) = Self::running(&mut t, from, request) else {
                return;
            };
            let inc = &mut t.incoming[i];
            let (size, root) = (inc.offer.size, inc.offer.root);
            let run = inc.run.as_mut().expect("running");
            if !run.verified
                || !run.inflight.contains(&index)
                || tx::piece_hash(data) != run.hashes[index as usize]
            {
                return;
            }
            run.last = Instant::now();
            run.inflight.retain(|x| *x != index);
            let wrote = run.file.as_mut().is_some_and(|f| {
                f.seek(SeekFrom::Start(slot(index)))
                    .and_then(|_| f.write_all(data))
                    .is_ok()
            });
            if !wrote {
                // A full disk, most likely: try again later.
                inc.status = Status::NoSpace;
                inc.run = None;
                self.emit_transfer(&t.incoming[i]);
                self.save_incoming(&t);
                return;
            }
            if !run.have[index as usize] {
                run.have[index as usize] = true;
                run.received += (data.len() - PIECE_OVERHEAD) as u64;
            }
            if run.inflight.is_empty() {
                want = next_want(run, root, size);
                done = want.is_none();
                self.emit_transfer(&t.incoming[i]);
            }
        }
        if let Some(w) = want {
            self.send_transfer(from, &w);
        }
        if done {
            self.finish_ready();
        }
    }

    /// The source doesn't have the file (or pieces of it).
    fn on_missing(&self, from: &PublicIdentity, request: u64) {
        let mut t = lock(&self.shared.transfers);
        let Some(i) = Self::running(&mut t, from, request) else {
            return;
        };
        let inc = &mut t.incoming[i];
        inc.status = Status::Unavailable;
        inc.run = None;
        self.emit_transfer(&t.incoming[i]);
        self.save_incoming(&t);
        drop(t);
        self.pump_transfers();
    }

    /// Keeps every download whose pieces are all in: decrypted into the
    /// app's store, recorded in history, and the sender told.
    fn finish_ready(&self) {
        self.rekey_transfers();
        let ready: Vec<(usize, ConversationId, u64)> = {
            let t = lock(&self.shared.transfers);
            t.incoming
                .iter()
                .enumerate()
                .filter(|(_, i)| {
                    i.run
                        .as_ref()
                        .is_some_and(|r| r.verified && r.have.iter().all(|h| *h))
                })
                .map(|(n, i)| (n, i.conv, i.offer.id))
                .collect()
        };
        for (_, conv, id) in ready {
            let (inc, store, path) = {
                let mut t = lock(&self.shared.transfers);
                let Some(i) = t.find_in(conv, id) else {
                    continue;
                };
                let Some(store) = t.store.clone() else {
                    continue;
                };
                let Some(path) = Self::partial_path(&t, &t.incoming[i].offer.root) else {
                    continue;
                };
                // Out of the list first, so it can't finish twice.
                let inc = t.incoming.remove(i);
                self.save_incoming(&t);
                (inc, store, path)
            };
            let node = self.clone();
            blocking(move || node.keep_download(inc, &*store, &path));
        }
    }

    fn keep_download(&self, inc: Incoming, store: &dyn LargeFileStore, path: &std::path::Path) {
        let offer = &inc.offer;
        let kept = std::fs::File::open(path).ok().and_then(|file| {
            let mut r = PieceReader {
                file,
                key: offer.file_key(),
                size: offer.size,
                index: 0,
                count: tx::piece_count(offer.size),
                buf: Zeroizing::new(Vec::new()),
                pos: 0,
            };
            store.keep_stream(&offer.name, offer.clip.is_some(), &mut r)
        });
        let t = lock(&self.shared.transfers);
        self.remove_partial(&t, &offer.root);
        drop(t);
        match kept {
            Some(loc) => {
                let _ = self.shared.home.set_file_location(
                    self.identity_ref(),
                    inc.conv,
                    &inc.file_ref(),
                    &loc,
                    now_ms(),
                );
                self.send_transfer(&inc.from, &TransferMsg::Have { root: offer.root });
                self.emit(Event::Transfer {
                    conv: inc.conv,
                    peer: inc.from,
                    id: offer.id,
                    state: TransferState::Done,
                });
                self.emit(Event::DeviceSync { from: inc.from });
            }
            None => {
                // Couldn't be kept (no room, or the store failed): back in
                // the list, to try again.
                let mut t = lock(&self.shared.transfers);
                let mut inc = inc;
                inc.status = Status::Failed;
                inc.run = None;
                self.emit_transfer(&inc);
                t.incoming.push(inc);
                self.save_incoming(&t);
            }
        }
        self.pump_transfers();
    }

    /// A session with `peer` came up: its downloads may go on.
    pub(crate) fn transfers_hello(&self, peer: &PublicIdentity) {
        let waiting = lock(&self.shared.transfers)
            .incoming
            .iter()
            .any(|i| i.from == *peer && i.status == Status::Wanted && i.run.is_none());
        if waiting {
            self.pump_transfers();
        }
    }

    /// Every minute: asks again where a download went quiet or its sender
    /// went away, and drops what is no longer needed (entries deleted or
    /// expired, offers past [`SERVE_DAYS`]).
    pub(crate) fn sweep_transfers(&self) {
        let sessions: HashSet<PublicIdentity> =
            self.sessions().into_iter().map(|s| s.peer).collect();
        let mut sends = Vec::new();
        {
            let mut t = lock(&self.shared.transfers);
            for inc in t.incoming.iter_mut() {
                let Some(run) = inc.run.as_mut() else {
                    continue;
                };
                if !sessions.contains(&inc.from) {
                    inc.run = None;
                } else if run.last.elapsed() >= STALL {
                    run.request = local_id();
                    run.inflight.clear();
                    run.last = Instant::now();
                    if let Some(w) = next_want(run, inc.offer.root, inc.offer.size) {
                        sends.push((inc.from, w));
                    }
                }
            }
        }
        for (to, w) in sends {
            self.send_transfer(&to, &w);
        }
        self.drop_stale_transfers();
        self.pump_transfers();
    }

    /// Drops offers whose history entry is gone, and ours past
    /// [`SERVE_DAYS`].
    fn drop_stale_transfers(&self) {
        self.rekey_transfers();
        let (convs, now) = {
            let t = lock(&self.shared.transfers);
            let mut c: Vec<ConversationId> = t
                .incoming
                .iter()
                .map(|i| i.conv)
                .chain(t.outgoing.iter().map(|o| o.conv))
                .collect();
            c.sort_by_key(|c| c.to_bytes());
            c.dedup();
            (c, now_ms())
        };
        if convs.is_empty() {
            return;
        }
        let me = *self.identity().as_bytes();
        let mut entries: HashMap<ConversationId, Vec<threnody_core::history::Entry>> =
            HashMap::new();
        for c in convs {
            if let Ok(h) = self.history(c) {
                entries.insert(c, h.entries().to_vec());
            }
        }
        let has = |conv: &ConversationId, r: &FileRef| {
            entries.get(conv).is_some_and(|es| {
                es.iter()
                    .any(|e| threnody_core::history::file_matches(r, e))
            })
        };
        let mut t = lock(&self.shared.transfers);
        let gone_in: Vec<[u8; 32]> = t
            .incoming
            .iter()
            .filter(|i| entries.contains_key(&i.conv) && !has(&i.conv, &i.file_ref()))
            .map(|i| i.offer.root)
            .collect();
        let gone_out: Vec<[u8; 32]> = t
            .outgoing
            .iter()
            .filter(|o| {
                o.at_ms + SERVE_DAYS * DAY_MS < now
                    || (entries.contains_key(&o.conv)
                        && !has(
                            &o.conv,
                            &FileRef {
                                device: me,
                                id: o.local_id,
                                name: o.name.clone(),
                                size: o.size,
                                at_ms: o.at_ms,
                            },
                        ))
            })
            .map(|o| o.root)
            .collect();
        if gone_in.is_empty() && gone_out.is_empty() {
            return;
        }
        for r in gone_in.iter().chain(&gone_out) {
            self.remove_partial(&t, r);
        }
        t.incoming.retain(|i| !gone_in.contains(&i.offer.root));
        t.outgoing.retain(|o| !gone_out.contains(&o.root));
        t.stripped.retain(|(r, ..)| !gone_out.contains(r));
        t.hash_cache.retain(|(r, _)| !gone_out.contains(r));
        self.save_incoming(&t);
        self.save_outgoing(&t);
    }

    /// How many files we offered are still served (not yet downloaded,
    /// deleted or expired).
    pub fn files_served(&self) -> usize {
        lock(&self.shared.transfers).outgoing.len()
    }

    /// For tests: files over `bytes` are offered rather than sent inline.
    #[doc(hidden)]
    pub fn set_large_threshold(&self, bytes: u64) {
        self.shared.large_threshold.store(bytes, Ordering::Relaxed);
    }
}

fn keep_stripped(t: &mut Transfers, root: [u8; 32], d: Stripping) {
    t.stripped.retain(|(r, ..)| *r != root);
    while t.stripped.len() >= SERVE_CACHE {
        t.stripped.pop_front();
    }
    t.stripped.push_back((root, d, Instant::now()));
}

/// The partial file for `root` in `dir` (made, owner-only, if need be).
fn open_partial(dir: &std::path::Path, root: &[u8; 32]) -> Option<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .ok()?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).ok()?;
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(dir.join(format!("{}.part", hex(&root[..16])))).ok()
}

/// Marks the pieces already in `run`'s partial file (each checked
/// against its hash), as after a restart.
fn scan_partial(run: &mut Run, size: u64) {
    let Some(f) = run.file.as_mut() else { return };
    let mut buf = vec![0u8; PIECE + PIECE_OVERHEAD];
    for i in 0..tx::piece_count(size) {
        let len = tx::piece_len(size, i).expect("i < count") + PIECE_OVERHEAD;
        let ok = f
            .seek(SeekFrom::Start(slot(i)))
            .and_then(|_| f.read_exact(&mut buf[..len]))
            .is_ok()
            && tx::piece_hash(&buf[..len]) == run.hashes[i as usize];
        if ok {
            run.have[i as usize] = true;
            run.received += (len - PIECE_OVERHEAD) as u64;
        }
    }
}

/// The next want of `run`: hash-list parts until the list is in, then up
/// to [`WINDOW`] pieces from the first missing one. None when all is in.
fn next_want(run: &mut Run, root: [u8; 32], size: u64) -> Option<TransferMsg> {
    if !run.verified {
        let first = run.hashes.len() as u64;
        run.inflight = vec![first];
        return Some(TransferMsg::Want {
            request: run.request,
            root,
            first,
            count: 1,
            hashes: true,
        });
    }
    let first = run.have.iter().position(|h| !h)? as u64;
    let n = tx::piece_count(size);
    let mut count = 0;
    while count < WINDOW && first + count < n && !run.have[(first + count) as usize] {
        count += 1;
    }
    run.inflight = (first..first + count).collect();
    Some(TransferMsg::Want {
        request: run.request,
        root,
        first,
        count,
        hashes: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let me = threnody_core::Identity::generate().public();
        let offer = Offer {
            sent_ms: 1,
            id: 2,
            name: "a.mkv".into(),
            size: 3 * PIECE as u64,
            root: [4; 32],
            key: [5; 32],
            sensitive: false,
            forwarded: false,
            clip: None,
            caption: String::new(),
            album: 0,
            thumbnail: Vec::new(),
        };
        let t = Transfers {
            outgoing: vec![Outgoing {
                root: [1; 32],
                key: [2; 32],
                conv: ConversationId::Peer([3; 32]),
                local_id: 4,
                name: "b.iso".into(),
                location: "/x/b.iso".into(),
                size: 5,
                strip: true,
                at_ms: 6,
            }],
            incoming: vec![Incoming {
                conv: ConversationId::Peer([7; 32]),
                from: me,
                offer: offer.clone(),
                at_ms: 8,
                status: Status::NoSpace,
                user: true,
                run: None,
            }],
            ..Transfers::default()
        };
        let back = Transfers::decode(
            Some(&t.encode_out().unwrap()),
            Some(&t.encode_in().unwrap()),
        );
        assert_eq!(back.outgoing, t.outgoing);
        assert_eq!(back.incoming.len(), 1);
        let i = &back.incoming[0];
        assert_eq!(
            (i.from, &i.offer, i.status, i.user),
            (me, &offer, Status::NoSpace, true)
        );
        // Damage gives empty lists, never a panic.
        let d = Transfers::decode(Some(&[1, 0x9f]), Some(&[9]));
        assert!(d.outgoing.is_empty() && d.incoming.is_empty());
    }

    #[test]
    fn hashes_round_trip() {
        let h = vec![[1u8; 32], [2; 32]];
        assert_eq!(decode_hashes(&encode_hashes(&h)), Some(h));
        assert_eq!(decode_hashes(&[VERSION, 1, 2]), None);
        assert_eq!(decode_hashes(&[]), None);
    }

    #[test]
    fn wants_walk_the_file() {
        let size = 20 * PIECE as u64 + 1;
        let n = tx::piece_count(size) as usize;
        let mut run = Run {
            request: 1,
            hashes: Vec::new(),
            verified: false,
            have: vec![false; n],
            received: 0,
            inflight: Vec::new(),
            last: Instant::now(),
            file: None,
        };
        let w = next_want(&mut run, [0; 32], size).unwrap();
        assert!(matches!(
            w,
            TransferMsg::Want {
                hashes: true,
                first: 0,
                count: 1,
                ..
            }
        ));
        run.verified = true;
        let w = next_want(&mut run, [0; 32], size).unwrap();
        assert!(matches!(
            w,
            TransferMsg::Want {
                hashes: false,
                first: 0,
                count: WINDOW,
                ..
            }
        ));
        run.have[..8].fill(true);
        run.have[10] = true;
        let w = next_want(&mut run, [0; 32], size).unwrap();
        assert!(matches!(
            w,
            TransferMsg::Want {
                first: 8,
                count: 2,
                ..
            }
        ));
        run.have.iter_mut().for_each(|h| *h = true);
        assert!(next_want(&mut run, [0; 32], size).is_none());
    }

    #[test]
    fn a_partial_file_is_picked_up_where_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let key = FileKey::generate();
        let size = 3 * PIECE as u64 + 7;
        let data: Vec<u8> = (0..size).map(|i| (i % 253) as u8).collect();
        let (hashes, root) = tx::prepare(&key, size, &data[..]).unwrap();
        let mut f = open_partial(dir.path(), &root).unwrap();
        for i in [0u64, 3] {
            let start = (i * PIECE as u64) as usize;
            let end = (start + PIECE).min(data.len());
            let p = tx::seal_piece(&key, size, i, &data[start..end]).unwrap();
            f.seek(SeekFrom::Start(slot(i))).unwrap();
            f.write_all(&p).unwrap();
        }
        // A damaged piece doesn't count.
        f.seek(SeekFrom::Start(slot(1))).unwrap();
        f.write_all(&[1; 100]).unwrap();
        let mut run = Run {
            request: 1,
            hashes,
            verified: true,
            have: vec![false; 4],
            received: 0,
            inflight: Vec::new(),
            last: Instant::now(),
            file: open_partial(dir.path(), &root),
        };
        scan_partial(&mut run, size);
        assert_eq!(run.have, [true, false, false, true]);
        assert_eq!(run.received, PIECE as u64 + 7);
        let w = next_want(&mut run, root, size).unwrap();
        assert!(matches!(
            w,
            TransferMsg::Want {
                first: 1,
                count: 2,
                ..
            }
        ));
    }

    #[test]
    fn auto_policy() {
        let mut o = Offer {
            sent_ms: 1,
            id: 2,
            name: "clip.jpg".into(),
            size: 10_000_000,
            root: [0; 32],
            key: [0; 32],
            sensitive: false,
            forwarded: false,
            clip: None,
            caption: String::new(),
            album: 0,
            thumbnail: Vec::new(),
        };
        assert!(wants_auto(&o, true, false));
        assert!(!wants_auto(&o, true, true), "over the mobile limit");
        assert!(!wants_auto(&o, false, false), "a message request");
        o.sensitive = true;
        assert!(!wants_auto(&o, true, false));
        o.sensitive = false;
        o.name = "notes.zip".into();
        assert!(!wants_auto(&o, true, false), "not a photo or clip");
    }
}
