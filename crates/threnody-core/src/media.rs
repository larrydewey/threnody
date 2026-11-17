//! Strips identifying metadata from photos and videos before they are
//! sent.
//!
//! Photos carry EXIF (GPS position, camera serial numbers, time), XMP,
//! comments, thumbnails and, from some phones, data after the image such
//! as the video of a motion photo; videos carry the place and time they
//! were recorded and the phone's make and model. All of it is removed
//! without re-encoding, so the picture or film itself is unchanged. What
//! decoders need to show the image as intended is kept: colour profiles,
//! transparency, animation, and a JPEG's orientation (rewritten as a
//! minimal EXIF block with nothing else in it).
//!
//! JPEG, PNG, WebP, GIF and ISO base media files (MP4, MOV, M4A, 3GP) are
//! understood. Anything else is left alone and reported as
//! [`Stripped::Unsupported`], so callers can decide. The names photos,
//! videos and sound files were given on the device ([`neutral_name`]) can
//! say as much as their metadata, so callers replace those too.

mod bmff;

use crate::{Error, Result};

/// What [`strip`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Stripped {
    /// A supported image or video, now without metadata (possibly
    /// unchanged).
    Image(Vec<u8>),
    /// Not a format [`Kind`] names: returned untouched.
    Unsupported(Vec<u8>),
}

/// Kinds recognised by their first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Webp,
    Gif,
    /// ISO base media: MP4, MOV, M4A, 3GP (not HEIF or AVIF photos, see
    /// [`needs_converting`]).
    Bmff,
}

pub fn kind(data: &[u8]) -> Option<Kind> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if bmff::is_bmff(data) && !needs_converting(data) {
        Some(Kind::Bmff)
    } else {
        None
    }
}

/// Whether `data` is a photo in a format that carries metadata (location,
/// camera details) which [`strip`] can't remove: HEIC/HEIF and AVIF, and
/// TIFF, which camera raw files such as DNG are built on. Apps re-encode
/// such photos as JPEG before sending; the node won't send them as they
/// are while stripping is on. Recognised by their first bytes, whatever
/// the file is called.
pub fn needs_converting(data: &[u8]) -> bool {
    /// HEIF and AVIF brands (their EXIF is an item that may lie anywhere
    /// in the file), and Canon's raw photos (whose EXIF is in boxes of
    /// Canon's own); videos in the same container have others.
    const BRANDS: [&[u8; 4]; 13] = [
        b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
        b"avif", b"avis", b"crx ",
    ];
    let heif =
        data.len() >= 12 && &data[4..8] == b"ftyp" && BRANDS.iter().any(|b| &data[8..12] == *b);
    heif || data.starts_with(b"II*\0") || data.starts_with(b"MM\0*")
}

/// Removes metadata from `data` if it is an image or video we
/// understand. A damaged file of a supported kind is an error rather
/// than being sent with its metadata.
///
/// [`as_sent`] relies on this being deterministic across versions;
/// changing the output makes old outgoing photos unservable from the
/// device that sent them.
pub fn strip(data: Vec<u8>) -> Result<Stripped> {
    Ok(match kind(&data) {
        Some(Kind::Jpeg) => Stripped::Image(strip_jpeg(&data)?),
        Some(Kind::Png) => Stripped::Image(strip_png(&data)?),
        Some(Kind::Webp) => Stripped::Image(strip_webp(&data)?),
        Some(Kind::Gif) => Stripped::Image(strip_gif(&data)?),
        Some(Kind::Bmff) => Stripped::Image(bmff::strip(data)?),
        None => Stripped::Unsupported(data),
    })
}

/// The bytes that were sent of a file now `raw` on disk, recorded as
/// `size` bytes: the metadata-stripped form if that has the recorded size
/// (an image stripped as it was sent), else `raw` if it has, else None
/// (the file changed since). The stripped form is tried first, so a
/// sender's original never leaves with metadata the sent copy didn't
/// have. A video stripped keeps its size, so one sent with its metadata
/// (or before videos were stripped) is served without it.
pub fn as_sent(raw: Vec<u8>, size: u64) -> Option<Vec<u8>> {
    if kind(&raw).is_some()
        && let Ok(Stripped::Image(s)) = strip(raw.clone())
        && s.len() as u64 == size
    {
        return Some(s);
    }
    (raw.len() as u64 == size).then_some(raw)
}

/// Picture extensions.
const IMAGES: [&str; 11] = [
    "jpg", "jpeg", "png", "webp", "gif", "heic", "heif", "avif", "bmp", "tif", "tiff",
];
const VIDEOS: [&str; 11] = [
    "mp4", "m4v", "mov", "qt", "3gp", "3g2", "webm", "mkv", "avi", "mpg", "mpeg",
];
const SOUNDS: [&str; 11] = [
    "m4a", "aac", "mp3", "ogg", "oga", "opus", "wav", "flac", "amr", "3ga", "weba",
];

/// Whether a file called `name` is a picture, by its extension (the
/// apps show these inline).
pub fn looks_like_image(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| IMAGES.iter().any(|i| ext.eq_ignore_ascii_case(i)))
}

