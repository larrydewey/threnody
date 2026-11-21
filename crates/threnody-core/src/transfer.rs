//! Files too large for one message (`docs/appendix-u-large-files.md`).
//! Codecs and piece cryptography only; the transfers themselves are in
//! `threnody-net`.
//!
//! A sender offers a file ([`Offer`]: name, size, `root`, a fresh file key)
//! and the receiver pulls it in pieces of [`PIECE`] bytes ([`TransferMsg`]).
//! Each piece is encrypted under the file key, and the hashes of the
//! encrypted pieces are committed to by `root`, so any party holding the
//! file serves byte-identical pieces and each piece is checked on its own:
//!
//! ```text
//! piece_key   = KDF(FILE_PIECE_KEY, K)
//! nonce_key   = KDF(FILE_PIECE_NONCE, K)
//! nonce_i     = BLAKE3-keyed(nonce_key, u64be(i) || plaintext_i)[..24]
//! piece_i     = nonce_i || XChaCha20-Poly1305(piece_key, nonce_i, plaintext_i,
//!                                             aad = u64be(size) || u64be(i))
//! hash_i      = BLAKE3(piece_i)
//! root        = BLAKE3-derive_key(FILE_ROOT, u64le(size) || u64le(PIECE) || hash_0 || ... || hash_n)
//! ```
//!
//! The nonce depends on the plaintext, so a source whose copy changed
//! can't reuse a nonce for different bytes: it makes a different piece,
//! whose hash then fails. Sources still never send a piece whose hash
//! isn't the committed one.
//!
//! ```text
//! Offer    = { 0: sent_ms uint, 1: id uint, 2: name tstr (≤ 255 bytes), 3: size uint,
//!              4: root (32), 5: key (32), 6: piece size uint (262144),
//!              ? 7: flags uint (File key 6), ? 8: duration_ms uint, ? 9: caption tstr,
//!              ? 10: album uint, ? 11: thumbnail bstr (≤ 24 KiB) }
//! Transfer = { 0: op uint, ... }
//!   op 1 Want    { 1: request uint, 2: root (32), 3: first uint, 4: count uint (1..=16),
//!                  ? 5: true (hashes rather than pieces) }
//!   op 2 Piece   { 1: request uint, 2: index uint, 3: data bstr (≤ 262144 + 40) }
//!   op 3 Hashes  { 1: request uint, 2: first uint, 3: data bstr (n × 32, 1 ≤ n ≤ 8192) }
//!   op 4 Missing { 1: request uint, 2: bstr (n × u64be piece indexes, n ≤ 16) }
//!   op 5 Have    { 2: root (32) }
//! ```
//!
//! A `Want` for pieces asks for `count` pieces from `first`; one for
//! hashes asks for `count` parts of the hash list, each of
//! [`HASHES_PER_PART`] hashes, the first starting at hash `first`. A
//! `Missing` with no indexes means the source has none of the file.
//! Unknown keys are skipped everywhere, so every map can grow.

use std::io::Read;

use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::kdf;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::message::{Clip, FILE_SENSITIVE, FILE_VIDEO, FILE_VOICE, FORWARDED};

/// Plaintext bytes per piece. Every piece but the last is this long.
pub const PIECE: usize = 256 * 1024;
/// Random nonce at the start of every piece.
pub const NONCE_LEN: usize = 24;
/// Poly1305 tag at the end of every piece.
pub const TAG_LEN: usize = 16;
/// What encryption adds to a piece.
pub const PIECE_OVERHEAD: usize = NONCE_LEN + TAG_LEN;
/// Largest file an offer may name. Its hash list is then 8 MiB.
pub const MAX_OFFERED: u64 = 64 * 1024 * 1024 * 1024;
/// Most pieces (or hash-list parts) one `Want` asks for.
pub const MAX_WANT: u64 = 16;
/// Hashes in one `Hashes` part (256 KiB of them).
pub const HASHES_PER_PART: usize = 8192;
/// Longest file name an offer carries, in bytes.
pub const MAX_NAME: usize = 255;
/// Largest thumbnail an offer carries (a JPEG, as for link previews).
pub const MAX_THUMBNAIL: usize = 24 * 1024;
/// Largest encoded offer: offers are small enough for any mailbox.
pub const MAX_OFFER_BYTES: usize = 96 * 1024;

