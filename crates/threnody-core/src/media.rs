//! Strips identifying metadata from images before they are sent.
//!
//! Photos carry EXIF (GPS position, camera serial numbers, time), XMP,
//! comments and, from some phones, data after the image such as the video
//! of a motion photo. All of it is removed without re-encoding, so the
//! picture itself is unchanged. What decoders need to show the image as
//! intended is kept: colour profiles, transparency, animation, and a
//! JPEG's orientation (rewritten as a minimal EXIF block with nothing
//! else in it).
//!
//! JPEG, PNG and WebP are understood. Anything else is left alone and
//! reported as [`Stripped::Unsupported`], so callers can decide.

use crate::{Error, Result};

/// What [`strip`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Stripped {
    /// A supported image, now without metadata (possibly unchanged).
    Image(Vec<u8>),
    /// Not a JPEG, PNG or WebP image: returned untouched.
    Unsupported(Vec<u8>),
}

/// Image kinds recognised by their first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    Webp,
}

pub fn kind(data: &[u8]) -> Option<Kind> {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else {
        None
    }
}

/// Removes metadata from `data` if it is an image we understand. A
/// damaged image of a supported kind is an error rather than being sent
/// with its metadata.
pub fn strip(data: Vec<u8>) -> Result<Stripped> {
    Ok(match kind(&data) {
        Some(Kind::Jpeg) => Stripped::Image(strip_jpeg(&data)?),
        Some(Kind::Png) => Stripped::Image(strip_png(&data)?),
        Some(Kind::Webp) => Stripped::Image(strip_webp(&data)?),
        None => Stripped::Unsupported(data),
    })
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
            // JFIF, and Adobe's colour transform flag, which decoders need.
            0xE0 | 0xEE => true,
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
        match &chunk[..4] {
            b"EXIF" | b"XMP " => {}
            ty => {
                if ty == b"VP8X" {
                    vp8x = Some(out.len());
                }
                out.extend_from_slice(chunk);
            }
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
        let mut w = b"RIFF".to_vec();
        w.extend_from_slice(&u32::try_from(body.len()).unwrap().to_le_bytes());
        w.extend(body);
        let Stripped::Image(out) = strip(w).unwrap() else {
            panic!()
        };
        assert!(!contains(&out, b"GPS") && !contains(&out, b"xmp"));
        assert!(contains(&out, b"pixels!"));
        assert_eq!(out[20], 0x10, "EXIF and XMP flags cleared, alpha kept");
        let riff = u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, out.len());
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let seeds = [
            jpeg(&exif(6, true)),
            b"\x89PNG\r\n\x1a\n\0\0\0\x01IHDRx".to_vec(),
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
}
