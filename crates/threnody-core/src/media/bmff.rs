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

use crate::{Error, Result};

const BAD: Error = Error::Malformed("video");

/// How deep boxes are followed. Everything removed is at most three
/// levels in (`moov` > `trak` > `mdia` > `mdhd`); a bound keeps a file of
/// nested boxes from exhausting the stack.
const MAX_DEPTH: usize = 4;

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

/// `d` without its metadata, the same length. A file without a movie
/// box or with boxes that don't fit is an error.
pub(super) fn strip(mut d: Vec<u8>) -> Result<Vec<u8>> {
    let mut movie = false;
    let len = d.len();
    walk(&mut d, 0, len, 0, &mut movie)?;
    if !movie {
        return Err(BAD);
    }
    clear_timed_metadata(&mut d)?;
    Ok(d)
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

/// Zeroes the samples of every track that isn't video or audio.
fn clear_timed_metadata(d: &mut [u8]) -> Result<()> {
    let top = children(d, 0, d.len())?;
    let fragmented = top.iter().any(|b| &b.ty == b"moof");
    let mut clear = Vec::new();
    for moov in top.iter().filter(|b| &b.ty == b"moov") {
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
                clear.extend(samples(d, &stbl)?);
            }
        }
    }
    for (at, len) in clear {
        d.get_mut(at..at + len).ok_or(BAD)?.fill(0);
    }
    Ok(())
}

/// Where each sample of the track whose sample table is `stbl` lies:
/// `(offset, length)`, every one inside the file.
fn samples(d: &[u8], stbl: &BoxAt) -> Result<Vec<(usize, usize)>> {
    let u32_at = |o: usize| -> Result<u32> {
        Ok(u32::from_be_bytes(
            d.get(o..o + 4).ok_or(BAD)?.try_into().map_err(|_| BAD)?,
        ))
    };
    let mut sizes: Option<Vec<usize>> = None;
    let mut chunks: Option<Vec<usize>> = None;
    let mut runs: Option<Vec<(u32, u32)>> = None;
    for b in children(d, stbl.body, stbl.end)? {
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
                let fixed = u32_at(b.body + 4)? as usize;
                let n = usize::try_from(u32_at(b.body + 8)?).map_err(|_| BAD)?;
                sizes = Some(if fixed != 0 {
                    // One size for all: bounded by the file, as each
                    // sample must fit in it.
                    if n > d.len() / fixed.max(1) {
                        return Err(BAD);
                    }
                    vec![fixed; n]
                } else {
                    if n > (b.end - b.body) / 4 {
                        return Err(BAD);
                    }
                    (0..n)
                        .map(|i| Ok(u32_at(b.body + 12 + 4 * i)? as usize))
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
                                    usize::from(if i % 2 == 0 { byte >> 4 } else { byte & 15 })
                                }
                                8 => usize::from(*d.get(at + i).ok_or(BAD)?),
                                16 => usize::from(u16::from_be_bytes(
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
                        .map(|i| Ok(u32_at(b.body + 8 + 4 * i)? as usize))
                        .collect::<Result<_>>()?,
                );
            }
            b"co64" => {
                let n = count(&b)?;
                chunks = Some(
                    (0..n)
                        .map(|i| {
                            let o = b.body + 8 + 8 * i;
                            let v = u64::from_be_bytes(
                                d.get(o..o + 8).ok_or(BAD)?.try_into().map_err(|_| BAD)?,
                            );
                            usize::try_from(v).map_err(|_| BAD)
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
            let Some(&len) = next.next() else { break };
            let end = at.checked_add(len).ok_or(BAD)?;
            if end > d.len() {
                return Err(BAD);
            }
            out.push((at, len));
            at = end;
        }
    }
    // Every sample must have been found.
    if next.next().is_some() {
        return Err(BAD);
    }
    Ok(out)
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
