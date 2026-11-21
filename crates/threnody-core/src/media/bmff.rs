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
//!
//! Some cameras also record the place as it changes, as samples of a
//! timed metadata track beside the picture and sound: Apple's `mebx`
//! location, GoPro's GPMF, Google's `camm`, drones' subtitle tracks. The
//! samples of every track that isn't video or audio are zeroed where
//! they lie in `mdat`, found through that track's sample tables. A
//! fragmented file (`moof`) with such a track is refused, since its
//! samples are described elsewhere.
//!
//! Since nothing moves, what changes is a list of [`Edits`]: found by
//! reading the top-level boxes' headers and the few boxes that hold
//! metadata or describe the samples, never the media itself, so a video
//! of any length is stripped as it is read.

use std::io::{Read, Seek, SeekFrom};

use crate::{Error, Result};

const BAD: Error = Error::Malformed("video");

/// How deep boxes are followed. Everything removed is at most three
/// levels in (`moov` > `trak` > `mdia` > `mdhd`); a bound keeps a file of
/// nested boxes from exhausting the stack.
const MAX_DEPTH: usize = 4;

/// The largest top-level box read whole to find what to change: the
/// movie box, which holds every track's sample tables (a few megabytes
/// for an hour of video). The media itself is never read.
pub const MAX_HELD: u64 = 64 * 1024 * 1024;

/// Track handlers whose samples are kept: picture and sound.
const KEPT_HANDLERS: [&[u8; 4]; 2] = [b"vide", b"soun"];

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

/// What stripping a video changes, by where in the file: bytes replaced,
/// then ranges zeroed. Applied to any range of the original as it is
/// read, it gives that range of the stripped video, which is the same
/// length.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Edits {
    /// Bytes written at an offset, in order, none overlapping.
    patches: Vec<(u64, Vec<u8>)>,
    /// `start..end` ranges zeroed, in order, none overlapping or
    /// touching.
    zeros: Vec<(u64, u64)>,
}

impl Edits {
    /// Turns `buf`, the bytes of the original from `offset`, into those
    /// of the stripped video.
    pub fn apply(&self, offset: u64, buf: &mut [u8]) {
        let end = offset.saturating_add(buf.len() as u64);
        let first = self
            .patches
            .partition_point(|(at, b)| at + b.len() as u64 <= offset);
        for (at, bytes) in &self.patches[first..] {
            if *at >= end {
                break;
            }
            let (from, to) = ((*at).max(offset), (at + bytes.len() as u64).min(end));
            buf[(from - offset) as usize..(to - offset) as usize]
                .copy_from_slice(&bytes[(from - at) as usize..(to - at) as usize]);
        }
        let first = self.zeros.partition_point(|&(_, e)| e <= offset);
        for &(start, stop) in &self.zeros[first..] {
            if start >= end {
                break;
            }
            let (from, to) = (start.max(offset), stop.min(end));
            buf[(from - offset) as usize..(to - offset) as usize].fill(0);
        }
    }

    /// Whether nothing changes.
    pub fn is_empty(&self) -> bool {
        self.patches.is_empty() && self.zeros.is_empty()
    }
}

/// `d` without its metadata, the same length. A file without a movie
/// box or with boxes that don't fit is an error.
pub(super) fn strip(mut d: Vec<u8>) -> Result<Vec<u8>> {
    let edits = plan(&mut std::io::Cursor::new(&d[..]), d.len() as u64)?;
    edits.apply(0, &mut d);
    Ok(d)
}

/// What stripping the video `r`, `len` bytes long, changes. Reads the
/// top-level boxes' headers, and whole only the movie box and any other
/// box with headers in it; never the media. A box to read that is larger
/// than [`MAX_HELD`] is an error.
pub(super) fn plan<R: Read + Seek>(r: &mut R, len: u64) -> Result<Edits> {
    let mut top = Vec::new();
    let mut at = 0;
    while at < len {
        let mut head = [0u8; 16];
        let n = usize::try_from((len - at).min(16)).map_err(|_| BAD)?;
        read_at(r, at, &mut head[..n])?;
        let (ty, size_len, size) = header(&head[..n], len - at)?;
        let b = Top {
            ty,
            at,
            body: at + size_len,
            end: at + size,
        };
        at = b.end;
        top.push(b);
    }
    let fragmented = top.iter().any(|b| &b.ty == b"moof");
    let mut edits = Edits::default();
    let mut zeros = Vec::new();
    let mut movie = false;
    for b in &top {
        match &b.ty {
            b"udta" | b"meta" => blank_top(&mut edits, &mut zeros, b),
            b"uuid" => {
                // As in a held box, the 16 bytes after the header,
                // wherever the box ends.
                let mut id = [0u8; 16];
                if b.body + 16 <= len {
                    read_at(r, b.body, &mut id)?;
                    if id == XMP_UUID {
                        blank_top(&mut edits, &mut zeros, b);
                    }
                }
            }
            b"moov" | b"trak" | b"mdia" | b"mvhd" | b"tkhd" | b"mdhd" => {
                let size = b.end - b.at;
                if size > MAX_HELD {
                    return Err(Error::Malformed("video: movie box too large"));
                }
                let mut d = vec![0; usize::try_from(size).map_err(|_| BAD)?];
                read_at(r, b.at, &mut d)?;
                let before = d.clone();
                let whole = d.len();
                walk(&mut d, 0, whole, 0, &mut movie)?;
                if &b.ty == b"moov" {
                    zeros.extend(timed_metadata(&d, fragmented, len)?);
                }
                if d != before {
                    edits.patches.push((b.at, d));
                }
            }
            _ => {}
        }
    }
    if !movie {
        return Err(BAD);
    }
    zeros.retain(|&(_, n)| n > 0);
    zeros.sort_unstable();
    for (start, n) in zeros {
        let end = start + n;
        match edits.zeros.last_mut() {
            Some((_, last)) if start <= *last => *last = (*last).max(end),
            _ => edits.zeros.push((start, end)),
        }
    }
    edits.patches.sort_unstable_by_key(|(at, _)| *at);
    Ok(edits)
}