/// A name for a photo, video or sound file that says nothing but what it
/// is: `photo.jpg` for `PXL_20261117_143022123.JPG`, `video.mov` for
/// `IMG_1234.MOV`. Names phones and cameras give carry the time taken, a
/// counter, the maker and sometimes a place. The extension, lower-cased,
/// is kept so the file still opens; a name without one takes it from
/// the bytes. None for anything else (documents, which people name on
/// purpose) and for a name that is already neutral.
pub fn neutral_name(name: &str, data: &[u8]) -> Option<String> {
    let ext = match name.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => match kind(data)? {
            Kind::Jpeg => "jpg".into(),
            Kind::Png => "png".into(),
            Kind::Webp => "webp".into(),
            Kind::Gif => "gif".into(),
            Kind::Bmff => "mp4".into(),
        },
    };
    let what = if IMAGES.contains(&ext.as_str()) {
        "photo"
    } else if VIDEOS.contains(&ext.as_str()) {
        "video"
    } else if SOUNDS.contains(&ext.as_str()) {
        "audio"
    } else {
        return None;
    };
    let neutral = format!("{what}.{ext}");
    (neutral != name).then_some(neutral)
}

const BAD: Error = Error::Malformed("image");

fn strip_jpeg(d: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(d.len());
    out.extend_from_slice(&[0xFF, 0xD8]);
    let mut orientation = None;
    let mut i = 2;
    loop {
        // Markers may be padded with extra 0xFF bytes.
        while d.get(i) == Some(&0xFF) && d.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        if d.get(i) != Some(&0xFF) {
            return Err(BAD);
        }
        let marker = *d.get(i + 1).ok_or(BAD)?;
        match marker {
            // Standalone markers have no length.
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&d[i..i + 2]);
                i += 2;
                continue;
            }
            // End of image before any scan.
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(out);
            }
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([
            *d.get(i + 2).ok_or(BAD)?,
            *d.get(i + 3).ok_or(BAD)?,
        ]));
        if len < 2 {
            return Err(BAD);
        }
        let seg = d.get(i..i + 2 + len).ok_or(BAD)?;
        let body = &seg[4..];
        let keep = match marker {
            // EXIF: keep only the orientation, written back below.
            0xE1 => {
                if body.starts_with(b"Exif\0\0") {
                    orientation = orientation.or_else(|| exif_orientation(&body[6..]));
                }
                false
            }
            // ICC colour profiles only (not MPF, which points at the
            // trailing images we drop).
            0xE2 => body.starts_with(b"ICC_PROFILE\0"),
            // JFIF, without its thumbnail (which can show the picture as
            // it was before it was cropped), written back here. JFXX
            // segments hold nothing but thumbnails.
            0xE0 => {
                if let Some(jfif) = jfif_segment(body) {
                    out.extend_from_slice(&jfif);
                }
                false
            }
            // Adobe's colour transform flag, which decoders need.
            0xEE => true,
            // Other application segments and comments: metadata.
            0xE3..=0xED | 0xEF | 0xFE => false,
            _ => true,
        };
        if keep {
            if marker == 0xDA || is_frame(marker) {
                // The first frame or scan: put the orientation before it.
                if let Some(o) = orientation.take().filter(|o| *o != 1) {
                    out.extend_from_slice(&exif_segment(o));
                }
            }
            out.extend_from_slice(seg);
        }
        i += 2 + len;
        if marker == 0xDA {
            // Entropy-coded data runs to the next marker that isn't a
            // stuffed byte or a restart marker.
            let start = i;
            loop {
                let ff = d.get(i..).ok_or(BAD)?.iter().position(|b| *b == 0xFF);
                i += ff.ok_or(BAD)?;
                match d.get(i + 1) {
                    Some(0x00 | 0xD0..=0xD7) => i += 2,
                    Some(0xFF) => i += 1,
                    Some(_) => break,
                    None => return Err(BAD),
                }
            }
            out.extend_from_slice(&d[start..i]);
            // Anything after the image's end (motion photo video, gain
            // map images) is dropped.
            if d.get(i + 1) == Some(&0xD9) {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Ok(out);
            }
        }
    }
}

/// A JFIF APP0 segment from the body of one, without the thumbnail
/// (whose size it then says is 0 by 0); None if `body` isn't JFIF.
fn jfif_segment(body: &[u8]) -> Option<Vec<u8>> {
    // "JFIF\0", version, units, densities; then the thumbnail's size.
    let head = body.get(..12).filter(|b| b.starts_with(b"JFIF\0"))?;
    let mut seg = vec![0xFF, 0xE0, 0, 16];
    seg.extend_from_slice(head);
    seg.extend_from_slice(&[0, 0]);
    Some(seg)
}

fn is_frame(marker: u8) -> bool {
    matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
}

