//! Metadata in ISO base media files: MP4, MOV, M4A, 3GP.
//!
//! Phones put the place a video was taken (`©xyz` in `udta`, an ISO 6709
//! string under `meta` keys such as `com.apple.quicktime.location.ISO6709`),
//! the make and model, and XMP into boxes beside the media, and the time
//! into the movie, track and media headers. Those boxes become `free`
//! boxes of the same size, and the times become zero, so nothing moves:
//! the chunk offsets in `stco`/`co64` still point at the same samples
//! whether `moov` comes before `mdat` or after it, and nothing is
//! re-encoded.

use crate::{Error, Result};

const BAD: Error = Error::Malformed("video");

/// How deep boxes are followed. Everything removed is at most three
/// levels in (`moov` > `trak` > `mdia` > `mdhd`); a bound keeps a file of
/// nested boxes from exhausting the stack.
const MAX_DEPTH: usize = 4;

/// The `uuid` box Adobe tools write XMP into.
const XMP_UUID: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];

/// Whether `d` starts with a box an ISO base media file can start with:
/// `ftyp`, or for old QuickTime movies, which have none, the movie or
/// its data or padding.
pub(super) fn is_bmff(d: &[u8]) -> bool {
    d.len() >= 8
        && [b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip"]
            .iter()
            .any(|t| &d[4..8] == *t)
}

/// `d` without its metadata, the same length. A file without a movie
/// box or with boxes that don't fit is an error.
pub(super) fn strip(mut d: Vec<u8>) -> Result<Vec<u8>> {
    let mut movie = false;
    let len = d.len();
    walk(&mut d, 0, len, 0, &mut movie)?;
    if !movie {
        return Err(BAD);
    }
    Ok(d)
}

/// A box in the file: its type, where its contents start and where it
/// ends.
struct BoxAt {
    ty: [u8; 4],
    at: usize,
    body: usize,
    end: usize,
}

/// The box at `at`, which must end by `limit`.
fn box_at(d: &[u8], at: usize, limit: usize) -> Result<BoxAt> {
    let word =
        |o: usize| -> Result<[u8; 4]> { d.get(o..o + 4).ok_or(BAD)?.try_into().map_err(|_| BAD) };
    let ty = word(at + 4)?;
    let (head, size) = match u32::from_be_bytes(word(at)?) {
        // A 64-bit size follows the type.
        1 => {
            let large: [u8; 8] = d
                .get(at + 8..at + 16)
                .ok_or(BAD)?
                .try_into()
                .map_err(|_| BAD)?;
            (
                16,
                usize::try_from(u64::from_be_bytes(large)).map_err(|_| BAD)?,
            )
        }
        // To the end of what holds it.
        0 => (8, limit - at),
        n => (8, usize::try_from(n).map_err(|_| BAD)?),
    };
    let end = at.checked_add(size).ok_or(BAD)?;
    if size < head || end > limit {
        return Err(BAD);
    }
    Ok(BoxAt {
        ty,
        at,
        body: at + head,
        end,
    })
}

/// Clears the metadata among the boxes from `from` to `to`.
fn walk(d: &mut [u8], from: usize, to: usize, depth: usize, movie: &mut bool) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(BAD);
    }
    let mut at = from;
    while at < to {
        let b = box_at(d, at, to)?;
        match &b.ty {
            b"udta" | b"meta" => blank(d, &b),
            b"uuid" if d.get(b.body..b.body + 16) == Some(&XMP_UUID[..]) => blank(d, &b),
            b"moov" | b"trak" | b"mdia" => {
                *movie |= depth == 0 && &b.ty == b"moov";
                walk(d, b.body, b.end, depth + 1, movie)?;
            }
            b"mvhd" | b"tkhd" | b"mdhd" => zero_times(d, &b)?,
            _ => {}
        }
        at = b.end;
    }
    Ok(())
}

/// Makes `b` a `free` box of the same size with nothing in it, so that
/// nothing after it moves.
fn blank(d: &mut [u8], b: &BoxAt) {
    d[b.at + 4..b.at + 8].copy_from_slice(b"free");
    d[b.body..b.end].fill(0);
}

/// Zeroes the creation and modification times at the start of a movie,
/// track or media header: 32-bit in version 0, 64-bit in version 1.
fn zero_times(d: &mut [u8], b: &BoxAt) -> Result<()> {
    let width = match d.get(b.body) {
        Some(0) => 4,
        Some(1) => 8,
        // Somewhere we don't know: rather not send it.
        _ => return Err(BAD),
    };
    let times = b.body + 4..b.body + 4 + 2 * width;
    if times.end > b.end {
        return Err(BAD);
    }
    d[times].fill(0);
    Ok(())
}