/// A top-level box, where in the file.
struct Top {
    ty: [u8; 4],
    at: u64,
    body: u64,
    end: u64,
}

/// A top-level box made `free` and emptied, as [`blank`] does.
fn blank_top(edits: &mut Edits, zeros: &mut Vec<(u64, u64)>, b: &Top) {
    edits.patches.push((b.at + 4, b"free".to_vec()));
    zeros.push((b.body, b.end - b.body));
}

/// Fills `buf` from `at` in `r`; a file shorter than that is damaged.
fn read_at<R: Read + Seek>(r: &mut R, at: u64, buf: &mut [u8]) -> Result<()> {
    r.seek(SeekFrom::Start(at))?;
    r.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => BAD,
        _ => Error::Io(e),
    })
}

/// The boxes directly in `from..to`.
fn children(d: &[u8], from: usize, to: usize) -> Result<Vec<BoxAt>> {
    let mut out = Vec::new();
    let mut at = from;
    while at < to {
        let b = box_at(d, at, to)?;
        at = b.end;
        out.push(b);
    }
    Ok(out)
}

/// The first box of type `ty` directly in `b`.
fn child(d: &[u8], b: &BoxAt, ty: &[u8; 4]) -> Result<Option<BoxAt>> {
    Ok(children(d, b.body, b.end)?
        .into_iter()
        .find(|c| &c.ty == ty))
}

/// Where the samples of every track in the movie box `d` that isn't
/// video or audio lie in the file, `len` bytes long: `(offset, length)`.
fn timed_metadata(d: &[u8], fragmented: bool, len: u64) -> Result<Vec<(u64, u64)>> {
    let mut clear = Vec::new();
    for moov in children(d, 0, d.len())?.iter().filter(|b| &b.ty == b"moov") {
        for trak in children(d, moov.body, moov.end)?
            .iter()
            .filter(|b| &b.ty == b"trak")
        {
            let Some(mdia) = child(d, trak, b"mdia")? else {
                continue;
            };
            let handler = match child(d, &mdia, b"hdlr")? {
                // Version and flags, then QuickTime's component type.
                Some(h) => d.get(h.body + 8..h.body + 12).ok_or(BAD)?,
                None => &[0; 4],
            };
            if KEPT_HANDLERS.iter().any(|k| &k[..] == handler) {
                continue;
            }
            if fragmented {
                return Err(BAD);
            }
            let stbl = child(d, &mdia, b"minf")?
                .map(|minf| child(d, &minf, b"stbl"))
                .transpose()?
                .flatten();
            // No sample table: no samples.
            if let Some(stbl) = stbl {
                clear.extend(samples(d, &stbl, len)?);
            }
        }
    }
    Ok(clear)
}