/// The orientation tag (0x0112) from a TIFF block's first IFD.
fn exif_orientation(t: &[u8]) -> Option<u16> {
    let be = match t.get(..4)? {
        b"MM\0*" => true,
        b"II*\0" => false,
        _ => return None,
    };
    let u16_at = |o: usize| {
        let b: [u8; 2] = t.get(o..o + 2)?.try_into().ok()?;
        Some(if be {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |o: usize| {
        let b: [u8; 4] = t.get(o..o + 4)?.try_into().ok()?;
        Some(if be {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let n = usize::from(u16_at(ifd)?);
    (0..n.min(512)).find_map(|k| {
        let e = ifd + 2 + 12 * k;
        (u16_at(e)? == 0x0112 && u16_at(e + 2)? == 3)
            .then(|| u16_at(e + 8))
            .flatten()
            .filter(|o| (1..=8).contains(o))
    })
}

/// An APP1 segment holding EXIF with nothing but `orientation`.
fn exif_segment(orientation: u16) -> Vec<u8> {
    let mut tiff = b"MM\0*\0\0\0\x08".to_vec();
    tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_be_bytes());
    tiff.extend_from_slice(&3u16.to_be_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_be_bytes());
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]);
    tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    let mut seg = vec![0xFF, 0xE1];
    let len = 2 + 6 + tiff.len();
    seg.extend_from_slice(&u16::try_from(len).unwrap_or(0).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

/// PNG chunks a decoder may need; everything else (text, EXIF, times,
/// unknown chunks) is dropped.
const PNG_KEEP: [&[u8; 4]; 15] = [
    b"IHDR", b"PLTE", b"IDAT", b"IEND", b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"iCCP", b"sBIT",
    b"bKGD", b"pHYs", b"acTL", b"fcTL", b"fdAT",
];

fn strip_png(d: &[u8]) -> Result<Vec<u8>> {
    let mut out = d[..8].to_vec();
    let mut i = 8;
    loop {
        let len = u32::from_be_bytes(d.get(i..i + 4).ok_or(BAD)?.try_into().map_err(|_| BAD)?);
        let len = usize::try_from(len).map_err(|_| BAD)?;
        let end = len.checked_add(i + 12).ok_or(BAD)?;
        let chunk = d.get(i..end).ok_or(BAD)?;
        let ty = &chunk[4..8];
        if PNG_KEEP.iter().any(|k| *k == ty) {
            out.extend_from_slice(chunk);
        }
        i = end;
        if ty == b"IEND" {
            // Nothing after the end.
            return Ok(out);
        }
    }
}

/// WebP chunks a decoder may need; everything else (EXIF, XMP, unknown
/// chunks) is dropped.
const WEBP_KEEP: [&[u8; 4]; 7] = [
    b"VP8 ", b"VP8L", b"VP8X", b"ALPH", b"ANIM", b"ANMF", b"ICCP",
];

fn strip_webp(d: &[u8]) -> Result<Vec<u8>> {
    let mut out = b"RIFF\0\0\0\0WEBP".to_vec();
    let riff = u32::from_le_bytes(d[4..8].try_into().map_err(|_| BAD)?);
    let end = usize::try_from(riff)
        .map_err(|_| BAD)?
        .checked_add(8)
        .ok_or(BAD)?;
    let body = d.get(..end).ok_or(BAD)?;
    let mut i = 12;
    let mut vp8x = None;
    while i < body.len() {
        let len = u32::from_le_bytes(
            body.get(i + 4..i + 8)
                .ok_or(BAD)?
                .try_into()
                .map_err(|_| BAD)?,
        );
        let len = usize::try_from(len).map_err(|_| BAD)?;
        let end = len
            .checked_add(len & 1)
            .and_then(|p| p.checked_add(i + 8))
            .ok_or(BAD)?;
        let chunk = body.get(i..end).ok_or(BAD)?;
        let ty = &chunk[..4];
        if WEBP_KEEP.iter().any(|k| *k == ty) {
            if ty == b"VP8X" {
                vp8x = Some(out.len());
            }
            out.extend_from_slice(chunk);
        }
        i = end;
    }
    // Clear the extended header's EXIF (0x08) and XMP (0x04) flags.
    if let Some(at) = vp8x {
        *out.get_mut(at + 8).ok_or(BAD)? &= !0x0C;
    }
    let size = u32::try_from(out.len() - 8).map_err(|_| BAD)?;
    out[4..8].copy_from_slice(&size.to_le_bytes());
    Ok(out)
}

/// GIF application extensions a decoder may need: looping and a colour
/// profile. Others (XMP among them) are dropped.
const GIF_APPS: [&[u8; 11]; 3] = [b"NETSCAPE2.0", b"ANIMEXTS1.0", b"ICCRGBG1012"];

fn strip_gif(d: &[u8]) -> Result<Vec<u8>> {
    // The header and screen descriptor, then the global colour table.
    let flags = *d.get(10).ok_or(BAD)?;
    let mut i = 13 + colour_table(flags);
    let mut out = d.get(..i).ok_or(BAD)?.to_vec();
    loop {
        match *d.get(i).ok_or(BAD)? {
            // The trailer: nothing after it.
            0x3B => {
                out.push(0x3B);
                return Ok(out);
            }
            // An image: descriptor, local colour table, LZW code size,
            // then the data.
            0x2C => {
                let flags = *d.get(i + 9).ok_or(BAD)?;
                let end = sub_blocks(d, i + 11 + colour_table(flags))?;
                out.extend_from_slice(&d[i..end]);
                i = end;
            }
            0x21 => {
                let label = *d.get(i + 1).ok_or(BAD)?;
                let end = sub_blocks(d, i + 2)?;
                let keep = match label {
                    // Graphic control (frame timing, transparency) and
                    // plain text, which is drawn as part of the picture.
                    0xF9 | 0x01 => true,
                    0xFF => {
                        d.get(i + 2) == Some(&11)
                            && GIF_APPS
                                .iter()
                                .any(|a| d.get(i + 3..i + 14) == Some(&a[..]))
                    }
                    // Comments, and anything unknown.
                    _ => false,
                };
                if keep {
                    out.extend_from_slice(&d[i..end]);
                }
                i = end;
            }
            _ => return Err(BAD),
        }
    }
}

/// The size of the colour table a GIF descriptor's `flags` announce.
fn colour_table(flags: u8) -> usize {
    if flags & 0x80 == 0 {
        0
    } else {
        3 << ((flags & 7) + 1)
    }
}

/// Where the sub-blocks starting at `i` end, after their empty
/// terminator.
fn sub_blocks(d: &[u8], mut i: usize) -> Result<usize> {
    loop {
        let n = usize::from(*d.get(i).ok_or(BAD)?);
        i += 1 + n;
        if n == 0 {
            return Ok(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&u16::try_from(body.len() + 2).unwrap().to_be_bytes());
        s.extend_from_slice(body);
        s
    }

    fn exif(orientation: u16, little: bool) -> Vec<u8> {
        let w16 = |v: u16| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let w32 = |v: u32| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let mut t = b"Exif\0\0".to_vec();
        t.extend_from_slice(if little { b"II*\0" } else { b"MM\0*" });
        t.extend_from_slice(&w32(8));
        t.extend_from_slice(&w16(2));
        // A GPS IFD pointer, then the orientation.
        t.extend_from_slice(&w16(0x8825));
        t.extend_from_slice(&w16(4));
        t.extend_from_slice(&w32(1));
        t.extend_from_slice(&w32(0x1234));
        t.extend_from_slice(&w16(0x0112));
        t.extend_from_slice(&w16(3));
        t.extend_from_slice(&w32(1));
        t.extend_from_slice(&w16(orientation));
        t.extend_from_slice(&[0, 0]);
        t.extend_from_slice(&w32(0));
        t.extend_from_slice(b"GPS 51.5N 0.12W serial 1234");
        t
    }

    fn jpeg(app1: &[u8]) -> Vec<u8> {
        let mut j = vec![0xFF, 0xD8];
        j.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        j.extend(seg(0xE1, app1));
        j.extend(seg(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta GPS/>"));
        j.extend(seg(0xE2, b"ICC_PROFILE\0\x01\x01colour"));
        j.extend(seg(0xE2, b"MPF\0pointers"));
        j.extend(seg(0xFE, b"a comment"));
        j.extend(seg(0xDB, &[0; 65]));
        j.extend(seg(0xC0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]));
        j.extend(seg(0xDA, &[1, 1, 0, 0, 0x3F, 0]));
        j.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56]);
        j.extend([0xFF, 0xD9]);
        j.extend(b"ftypmp42 motion photo video");
        j
    }

    fn contains(h: &[u8], n: &[u8]) -> bool {
        h.windows(n.len()).any(|w| w == n)
    }

    #[test]
    fn jpeg_loses_metadata_but_keeps_pixels_colour_and_orientation() {
        for little in [false, true] {
            let Stripped::Image(out) = strip(jpeg(&exif(6, little))).unwrap() else {
                panic!("not an image")
            };
            for gone in [
                &b"GPS"[..],
                b"xmpmeta",
                b"a comment",
                b"MPF",
                b"motion photo",
            ] {
                assert!(
                    !contains(&out, gone),
                    "{:?} left",
                    String::from_utf8_lossy(gone)
                );
            }
            for kept in [
                &b"JFIF"[..],
                b"ICC_PROFILE",
                &[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56],
            ] {
                assert!(contains(&out, kept));
            }
            assert!(out.ends_with(&[0xFF, 0xD9]));
            // Orientation 6 survives, in a minimal EXIF block before the frame.
            let at = out.windows(2).position(|w| w == [0xFF, 0xE1]).unwrap();
            let len = usize::from(u16::from_be_bytes([out[at + 2], out[at + 3]]));
            assert_eq!(exif_orientation(&out[at + 10..at + 2 + len]), Some(6));
            let frame = out.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
            assert!(at < frame);
            // Stripping again changes nothing.
            assert_eq!(strip(out.clone()).unwrap(), Stripped::Image(out));
        }
        // Upright photos need no EXIF at all.
        let Stripped::Image(out) = strip(jpeg(&exif(1, false))).unwrap() else {
            panic!()
        };
        assert!(!contains(&out, b"Exif"));
    }

    #[test]
    fn png_keeps_only_rendering_chunks() {
        fn chunk(ty: &[u8], body: &[u8]) -> Vec<u8> {
            let mut c = u32::try_from(body.len()).unwrap().to_be_bytes().to_vec();
            c.extend_from_slice(ty);
            c.extend_from_slice(body);
            c.extend_from_slice(&[0; 4]); // CRCs aren't checked here
            c
        }
        let mut p = b"\x89PNG\r\n\x1a\n".to_vec();
        p.extend(chunk(b"IHDR", &[0; 13]));
        p.extend(chunk(b"tEXt", b"Author\0me"));
        p.extend(chunk(b"eXIf", b"MM\0*GPS"));
        p.extend(chunk(b"iCCP", b"profile"));
        p.extend(chunk(b"IDAT", b"pixels"));
        p.extend(chunk(b"tIME", &[0; 7]));
        p.extend(chunk(b"IEND", b""));
        p.extend(b"trailing");
        let Stripped::Image(out) = strip(p).unwrap() else {
            panic!()
        };
        for gone in [&b"Author"[..], b"GPS", b"tIME", b"trailing"] {
            assert!(!contains(&out, gone));
        }
        for kept in [&b"IHDR"[..], b"iCCP", b"pixels", b"IEND"] {
            assert!(contains(&out, kept));
        }
    }

    #[test]
    fn webp_drops_exif_and_xmp_and_fixes_sizes() {
        fn chunk(ty: &[u8], body: &[u8]) -> Vec<u8> {
            let mut c = ty.to_vec();
            c.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
            c.extend_from_slice(body);
            if body.len() % 2 == 1 {
                c.push(0);
            }
            c
        }
        let mut body = b"WEBP".to_vec();
        body.extend(chunk(b"VP8X", &[0x0C | 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        body.extend(chunk(b"VP8L", b"pixels!"));
        body.extend(chunk(b"EXIF", b"GPS here"));
        body.extend(chunk(b"XMP ", b"<xmp/>"));
        // Not one a decoder needs, whatever it holds.
        body.extend(chunk(b"MAKR", b"Pixel 9 serial 42"));
        let mut w = b"RIFF".to_vec();
        w.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        w.extend(body);
        let Stripped::Image(out) = strip(w).unwrap() else {
            panic!()
        };
        assert!(!contains(&out, b"GPS") && !contains(&out, b"xmp"));
        assert!(!contains(&out, b"MAKR") && !contains(&out, b"serial"));
        assert!(contains(&out, b"pixels!"));
        assert_eq!(out[20], 0x10, "EXIF and XMP flags cleared, alpha kept");
        let riff = u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, out.len());
    }

    #[test]
    fn jpeg_loses_jfif_thumbnails() {
        let mut j = vec![0xFF, 0xD8];
        // JFIF with a 1x1 thumbnail, then a JFXX thumbnail.
        j.extend(seg(0xE0, b"JFIF\0\x01\x02\x01\0\x48\0\x48\x01\x01THM"));
        j.extend(seg(0xE0, b"JFXX\0\x10uncropped thumbnail"));
        j.extend(seg(0xDB, &[0; 65]));
        j.extend(seg(0xC0, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]));
        j.extend(seg(0xDA, &[1, 1, 0, 0, 0x3F, 0]));
        j.extend([0x12, 0x34, 0xFF, 0xD9]);
        let Stripped::Image(out) = strip(j).unwrap() else {
            panic!()
        };
        assert!(!contains(&out, b"THM") && !contains(&out, b"JFXX"));
        assert!(!contains(&out, b"thumbnail"));
        // JFIF stays, its densities as they were and no thumbnail.
        assert!(contains(
            &out,
            &seg(0xE0, b"JFIF\0\x01\x02\x01\0\x48\0\x48\0\0")
        ));
        assert_eq!(strip(out.clone()).unwrap(), Stripped::Image(out));
    }

    fn gif() -> Vec<u8> {
        let mut g = b"GIF89a\x01\0\x01\0\x80\0\0".to_vec();
        g.extend([0, 0, 0, 0xFF, 0xFF, 0xFF]); // two colours
        g.extend(b"\x21\xFF\x0bNETSCAPE2.0\x03\x01\0\0\0");
        g.extend(b"\x21\xFE\x09a comment\0");
        // XMP's "magic trailer" makes it valid sub-blocks; a short one.
        g.extend(b"\x21\xFF\x0bXMP DataXMP\x0f<x:xmpmeta GPS>\0");
        g.extend(b"\x21\xF9\x04\x01\x0a\0\x01\0");
        // An image with a local table of two colours.
        g.extend(b"\x2C\0\0\0\0\x01\0\x01\0\x80");
        g.extend([1, 2, 3, 4, 5, 6]);
        g.extend(b"\x02\x02\x44\x01\0");
        g.extend(b"\x3Bappended after the end");
        g
    }

    #[test]
    fn gif_loses_comments_and_xmp_but_keeps_frames_and_looping() {
        let Stripped::Image(out) = strip(gif()).unwrap() else {
            panic!()
        };
        for gone in [&b"a comment"[..], b"XMP", b"GPS", b"appended"] {
            assert!(!contains(&out, gone), "{:?}", String::from_utf8_lossy(gone));
        }
        for kept in [
            &b"NETSCAPE2.0\x03\x01\0\0\0"[..],
            b"\x21\xF9\x04\x01\x0a\0\x01\0",
            b"\x2C\0\0\0\0\x01\0\x01\0\x80\x01\x02\x03\x04\x05\x06\x02\x02\x44\x01\0",
        ] {
            assert!(contains(&out, kept));
        }
        assert!(out.starts_with(b"GIF89a") && out.ends_with(b"\0\x3B"));
        assert_eq!(strip(out.clone()).unwrap(), Stripped::Image(out));
    }

    /// An ISO base media box.
    fn bx(ty: &[u8], body: &[u8]) -> Vec<u8> {
        let mut b = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        b.extend_from_slice(ty);
        b.extend_from_slice(body);
        b
    }

    /// How an `mdat` says how long it is.
    #[derive(Clone, Copy)]
    enum Size {
        Plain,
        Large,
        ToEnd,
    }

    const SAMPLE: &[u8] = b"SAMPLE!!";

    /// A phone video's boxes in miniature: location, make and times, and
    /// a chunk offset pointing at the one sample.
    fn movie(moov_first: bool, size: Size) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"qt  \0\0\0\0qt  isom");
        let mdat = match size {
            Size::Plain => bx(b"mdat", SAMPLE),
            Size::Large => {
                let mut m = 1u32.to_be_bytes().to_vec();
                m.extend_from_slice(b"mdat");
                m.extend_from_slice(&(16 + SAMPLE.len() as u64).to_be_bytes());
                m.extend_from_slice(SAMPLE);
                m
            }
            Size::ToEnd => {
                let mut m = vec![0, 0, 0, 0];
                m.extend_from_slice(b"mdat");
                m.extend_from_slice(SAMPLE);
                m
            }
        };
        let xmp = {
            let mut b = bmff_xmp_uuid().to_vec();
            b.extend_from_slice(b"<x:xmpmeta GPS/>");
            bx(b"uuid", &b)
        };
        let moov = |offset: u32| {
            let mut mvhd = vec![0, 0, 0, 0];
            mvhd.extend([0xC1; 4]);
            mvhd.extend([0xC2; 4]);
            mvhd.extend([0x55; 88]);
            let mut tkhd = vec![1, 0, 0, 3];
            tkhd.extend([0xC1; 8]);
            tkhd.extend([0xC2; 8]);
            tkhd.extend([0x55; 64]);
            let mut mdhd = vec![0, 0, 0, 0];
            mdhd.extend([0xC1; 4]);
            mdhd.extend([0xC2; 4]);
            mdhd.extend([0x55; 12]);
            let mut stco = vec![0, 0, 0, 0, 0, 0, 0, 1];
            stco.extend(offset.to_be_bytes());
            let stbl = bx(b"stbl", &bx(b"stco", &stco));
            let mut mdia = bx(b"mdhd", &mdhd);
            mdia.extend(bx(b"minf", &stbl));
            let mut trak = bx(b"tkhd", &tkhd);
            trak.extend(bx(b"meta", b"\0\0\0\0track GPS"));
            trak.extend(bx(b"mdia", &mdia));
            let mut meta = vec![0, 0, 0, 0];
            meta.extend(bx(b"keys", b"com.apple.quicktime.location.ISO6709"));
            meta.extend(bx(b"ilst", b"+51.5074-000.1278/ Pixel 9"));
            let mut body = bx(b"mvhd", &mvhd);
            body.extend(bx(b"udta", &bx(b"\xa9xyz", b"+51.5074-000.1278/")));
            body.extend(bx(b"meta", &meta));
            body.extend(bx(b"trak", &trak));
            bx(b"moov", &body)
        };
        let head = mdat.len() - SAMPLE.len();
        let mut m = ftyp.clone();
        if moov_first {
            let at = ftyp.len() + moov(0).len() + xmp.len() + head;
            m.extend(moov(u32::try_from(at).unwrap()));
            m.extend(xmp);
            m.extend(mdat);
        } else if matches!(size, Size::ToEnd) {
            unreachable!("mdat to the end comes last")
        } else {
            m.extend(xmp);
            m.extend(mdat);
            let at = ftyp.len() + bmff_xmp_len() + head;
            m.extend(moov(u32::try_from(at).unwrap()));
        }
        m
    }

    fn bmff_xmp_uuid() -> [u8; 16] {
        [
            0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3,
            0xaf, 0xac,
        ]
    }

    fn bmff_xmp_len() -> usize {
        8 + 16 + b"<x:xmpmeta GPS/>".len()
    }

    /// The sample the `stco` box of `m` points at.
    fn sample_at_offset(m: &[u8]) -> &[u8] {
        let at = m.windows(4).position(|w| w == b"stco").unwrap() + 12;
        let off = u32::from_be_bytes(m[at..at + 4].try_into().unwrap()) as usize;
        &m[off..off + SAMPLE.len()]
    }

    #[test]
    fn video_loses_place_device_and_times_and_nothing_moves() {
        for (moov_first, size) in [
            (true, Size::Plain),
            (false, Size::Plain),
            (true, Size::Large),
            (false, Size::Large),
            (true, Size::ToEnd),
        ] {
            let m = movie(moov_first, size);
            assert_eq!(kind(&m), Some(Kind::Bmff));
            assert_eq!(sample_at_offset(&m), SAMPLE);
            let Stripped::Image(out) = strip(m.clone()).unwrap() else {
                panic!()
            };
            for gone in [
                &b"51.5074"[..],
                b"\xa9xyz",
                b"ISO6709",
                b"Pixel",
                b"GPS",
                b"udta",
                b"meta",
                &bmff_xmp_uuid(),
                &[0xC1; 4],
                &[0xC2; 4],
            ] {
                assert!(!contains(&out, gone), "{:?}", String::from_utf8_lossy(gone));
            }
            // Same size, same place for every box and sample.
            assert_eq!(out.len(), m.len());
            assert_eq!(sample_at_offset(&out), SAMPLE);
            for kept in [&b"ftypqt  "[..], b"mvhd", b"tkhd\x01\0\0\x03", b"stco"] {
                assert!(contains(&out, kept));
            }
            assert_eq!(strip(out.clone()).unwrap(), Stripped::Image(out));
        }
    }

    /// A track: handler `handler`, its samples' sizes, and the chunk
    /// offsets (one sample per chunk), in `co64` when `wide`.
    fn track(handler: &[u8; 4], sizes: &[u32], offsets: &[u64], wide: bool) -> Vec<u8> {
        let mut hdlr = vec![0; 8];
        hdlr.extend(handler);
        hdlr.extend([0; 12]);
        let mut stsz = vec![0; 8];
        stsz[8 - 4..].copy_from_slice(&0u32.to_be_bytes());
        stsz.extend((sizes.len() as u32).to_be_bytes());
        for z in sizes {
            stsz.extend(z.to_be_bytes());
        }
        let mut stsc = vec![0, 0, 0, 0, 0, 0, 0, 1];
        stsc.extend([0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);
        let mut co = vec![0, 0, 0, 0];
        co.extend((offsets.len() as u32).to_be_bytes());
        for o in offsets {
            if wide {
                co.extend(o.to_be_bytes());
            } else {
                co.extend((*o as u32).to_be_bytes());
            }
        }
        let mut stbl = bx(b"stsz", &stsz);
        stbl.extend(bx(b"stsc", &stsc));
        stbl.extend(bx(if wide { b"co64" } else { b"stco" }, &co));
        let mut mdia = bx(b"hdlr", &hdlr);
        mdia.extend(bx(b"minf", &bx(b"stbl", &stbl)));
        bx(b"trak", &bx(b"mdia", &mdia))
    }

    #[test]
    fn video_loses_timed_location_tracks() {
        const PICTURE: &[u8] = b"PICTURE!";
        const FIXES: [&[u8]; 2] = [b"GPS +51.5074", b"GPS -000.1278"];
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
        let build = |offsets: [u64; 3], handler: &[u8; 4]| {
            let mut body = track(b"vide", &[PICTURE.len() as u32], &offsets[..1], false);
            body.extend(track(
                handler,
                &FIXES.map(|f| f.len() as u32),
                &offsets[1..],
                true,
            ));
            bx(b"moov", &body)
        };
        let mut samples = PICTURE.to_vec();
        for f in FIXES {
            samples.extend(f);
        }
        let mdat = bx(b"mdat", &samples);
        for handler in [b"meta", b"text", b"camm"] {
            let start = (ftyp.len() + build([0; 3], handler).len() + 8) as u64;
            let offsets = [
                start,
                start + PICTURE.len() as u64,
                start + (PICTURE.len() + FIXES[0].len()) as u64,
            ];
            let mut m = ftyp.clone();
            m.extend(build(offsets, handler));
            m.extend(&mdat);
            let Stripped::Image(out) = strip(m.clone()).unwrap() else {
                panic!()
            };
            assert_eq!(out.len(), m.len());
            assert!(contains(&out, PICTURE), "the picture stays");
            assert!(
                !contains(&out, b"GPS"),
                "{:?}",
                String::from_utf8_lossy(handler)
            );
            let at = offsets[1] as usize;
            assert_eq!(&out[at..at + FIXES[0].len() + FIXES[1].len()], &[0; 25][..]);
            assert_eq!(strip(out.clone()).unwrap(), Stripped::Image(out));
            // A sample said to lie past the end: refused, not sent.
            let mut bad = ftyp.clone();
            bad.extend(build([offsets[0], offsets[1], m.len() as u64], handler));
            bad.extend(&mdat);
            assert!(strip(bad).is_err());
            // Fragmented, with such a track: refused.
            let mut frag = m.clone();
            frag.extend(bx(b"moof", &[]));
            assert!(strip(frag).is_err());
        }
    }

    #[test]
    fn damaged_videos_fail() {
        let m = movie(true, Size::Plain);
        // Cut short anywhere, a box no longer fits (or the movie is gone).
        for cut in [9, 30, 60, m.len() / 2, m.len() - 1] {
            assert!(strip(m[..cut].to_vec()).is_err(), "cut at {cut}");
        }
        // No movie at all.
        assert!(strip(bx(b"ftyp", b"isom")).is_err());
        // A box smaller than its header.
        let mut tiny = bx(b"ftyp", b"isom");
        tiny.extend([0, 0, 0, 4]);
        tiny.extend(b"moov");
        assert!(strip(tiny).is_err());
        // A header of a version we don't know where the times are in.
        let mut odd = bx(b"ftyp", b"isom");
        odd.extend(bx(b"moov", &bx(b"mvhd", &[2; 40])));
        assert!(strip(odd).is_err());
        // Boxes nested deeper than any metadata is.
        let mut deep = bx(b"mdhd", &[0; 24]);
        for _ in 0..100 {
            deep = bx(b"trak", &deep);
        }
        let mut deep_movie = bx(b"ftyp", b"isom");
        deep_movie.extend(bx(b"moov", &deep));
        assert!(strip(deep_movie).is_err());
        // A 64-bit size past the end.
        let mut huge = bx(b"ftyp", b"isom");
        huge.extend(1u32.to_be_bytes());
        huge.extend(b"mdat");
        huge.extend(u64::MAX.to_be_bytes());
        assert!(strip(huge).is_err());
    }

    #[test]
    fn neutral_names_for_photos_videos_and_sounds_only() {
        let jpeg = jpeg(&exif(1, false));
        for (name, neutral) in [
            ("PXL_20261117_143022123.jpg", Some("photo.jpg")),
            ("IMG_1234.HEIC", Some("photo.heic")),
            ("IMG_1234.MOV", Some("video.mov")),
            ("VID_20261117_143022.mp4", Some("video.mp4")),
            ("Recording 17 Nov at 14.30.m4a", Some("audio.m4a")),
            ("photo.jpg", None),
            ("Tax return 2026.pdf", None),
            ("notes", None),
            ("archive.tar.gz", None),
        ] {
            assert_eq!(neutral_name(name, b"%PDF").as_deref(), neutral, "{name}");
        }
        // No extension: the bytes say what it is.
        assert_eq!(neutral_name("image", &jpeg).as_deref(), Some("photo.jpg"));
        assert_eq!(
            neutral_name("clip", &movie(true, Size::Plain)).as_deref(),
            Some("video.mp4")
        );
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let seeds = [
            jpeg(&exif(6, true)),
            b"\x89PNG\r\n\x1a\n\0\0\0\x01IHDRx".to_vec(),
            gif(),
            movie(true, Size::Plain),
            movie(false, Size::Large),
        ];
        let mut x: u32 = 1;
        let mut rand = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as usize
        };
        for _ in 0..20_000 {
            let mut d = seeds[rand() % seeds.len()].clone();
            for _ in 0..1 + rand() % 4 {
                let i = rand() % d.len();
                d[i] = rand() as u8;
            }
            d.truncate(1 + rand() % d.len());
            let _ = strip(d);
        }
        let mut w = b"RIFF\x10\0\0\0WEBPVP8X\xff\xff\xff\xff".to_vec();
        w.resize(24, 0);
        let _ = strip(w);
    }

    #[test]
    fn other_files_pass_and_damaged_images_fail() {
        assert_eq!(
            strip(b"hello".to_vec()).unwrap(),
            Stripped::Unsupported(b"hello".to_vec())
        );
        let j = jpeg(&exif(6, false));
        for cut in [3, 10, 40, j.len() - 40] {
            assert!(strip(j[..cut].to_vec()).is_err(), "cut at {cut}");
        }
        assert!(strip(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec()).is_err());
        assert!(strip(b"RIFF\xff\xff\xff\x7fWEBP".to_vec()).is_err());
    }

    #[test]
    fn photo_formats_with_metadata_we_cant_strip_are_recognised() {
        for photo in [
            &b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"[..],
            b"\0\0\0\x1cftypmif1\0\0\0\0mif1heic",
            b"\0\0\0\x20ftypavif\0\0\0\0avifmif1",
            b"II*\0\x08\0\0\0",
            b"MM\0*\0\0\0\x08",
        ] {
            assert!(needs_converting(photo), "{photo:?}");
        }
        // Videos share the container, and the node strips these itself.
        for other in [
            &b"\0\0\0\x18ftypisom\0\0\0\0isommp42"[..],
            b"\0\0\0\x14ftypqt  \0\0\0\0qt  ",
            b"\xff\xd8\xff\xe0",
            b"hello",
            b"",
        ] {
            assert!(!needs_converting(other), "{other:?}");
        }
    }

    #[test]
    fn as_sent_prefers_stripped_then_raw() {
        let original = jpeg(&exif(6, false));
        let Stripped::Image(stripped) = strip(original.clone()).unwrap() else {
            panic!()
        };
        assert!(stripped.len() < original.len());
        // A photo sent stripped: its stripped size is recorded.
        assert_eq!(
            as_sent(original.clone(), stripped.len() as u64),
            Some(stripped.clone())
        );
        // A note keeps the raw size.
        assert_eq!(
            as_sent(original.clone(), original.len() as u64),
            Some(original.clone())
        );
        // Not an image: raw, if the size matches.
        let doc = b"%PDF-1.7 whatever".to_vec();
        assert_eq!(as_sent(doc.clone(), doc.len() as u64), Some(doc.clone()));
        // Changed since: neither size matches.
        assert_eq!(as_sent(original, 3), None);
        assert_eq!(as_sent(doc, 3), None);
    }

    #[test]
    fn image_names() {
        for yes in [
            "IMG.JPG", "a.heic", "x.gif", "s.tiff", "b.bmp", "c.webp", "d.Png",
        ] {
            assert!(looks_like_image(yes), "{yes}");
        }
        for no in ["doc.pdf", "jpg", "", "a.jpg.txt", "clip.mp4"] {
            assert!(!looks_like_image(no), "{no}");
        }
    }
}