const OP_WANT: u64 = 1;
const OP_PIECE: u64 = 2;
const OP_HASHES: u64 = 3;
const OP_MISSING: u64 = 4;
const OP_HAVE: u64 = 5;

/// The random key a file is offered under. Fresh for every send, so two
/// sends of the same file share nothing.
#[derive(Clone)]
pub struct FileKey(Zeroizing<[u8; 32]>);

impl FileKey {
    pub fn generate() -> Self {
        Self(Zeroizing::new(random_bytes()))
    }

    pub fn from_bytes(b: [u8; 32]) -> Self {
        Self(Zeroizing::new(b))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn piece_cipher(&self) -> XChaCha20Poly1305 {
        let k: Zeroizing<[u8; 32]> =
            Zeroizing::new(kdf::derive(kdf::label::FILE_PIECE_KEY, &[&self.0[..]]));
        XChaCha20Poly1305::new((&*k).into())
    }

    fn nonce_key(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(kdf::derive(kdf::label::FILE_PIECE_NONCE, &[&self.0[..]]))
    }
}

impl std::fmt::Debug for FileKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileKey(..)")
    }
}

/// How many pieces a file of `size` bytes has: at least one, so an empty
/// file is one empty piece.
pub fn piece_count(size: u64) -> u64 {
    size.div_ceil(PIECE as u64).max(1)
}

/// The plaintext length of piece `index` of a file of `size` bytes, or
/// None past the last piece.
pub fn piece_len(size: u64, index: u64) -> Option<usize> {
    if index >= piece_count(size) {
        return None;
    }
    let start = index * PIECE as u64;
    Some((size - start).min(PIECE as u64) as usize)
}

fn aad(size: u64, index: u64) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&size.to_be_bytes());
    a[8..].copy_from_slice(&index.to_be_bytes());
    a
}