/// Where each sample of the track whose sample table is `stbl` lies in
/// the file, `len` bytes long: `(offset, length)`, every one inside it.
fn samples(whole: &[u8], stbl: &BoxAt, len: u64) -> Result<Vec<(u64, u64)>> {
    let mut sizes: Option<Vec<u64>> = None;
    let mut chunks: Option<Vec<u64>> = None;
    let mut runs: Option<Vec<(u32, u32)>> = None;
    for b in children(whole, stbl.body, stbl.end)? {
        // A table's entries end where its box does.
        let d = &whole[..b.end];
        let u32_at = |o: usize| -> Result<u32> {
            Ok(u32::from_be_bytes(
                d.get(o..o + 4).ok_or(BAD)?.try_into().map_err(|_| BAD)?,
            ))
        };
        // Every table starts with version and flags, then a count.
        let count = |b: &BoxAt| -> Result<usize> {
            let n = usize::try_from(u32_at(b.body + 4)?).map_err(|_| BAD)?;
            // No table holds more entries than its box has bytes.
            if n > b.end - b.body {
                return Err(BAD);
            }
            Ok(n)
        };
        match &b.ty {
            b"stsz" => {
                let fixed = u64::from(u32_at(b.body + 4)?);
                let n = usize::try_from(u32_at(b.body + 8)?).map_err(|_| BAD)?;
                sizes = Some(if let Some(most) = len.checked_div(fixed) {
                    // One size for all: bounded by the file, as each
                    // sample must fit in it.
                    if n as u64 > most {
                        return Err(BAD);
                    }
                    vec![fixed; n]
                } else {
                    if n > (b.end - b.body) / 4 {
                        return Err(BAD);
                    }
                    (0..n)
                        .map(|i| Ok(u64::from(u32_at(b.body + 12 + 4 * i)?)))
                        .collect::<Result<_>>()?
                });
            }
            b"stz2" => {
                // Version and flags, 3 reserved bytes and the field size,
                // then the count.
                let field = *d.get(b.body + 7).ok_or(BAD)?;
                let n = usize::try_from(u32_at(b.body + 8)?).map_err(|_| BAD)?;
                if n > 2 * (b.end - b.body) {
                    return Err(BAD);
                }
                let at = b.body + 12;
                sizes = Some(
                    (0..n)
                        .map(|i| {
                            Ok(match field {
                                4 => {
                                    let byte = *d.get(at + i / 2).ok_or(BAD)?;
                                    u64::from(if i % 2 == 0 { byte >> 4 } else { byte & 15 })
                                }
                                8 => u64::from(*d.get(at + i).ok_or(BAD)?),
                                16 => u64::from(u16::from_be_bytes(
                                    d.get(at + 2 * i..at + 2 * i + 2)
                                        .ok_or(BAD)?
                                        .try_into()
                                        .map_err(|_| BAD)?,
                                )),
                                _ => return Err(BAD),
                            })
                        })
                        .collect::<Result<_>>()?,
                );
            }
            b"stco" => {
                let n = count(&b)?;
                chunks = Some(
                    (0..n)
                        .map(|i| Ok(u64::from(u32_at(b.body + 8 + 4 * i)?)))
                        .collect::<Result<_>>()?,
                );
            }
            b"co64" => {
                let n = count(&b)?;
                chunks = Some(
                    (0..n)
                        .map(|i| {
                            let o = b.body + 8 + 8 * i;
                            Ok(u64::from_be_bytes(
                                d.get(o..o + 8).ok_or(BAD)?.try_into().map_err(|_| BAD)?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                );
            }
            b"stsc" => {
                let n = count(&b)?;
                runs = Some(
                    (0..n)
                        .map(|i| {
                            let o = b.body + 8 + 12 * i;
                            Ok((u32_at(o)?, u32_at(o + 4)?))
                        })
                        .collect::<Result<_>>()?,
                );
            }
            _ => {}
        }
    }
    let sizes = sizes.unwrap_or_default();
    if sizes.is_empty() {
        return Ok(Vec::new());
    }
    // Samples without a chunk to say where they are: rather not send it.
    let (Some(chunks), Some(runs)) = (chunks, runs) else {
        return Err(BAD);
    };
    let mut out = Vec::with_capacity(sizes.len());
    let mut next = sizes.iter();
    for (i, &start) in chunks.iter().enumerate() {
        let number = u32::try_from(i + 1).map_err(|_| BAD)?;
        // The last run starting at or before this chunk says how many
        // samples it holds.
        let per = runs
            .iter()
            .rev()
            .find(|(first, _)| *first <= number)
            .map_or(0, |(_, n)| *n);
        let mut at = start;
        for _ in 0..per {
            let Some(&n) = next.next() else { break };
            let end = at.checked_add(n).ok_or(BAD)?;
            if end > len {
                return Err(BAD);
            }
            out.push((at, n));
            at = end;
        }
    }
    // Every sample must have been found.
    if next.next().is_some() {
        return Err(BAD);
    }
    Ok(out)
}

/// A box in a held part of the file: its type, where its contents start
/// and where it ends.
struct BoxAt {
    ty: [u8; 4],
    at: usize,
    body: usize,
    end: usize,
}

/// The type, header length and size of the box whose header starts
/// `h`, which must end within `room` bytes.
fn header(h: &[u8], room: u64) -> Result<([u8; 4], u64, u64)> {
    let word =
        |o: usize| -> Result<[u8; 4]> { h.get(o..o + 4).ok_or(BAD)?.try_into().map_err(|_| BAD) };
    let ty = word(4)?;
    let (head, size) = match u32::from_be_bytes(word(0)?) {
        // A 64-bit size follows the type.
        1 => {
            let large: [u8; 8] = h.get(8..16).ok_or(BAD)?.try_into().map_err(|_| BAD)?;
            (16, u64::from_be_bytes(large))
        }
        // To the end of what holds it.
        0 => (8, room),
        n => (8, u64::from(n)),
    };
    if size < head || size > room {
        return Err(BAD);
    }
    Ok((ty, head, size))
}

/// The box at `at`, which must end by `limit`.
fn box_at(d: &[u8], at: usize, limit: usize) -> Result<BoxAt> {
    let (ty, head, size) = header(d.get(at..limit).ok_or(BAD)?, (limit - at) as u64)?;
    // Both fit in `limit - at`.
    let (head, size) = (head as usize, size as usize);
    Ok(BoxAt {
        ty,
        at,
        body: at + head,
        end: at + size,
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