/// Encrypts piece `index` of a file of `size` bytes. `plaintext` must be
/// exactly that piece's length.
pub fn seal_piece(key: &FileKey, size: u64, index: u64, plaintext: &[u8]) -> Result<Vec<u8>> {
    if piece_len(size, index) != Some(plaintext.len()) {
        return Err(Error::Malformed("file piece has the wrong length"));
    }
    let mut h = blake3::Hasher::new_keyed(&key.nonce_key());
    h.update(&index.to_be_bytes());
    h.update(plaintext);
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&h.finalize().as_bytes()[..NONCE_LEN]);
    let ct = key
        .piece_cipher()
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: plaintext,
                aad: &aad(size, index),
            },
        )
        .map_err(|_| Error::Malformed("file piece too large"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypts piece `index` of a file of `size` bytes. Check the piece
/// against its hash first ([`piece_hash`]): the key alone doesn't stop
/// another holder of it from making a piece up.
pub fn open_piece(key: &FileKey, size: u64, index: u64, piece: &[u8]) -> Result<Vec<u8>> {
    let len = piece_len(size, index).ok_or(Error::Malformed("file piece past the end"))?;
    if piece.len() != len + PIECE_OVERHEAD {
        return Err(Error::Malformed("file piece has the wrong length"));
    }
    let (nonce, ct) = piece.split_at(NONCE_LEN);
    let nonce: &[u8; NONCE_LEN] = nonce.try_into().expect("split at NONCE_LEN");
    key.piece_cipher()
        .decrypt(
            nonce.into(),
            Payload {
                msg: ct,
                aad: &aad(size, index),
            },
        )
        .map_err(|_| Error::Decrypt)
}

/// The hash a piece is committed to by.
pub fn piece_hash(piece: &[u8]) -> [u8; 32] {
    *blake3::hash(piece).as_bytes()
}

/// The root over a file's piece hashes. None unless there is exactly one
/// hash per piece.
pub fn root(size: u64, hashes: &[[u8; 32]]) -> Option<[u8; 32]> {
    if size > MAX_OFFERED || hashes.len() as u64 != piece_count(size) {
        return None;
    }
    let mut h = blake3::Hasher::new_derive_key(kdf::label::FILE_ROOT);
    h.update(&size.to_le_bytes());
    h.update(&(PIECE as u64).to_le_bytes());
    for x in hashes {
        h.update(x);
    }
    Some(*h.finalize().as_bytes())
}

/// Reads exactly `size` bytes from `src`, encrypting them piece by piece
/// under `key`, and returns the piece hashes and the root. Only one piece
/// is in memory at a time.
pub fn prepare(key: &FileKey, size: u64, mut src: impl Read) -> Result<(Vec<[u8; 32]>, [u8; 32])> {
    if size > MAX_OFFERED {
        return Err(Error::Malformed("file too large to offer"));
    }
    let count = piece_count(size);
    let mut hashes = Vec::with_capacity(count as usize);
    let mut buf = Zeroizing::new(vec![0u8; PIECE]);
    for i in 0..count {
        let len = piece_len(size, i).expect("i < count");
        src.read_exact(&mut buf[..len])?;
        hashes.push(piece_hash(&seal_piece(key, size, i, &buf[..len])?));
    }
    let mut extra = [0u8; 1];
    if src.read(&mut extra)? != 0 {
        return Err(Error::Malformed("file longer than its size"));
    }
    let r = root(size, &hashes).expect("one hash per piece");
    Ok((hashes, r))
}

/// A file offered in place of sending it inline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub sent_ms: u64,
    /// The sender's id for the message (0 = none), as for `File`.
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub root: [u8; 32],
    pub key: [u8; 32],
    pub sensitive: bool,
    pub forwarded: bool,
    pub clip: Option<Clip>,
    pub caption: String,
    pub album: u64,
    /// A JPEG of at most [`MAX_THUMBNAIL`] bytes, or empty.
    pub thumbnail: Vec<u8>,
}

/// `s` cut to at most `max` bytes, at a char boundary.
fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

impl Offer {
    pub fn file_key(&self) -> FileKey {
        FileKey::from_bytes(self.key)
    }

    fn flags(&self) -> u64 {
        let mut f = 0;
        if self.sensitive {
            f |= FILE_SENSITIVE;
        }
        if self.forwarded {
            f |= FORWARDED;
        }
        if let Some(c) = self.clip {
            f |= c.flag();
        }
        f
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let name = truncate(self.name.clone(), MAX_NAME);
        let flags = self.flags();
        let duration = self.clip.map_or(0, |c| c.duration_ms);
        let thumb = if self.thumbnail.len() <= MAX_THUMBNAIL {
            &self.thumbnail[..]
        } else {
            &[]
        };
        let out = cbor::to_vec(name.len() + self.caption.len() + thumb.len() + 160, |e| {
            e.map_len(
                7 + usize::from(flags != 0)
                    + usize::from(duration != 0)
                    + usize::from(!self.caption.is_empty())
                    + usize::from(self.album != 0)
                    + usize::from(!thumb.is_empty()),
            )?;
            e.u8(0)?.u64(self.sent_ms)?;
            e.u8(1)?.u64(self.id)?;
            e.u8(2)?.str(&name)?;
            e.u8(3)?.u64(self.size)?;
            e.u8(4)?.bytes(&self.root)?;
            e.u8(5)?.bytes(&self.key)?;
            e.u8(6)?.u64(PIECE as u64)?;
            if flags != 0 {
                e.u8(7)?.u64(flags)?;
            }
            if duration != 0 {
                e.u8(8)?.u32(duration)?;
            }
            if !self.caption.is_empty() {
                e.u8(9)?.str(&self.caption)?;
            }
            if self.album != 0 {
                e.u8(10)?.u64(self.album)?;
            }
            if !thumb.is_empty() {
                e.u8(11)?.bytes(thumb)?;
            }
            Ok(())
        })?;
        if out.len() > MAX_OFFER_BYTES {
            return Err(Error::Malformed("offer too large"));
        }
        Ok(out)
    }

    /// Decodes an offer. One over [`MAX_OFFER_BYTES`], naming a file over
    /// [`MAX_OFFERED`] or another piece size, or with both clip bits, is
    /// malformed. A thumbnail over [`MAX_THUMBNAIL`] is dropped.
    pub fn decode(b: &[u8]) -> Result<Self> {
        if b.len() > MAX_OFFER_BYTES {
            return Err(Error::Malformed("offer too large"));
        }
        let mut dec = Decoder::new(b);
        let (mut sent_ms, mut id, mut name, mut size, mut root, mut key, mut piece) =
            (None, None, None, None, None, None, None);
        let (mut flags, mut duration, mut caption, mut album, mut thumbnail) =
            (0u64, 0u32, String::new(), 0u64, Vec::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => sent_ms = Some(d.u64()?),
                1 => id = Some(d.u64()?),
                2 => name = Some(truncate(d.str()?.to_owned(), MAX_NAME)),
                3 => size = Some(d.u64()?),
                4 => root = Some(fixed_bytes::<32>(d)?),
                5 => key = Some(fixed_bytes::<32>(d)?),
                6 => piece = Some(d.u64()?),
                7 => flags = d.u64()?,
                8 => duration = d.u32()?,
                9 => caption = d.str()?.to_owned(),
                10 => album = d.u64()?,
                11 => {
                    let t = d.bytes()?;
                    if t.len() <= MAX_THUMBNAIL {
                        thumbnail = t.to_vec();
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let size = required(size, "offer size")?;
        if size > MAX_OFFERED {
            return Err(Error::Malformed("offered file too large"));
        }
        if required(piece, "offer piece size")? != PIECE as u64 {
            return Err(Error::Malformed("offer piece size"));
        }
        Ok(Self {
            sent_ms: required(sent_ms, "offer time")?,
            id: required(id, "offer id")?,
            name: required(name, "offer name")?,
            size,
            root: required(root, "offer root")?,
            key: required(key, "offer key")?,
            sensitive: flags & FILE_SENSITIVE != 0,
            forwarded: flags & FORWARDED != 0,
            clip: Clip::from_flags(flags & (FILE_VOICE | FILE_VIDEO), duration)?,
            caption,
            album,
            thumbnail,
        })
    }
}

/// What a receiver and the sources it pulls from tell each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferMsg {
    /// Asks for `count` pieces from `first`, or with `hashes`, for `count`
    /// parts of the hash list starting at hash `first`.
    Want {
        /// Random; what answers it carries it back.
        request: u64,
        root: [u8; 32],
        first: u64,
        count: u64,
        hashes: bool,
    },
    /// Piece `index`, encrypted, as committed to by its hash.
    Piece {
        request: u64,
        index: u64,
        data: Vec<u8>,
    },
    /// Hashes from `first` on: `data` is n × 32 bytes.
    Hashes {
        request: u64,
        first: u64,
        data: Vec<u8>,
    },
    /// The source doesn't have these pieces (none listed: it has nothing
    /// of the file).
    Missing { request: u64, pieces: Vec<u64> },
    /// The sender of this has the whole file.
    Have { root: [u8; 32] },
}

impl TransferMsg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Want {
                request,
                root,
                first,
                count,
                hashes,
            } => cbor::to_vec(96, |e| {
                e.map_len(5 + usize::from(*hashes))?;
                e.u8(0)?.u64(OP_WANT)?;
                e.u8(1)?.u64(*request)?;
                e.u8(2)?.bytes(root)?;
                e.u8(3)?.u64(*first)?;
                e.u8(4)?.u64(*count)?;
                if *hashes {
                    e.u8(5)?.bool(true)?;
                }
                Ok(())
            }),
            Self::Piece {
                request,
                index,
                data,
            } => cbor::to_vec(data.len() + 48, |e| {
                e.map_len(4)?;
                e.u8(0)?.u64(OP_PIECE)?;
                e.u8(1)?.u64(*request)?;
                e.u8(2)?.u64(*index)?;
                e.u8(3)?.bytes(data)?;
                Ok(())
            }),
            Self::Hashes {
                request,
                first,
                data,
            } => cbor::to_vec(data.len() + 48, |e| {
                e.map_len(4)?;
                e.u8(0)?.u64(OP_HASHES)?;
                e.u8(1)?.u64(*request)?;
                e.u8(2)?.u64(*first)?;
                e.u8(3)?.bytes(data)?;
                Ok(())
            }),
            Self::Missing { request, pieces } => {
                let packed: Vec<u8> = pieces.iter().flat_map(|i| i.to_be_bytes()).collect();
                cbor::to_vec(packed.len() + 32, |e| {
                    e.map_len(3)?;
                    e.u8(0)?.u64(OP_MISSING)?;
                    e.u8(1)?.u64(*request)?;
                    e.u8(2)?.bytes(&packed)?;
                    Ok(())
                })
            }
            Self::Have { root } => cbor::to_vec(48, |e| {
                e.map_len(2)?;
                e.u8(0)?.u64(OP_HAVE)?;
                e.u8(2)?.bytes(root)?;
                Ok(())
            }),
        }
    }

    /// Decodes a transfer message. Pieces, hash parts or `Missing` lists
    /// over their bounds, a `Want` for no or too many, and unknown ops are
    /// malformed.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut request, mut root, mut first, mut count, mut hashes, mut data) =
            (None, None, None, None, None, false, None);
        let mut index = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u64()?),
                1 => request = Some(d.u64()?),
                2 => match op {
                    // Key 2 is the index of a piece, the first hash of a
                    // part, the root of a want or have, or a missing list.
                    Some(OP_PIECE) => index = Some(d.u64()?),
                    Some(OP_HASHES) => first = Some(d.u64()?),
                    Some(OP_MISSING) => {
                        let b = d.bytes()?;
                        if b.len() % 8 != 0 || b.len() / 8 > MAX_WANT as usize {
                            return Err(Error::Malformed("missing list"));
                        }
                        data = Some(b.to_vec());
                    }
                    Some(OP_WANT | OP_HAVE) => root = Some(fixed_bytes::<32>(d)?),
                    _ => return Err(Error::Malformed("transfer op must come first")),
                },
                3 => match op {
                    Some(OP_WANT) => first = Some(d.u64()?),
                    Some(OP_PIECE) => {
                        let b = d.bytes()?;
                        if b.len() > PIECE + PIECE_OVERHEAD {
                            return Err(Error::Malformed("file piece too large"));
                        }
                        data = Some(b.to_vec());
                    }
                    Some(OP_HASHES) => {
                        let b = d.bytes()?;
                        if b.is_empty() || b.len() % 32 != 0 || b.len() / 32 > HASHES_PER_PART {
                            return Err(Error::Malformed("hash list part"));
                        }
                        data = Some(b.to_vec());
                    }
                    _ => return Ok(false),
                },
                4 => count = Some(d.u64()?),
                5 => hashes = d.bool()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        match required(op, "transfer op")? {
            OP_WANT => {
                let count = required(count, "want count")?;
                if count == 0 || count > MAX_WANT {
                    return Err(Error::Malformed("want count"));
                }
                Ok(Self::Want {
                    request: required(request, "request")?,
                    root: required(root, "want root")?,
                    first: required(first, "want first")?,
                    count,
                    hashes,
                })
            }
            OP_PIECE => Ok(Self::Piece {
                request: required(request, "request")?,
                index: required(index, "piece index")?,
                data: required(data, "piece data")?,
            }),
            OP_HASHES => Ok(Self::Hashes {
                request: required(request, "request")?,
                first: required(first, "hashes first")?,
                data: required(data, "hashes data")?,
            }),
            OP_MISSING => Ok(Self::Missing {
                request: required(request, "request")?,
                pieces: data
                    .unwrap_or_default()
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| u64::from_be_bytes(*c))
                    .collect(),
            }),
            OP_HAVE => Ok(Self::Have {
                root: required(root, "have root")?,
            }),
            _ => Err(Error::Malformed("unknown transfer op")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer() -> Offer {
        Offer {
            sent_ms: 1_700_000_000_000,
            id: 42,
            name: "holiday.mp4".into(),
            size: 3 * PIECE as u64 + 5,
            root: [7; 32],
            key: [9; 32],
            sensitive: true,
            forwarded: false,
            clip: Some(Clip {
                video: true,
                duration_ms: 61_000,
            }),
            caption: "the boat".into(),
            album: 77,
            thumbnail: vec![0xFF, 0xD8, 0xFF, 1, 2, 3],
        }
    }

    #[test]
    fn offer_round_trips() {
        let o = offer();
        assert_eq!(Offer::decode(&o.encode().unwrap()).unwrap(), o);
        let plain = Offer {
            sensitive: false,
            clip: None,
            caption: String::new(),
            album: 0,
            thumbnail: Vec::new(),
            ..offer()
        };
        assert_eq!(Offer::decode(&plain.encode().unwrap()).unwrap(), plain);
    }

    #[test]
    fn offer_bounds() {
        let big = Offer {
            size: MAX_OFFERED + 1,
            ..offer()
        };
        assert!(Offer::decode(&big.encode().unwrap()).is_err());
        let max = Offer {
            size: MAX_OFFERED,
            ..offer()
        };
        assert!(Offer::decode(&max.encode().unwrap()).is_ok());
        // An oversized thumbnail is left out, not an error.
        let thumb = Offer {
            thumbnail: vec![1; MAX_THUMBNAIL + 1],
            ..offer()
        };
        assert!(
            Offer::decode(&thumb.encode().unwrap())
                .unwrap()
                .thumbnail
                .is_empty()
        );
        let long = Offer {
            name: "é".repeat(200),
            ..offer()
        };
        let back = Offer::decode(&long.encode().unwrap()).unwrap();
        assert!(back.name.len() <= MAX_NAME);
        let huge = Offer {
            caption: "x".repeat(MAX_OFFER_BYTES),
            ..offer()
        };
        assert!(huge.encode().is_err());
    }

    #[test]
    fn offer_rejects_another_piece_size() {
        let mut b = offer().encode().unwrap();
        // Key 6 is encoded as 0x06 then 0x1A 0x00 0x04 0x00 0x00 (262144).
        let at = b
            .windows(6)
            .position(|w| w == [0x06, 0x1A, 0x00, 0x04, 0x00, 0x00])
            .unwrap();
        b[at + 3] = 0x08;
        assert!(Offer::decode(&b).is_err());
    }

    #[test]
    fn transfer_round_trips() {
        let all = [
            TransferMsg::Want {
                request: 1,
                root: [3; 32],
                first: 10,
                count: 16,
                hashes: false,
            },
            TransferMsg::Want {
                request: 2,
                root: [3; 32],
                first: 0,
                count: 1,
                hashes: true,
            },
            TransferMsg::Piece {
                request: 3,
                index: 9,
                data: vec![4; PIECE + PIECE_OVERHEAD],
            },
            TransferMsg::Hashes {
                request: 4,
                first: 8192,
                data: vec![5; 32 * HASHES_PER_PART],
            },
            TransferMsg::Missing {
                request: 5,
                pieces: vec![1, 2, u64::MAX],
            },
            TransferMsg::Missing {
                request: 6,
                pieces: Vec::new(),
            },
            TransferMsg::Have { root: [6; 32] },
        ];
        for m in all {
            assert_eq!(TransferMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn transfer_bounds() {
        let bad = [
            TransferMsg::Want {
                request: 1,
                root: [3; 32],
                first: 0,
                count: 0,
                hashes: false,
            },
            TransferMsg::Want {
                request: 1,
                root: [3; 32],
                first: 0,
                count: MAX_WANT + 1,
                hashes: false,
            },
            TransferMsg::Piece {
                request: 3,
                index: 0,
                data: vec![4; PIECE + PIECE_OVERHEAD + 1],
            },
            TransferMsg::Hashes {
                request: 4,
                first: 0,
                data: vec![5; 32 * HASHES_PER_PART + 32],
            },
            TransferMsg::Hashes {
                request: 4,
                first: 0,
                data: vec![5; 31],
            },
            TransferMsg::Hashes {
                request: 4,
                first: 0,
                data: Vec::new(),
            },
            TransferMsg::Missing {
                request: 5,
                pieces: vec![0; MAX_WANT as usize + 1],
            },
        ];
        for m in bad {
            assert!(TransferMsg::decode(&m.encode().unwrap()).is_err(), "{m:?}");
        }
        // An unknown op.
        let b = cbor::to_vec(16, |e| {
            e.map_len(1)?.u8(0)?.u64(99)?;
            Ok(())
        })
        .unwrap();
        assert!(TransferMsg::decode(&b).is_err());
    }

    #[test]
    fn piece_counts() {
        let p = PIECE as u64;
        assert_eq!(piece_count(0), 1);
        assert_eq!(piece_len(0, 0), Some(0));
        assert_eq!(piece_len(0, 1), None);
        assert_eq!(piece_count(p), 1);
        assert_eq!(piece_count(p + 1), 2);
        assert_eq!(piece_len(p + 1, 1), Some(1));
        assert_eq!(piece_count(MAX_OFFERED), MAX_OFFERED / p);
    }

    #[test]
    fn pieces_seal_and_open() {
        let key = FileKey::generate();
        let size = PIECE as u64 + 10;
        let a = vec![1u8; PIECE];
        let b = vec![2u8; 10];
        let pa = seal_piece(&key, size, 0, &a).unwrap();
        let pb = seal_piece(&key, size, 1, &b).unwrap();
        assert_eq!(pa.len(), PIECE + PIECE_OVERHEAD);
        // Deterministic: any holder makes the same piece.
        assert_eq!(pa, seal_piece(&key, size, 0, &a).unwrap());
        assert_eq!(open_piece(&key, size, 0, &pa).unwrap(), a);
        assert_eq!(open_piece(&key, size, 1, &pb).unwrap(), b);
        // Wrong place, wrong size, wrong key, tampering.
        assert!(open_piece(&key, size, 1, &pa).is_err());
        assert!(open_piece(&key, size + 1, 1, &pb).is_err());
        assert!(open_piece(&FileKey::generate(), size, 0, &pa).is_err());
        let mut t = pb.clone();
        t[30] ^= 1;
        assert!(open_piece(&key, size, 1, &t).is_err());
        // Wrong plaintext length to seal.
        assert!(seal_piece(&key, size, 1, &a).is_err());
        // A changed plaintext gets a different nonce, never the same one.
        let b2 = vec![3u8; 10];
        assert_ne!(
            seal_piece(&key, size, 1, &b2).unwrap()[..NONCE_LEN],
            pb[..NONCE_LEN]
        );
    }

    #[test]
    fn prepare_commits_to_every_piece() {
        let key = FileKey::generate();
        let data: Vec<u8> = (0..2 * PIECE + 3).map(|i| i as u8).collect();
        let size = data.len() as u64;
        let (hashes, r) = prepare(&key, size, &data[..]).unwrap();
        assert_eq!(hashes.len(), 3);
        assert_eq!(root(size, &hashes), Some(r));
        for (i, h) in hashes.iter().enumerate() {
            let start = i * PIECE;
            let end = (start + PIECE).min(data.len());
            let p = seal_piece(&key, size, i as u64, &data[start..end]).unwrap();
            assert_eq!(&piece_hash(&p), h);
        }
        // Another key, another root, though the file is the same.
        assert_ne!(prepare(&FileKey::generate(), size, &data[..]).unwrap().1, r);
        // The size is bound into the root.
        assert_ne!(root(size + 1, &hashes), Some(r));
        // Too short, too long.
        assert!(prepare(&key, size + 1, &data[..]).is_err());
        assert!(prepare(&key, size - 1, &data[..]).is_err());
        // An empty file is one empty piece.
        let (h0, r0) = prepare(&key, 0, &[][..]).unwrap();
        assert_eq!(h0.len(), 1);
        assert_eq!(root(0, &h0), Some(r0));
    }
}
