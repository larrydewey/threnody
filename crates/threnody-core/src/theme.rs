//! Chat themes (Appendix W): a chat's shared background, accent colour
//! and (for groups) picture.
//!
//! In a chat with a contact, either side may set it; in a group, its
//! owners and admins. The newest setting wins (`set_ms`). Each person can
//! still keep their own plain look on their devices, which is local and
//! never sent.
//!
//! Patterns travel as a number and two colours, and every app draws them
//! from the same shapes ([`Pattern::shapes`]), so they look alike
//! everywhere and cost a few bytes. Photos (a background, a group's
//! picture) travel inside the theme, stripped of metadata and capped in
//! size.

use crate::cbor;
use crate::error::{Error, Result};

/// The largest photo background, in bytes (a phone-sized JPEG).
pub const MAX_BACKGROUND_PHOTO: usize = 384 * 1024;
/// The largest group picture, in bytes.
pub const MAX_PICTURE_PHOTO: usize = 128 * 1024;
/// The longest emoji a picture may be (a flag, a family), in bytes.
const MAX_EMOJI: usize = 32;

/// A chat's theme. Colours are `0xAARRGGBB`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub background: Background,
    /// The colour of our bubbles and buttons in the chat; `None` keeps
    /// the app's own.
    pub accent: Option<u32>,
    /// A group's picture, in place of its initial.
    pub picture: Option<Picture>,
    /// When it was chosen: the newest choice wins.
    pub set_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Background {
    /// The app's own plain background.
    #[default]
    Plain,
    /// A repeating pattern in `ink` on `paper`.
    Pattern {
        pattern: Pattern,
        ink: u32,
        paper: u32,
    },
    Color(u32),
    /// From the top colour to the bottom one.
    Gradient(u32, u32),
    /// A photo (JPEG, PNG or WebP), drawn dimmed behind the messages.
    Photo(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Picture {
    /// An emoji on a round field of `color`.
    Emoji { emoji: String, color: u32 },
    /// A photo (JPEG, PNG or WebP).
    Photo(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    Snow = 1,
    Pines = 2,
    Hearts = 3,
    Confetti = 4,
    Leaves = 5,
    Waves = 6,
    Stars = 7,
    Dots = 8,
}

impl Pattern {
    pub const ALL: [Pattern; 8] = [
        Pattern::Snow,
        Pattern::Pines,
        Pattern::Hearts,
        Pattern::Confetti,
        Pattern::Leaves,
        Pattern::Waves,
        Pattern::Stars,
        Pattern::Dots,
    ];

    pub fn from_id(id: u64) -> Option<Self> {
        Self::ALL.into_iter().find(|p| *p as u64 == id)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Snow => "Snow",
            Self::Pines => "Pines",
            Self::Hearts => "Hearts",
            Self::Confetti => "Confetti",
            Self::Leaves => "Leaves",
            Self::Waves => "Waves",
            Self::Stars => "Stars",
            Self::Dots => "Dots",
        }
    }

    /// The pattern's tile: shapes in a unit square (0 to 1 each way,
    /// y downwards), to be filled in the ink colour and repeated. Lines
    /// give their width in the same units. The tile wraps: what leaves
    /// one edge comes in at the other.
    pub fn shapes(self) -> Vec<Shape> {
        let mut out = Vec::new();
        match self {
            Self::Snow => {
                for (x, y, r) in [(0.27, 0.3, 0.11), (0.76, 0.74, 0.08)] {
                    flake(&mut out, x, y, r);
                }
                for (x, y) in [(0.78, 0.22), (0.2, 0.8), (0.52, 0.5), (0.93, 0.48)] {
                    out.push(Shape::Circle { x, y, r: 0.018 });
                }
            }
            Self::Pines => {
                for (x, y, s) in [(0.27, 0.36, 0.22), (0.75, 0.82, 0.16)] {
                    pine(&mut out, x, y, s);
                }
                for (x, y) in [(0.7, 0.2), (0.15, 0.85), (0.45, 0.62)] {
                    out.push(Shape::Circle { x, y, r: 0.016 });
                }
            }
            Self::Hearts => {
                heart(&mut out, 0.28, 0.3, 0.13, -0.2);
                heart(&mut out, 0.76, 0.76, 0.09, 0.25);
                out.push(Shape::Circle {
                    x: 0.78,
                    y: 0.24,
                    r: 0.02,
                });
                out.push(Shape::Circle {
                    x: 0.2,
                    y: 0.78,
                    r: 0.015,
                });
            }
            Self::Confetti => {
                let bits = [
                    (0.15, 0.2, 0.6),
                    (0.55, 0.12, -0.4),
                    (0.85, 0.42, 1.1),
                    (0.35, 0.55, -0.9),
                    (0.7, 0.8, 0.3),
                    (0.12, 0.85, 1.4),
                ];
                for (x, y, a) in bits {
                    out.push(Shape::Polygon {
                        points: rotated_rect(x, y, 0.07, 0.025, a),
                    });
                }
                for (x, y) in [(0.4, 0.3), (0.9, 0.12), (0.6, 0.6), (0.3, 0.9)] {
                    out.push(Shape::Circle { x, y, r: 0.022 });
                }
            }
            Self::Leaves => {
                leaf(&mut out, 0.28, 0.32, 0.14, 0.7);
                leaf(&mut out, 0.74, 0.76, 0.11, -0.6);
                out.push(Shape::Circle {
                    x: 0.8,
                    y: 0.22,
                    r: 0.016,
                });
            }
            Self::Waves => {
                for y in [0.25, 0.75] {
                    let n = 24;
                    for i in 0..n {
                        let x1 = i as f32 / n as f32;
                        let x2 = (i + 1) as f32 / n as f32;
                        let at = |x: f32| y + 0.05 * (x * std::f32::consts::TAU).sin();
                        out.push(Shape::Line {
                            x1,
                            y1: at(x1),
                            x2,
                            y2: at(x2),
                            width: 0.022,
                        });
                    }
                }
            }
            Self::Stars => {
                star(&mut out, 0.27, 0.3, 0.1);
                star(&mut out, 0.76, 0.75, 0.07);
                for (x, y, r) in [(0.75, 0.2, 0.016), (0.18, 0.8, 0.02), (0.5, 0.55, 0.012)] {
                    out.push(Shape::Circle { x, y, r });
                }
            }
            Self::Dots => {
                for (x, y) in [(0.25, 0.25), (0.75, 0.75)] {
                    out.push(Shape::Circle { x, y, r: 0.07 });
                }
            }
        }
        out
    }
}

/// A shape of a pattern's tile (see [`Pattern::shapes`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// A filled circle.
    Circle { x: f32, y: f32, r: f32 },
    /// A line with round ends.
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
    },
    /// A filled polygon.
    Polygon { points: Vec<(f32, f32)> },
}

fn flake(out: &mut Vec<Shape>, x: f32, y: f32, r: f32) {
    for k in 0..3 {
        let a = k as f32 * std::f32::consts::PI / 3.0;
        let (dx, dy) = (r * a.cos(), r * a.sin());
        out.push(Shape::Line {
            x1: x - dx,
            y1: y - dy,
            x2: x + dx,
            y2: y + dy,
            width: r * 0.16,
        });
    }
    out.push(Shape::Circle { x, y, r: r * 0.18 });
}

fn pine(out: &mut Vec<Shape>, x: f32, y: f32, s: f32) {
    for (top, half, base) in [(-0.5, 0.32, -0.05), (-0.25, 0.42, 0.2)] {
        out.push(Shape::Polygon {
            points: vec![
                (x, y + top * s),
                (x + half * s, y + base * s),
                (x - half * s, y + base * s),
            ],
        });
    }
    out.push(Shape::Line {
        x1: x,
        y1: y + 0.2 * s,
        x2: x,
        y2: y + 0.38 * s,
        width: 0.12 * s,
    });
}

fn heart(out: &mut Vec<Shape>, x: f32, y: f32, s: f32, tilt: f32) {
    let points = (0..32)
        .map(|i| {
            let t = i as f32 / 32.0 * std::f32::consts::TAU;
            let hx = 16.0 * t.sin().powi(3) / 17.0;
            let hy =
                -(13.0 * t.cos() - 5.0 * (2.0 * t).cos() - 2.0 * (3.0 * t).cos() - (4.0 * t).cos())
                    / 17.0;
            let (c, sn) = (tilt.cos(), tilt.sin());
            (x + s * (hx * c - hy * sn), y + s * (hx * sn + hy * c))
        })
        .collect();
    out.push(Shape::Polygon { points });
}

fn rotated_rect(x: f32, y: f32, w: f32, h: f32, a: f32) -> Vec<(f32, f32)> {
    let (c, s) = (a.cos(), a.sin());
    [(-w, -h), (w, -h), (w, h), (-w, h)]
        .iter()
        .map(|(dx, dy)| (x + dx * c - dy * s, y + dx * s + dy * c))
        .collect()
}

fn leaf(out: &mut Vec<Shape>, x: f32, y: f32, s: f32, a: f32) {
    let (c, sn) = (a.cos(), a.sin());
    let at = |u: f32, v: f32| (x + s * (u * c - v * sn), y + s * (u * sn + v * c));
    let n = 12;
    let mut points = Vec::with_capacity(2 * n);
    for i in 0..=n {
        let u = -1.0 + 2.0 * i as f32 / n as f32;
        points.push(at(u, 0.45 * (1.0 - u * u)));
    }
    for i in (1..n).rev() {
        let u = -1.0 + 2.0 * i as f32 / n as f32;
        points.push(at(u, -0.45 * (1.0 - u * u)));
    }
    out.push(Shape::Polygon { points });
    let (a1, a2) = (at(-1.25, 0.0), at(-0.95, 0.0));
    out.push(Shape::Line {
        x1: a1.0,
        y1: a1.1,
        x2: a2.0,
        y2: a2.1,
        width: 0.08 * s,
    });
}

fn star(out: &mut Vec<Shape>, x: f32, y: f32, r: f32) {
    let points = (0..10)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            let d = if i % 2 == 0 { r } else { r * 0.45 };
            (x + d * a.cos(), y + d * a.sin())
        })
        .collect();
    out.push(Shape::Polygon { points });
}

/// A ready-made theme: background, accent and (for groups) a picture.
pub struct Pack {
    pub name: &'static str,
    pub emoji: &'static str,
    pub theme: Theme,
}

/// The built-in theme packs, each set in one tap.
pub fn packs() -> Vec<Pack> {
    let pack = |name, emoji: &'static str, pattern, ink, paper, accent, field| Pack {
        name,
        emoji,
        theme: Theme {
            background: Background::Pattern {
                pattern,
                ink,
                paper,
            },
            accent: Some(accent),
            picture: Some(Picture::Emoji {
                emoji: emoji.to_owned(),
                color: field,
            }),
            set_ms: 0,
        },
    };
    vec![
        pack(
            "Christmas",
            "🎄",
            Pattern::Pines,
            0x402E7D32,
            0xFFFFF5F2,
            0xFFC62828,
            0xFFFFE3DE,
        ),
        pack(
            "Winter",
            "⛄",
            Pattern::Snow,
            0x405C8DB8,
            0xFFEEF5FB,
            0xFF1E6FB8,
            0xFFDCEBF7,
        ),
        pack(
            "Halloween",
            "🎃",
            Pattern::Stars,
            0x40E65100,
            0xFFFFF3E6,
            0xFF6A1B9A,
            0xFFFFE2C4,
        ),
        pack(
            "Birthday",
            "🎂",
            Pattern::Confetti,
            0x40D81B60,
            0xFFFFF7FB,
            0xFFD81B60,
            0xFFFFE0EF,
        ),
        pack(
            "Love",
            "💕",
            Pattern::Hearts,
            0x40E91E63,
            0xFFFFF0F3,
            0xFFC2185B,
            0xFFFFDDE6,
        ),
        pack(
            "Beach",
            "🏖️",
            Pattern::Waves,
            0x4000897B,
            0xFFF1FAF9,
            0xFF00897B,
            0xFFD5F2EE,
        ),
        pack(
            "Autumn",
            "🍂",
            Pattern::Leaves,
            0x40D2691E,
            0xFFFFF6EC,
            0xFFBF5B17,
            0xFFFFE7CC,
        ),
        pack(
            "Night sky",
            "🌙",
            Pattern::Stars,
            0x403F51B5,
            0xFFEEF0FA,
            0xFF3949AB,
            0xFFDDE1F7,
        ),
    ]
}

/// The solid backgrounds offered (light; apps darken them at night).
pub const COLORS: [u32; 10] = [
    0xFFF3F4F8, 0xFFFDECEC, 0xFFFFF3E0, 0xFFFFFBE0, 0xFFEAF7EA, 0xFFE6F4F1, 0xFFE8F1FC, 0xFFEDEBFB,
    0xFFF8E9F4, 0xFFF1ECE6,
];

/// The gradients offered, top to bottom.
pub const GRADIENTS: [(u32, u32); 6] = [
    (0xFFE8F1FC, 0xFFF8E9F4),
    (0xFFFFF3E0, 0xFFFDECEC),
    (0xFFE6F4F1, 0xFFE8F1FC),
    (0xFFEDEBFB, 0xFFE6F4F1),
    (0xFFFFFBE0, 0xFFEAF7EA),
    (0xFF1E2A4A, 0xFF5B3A6E),
];

/// `c` for the dark theme: a light background colour turned deep (its
/// hue kept), so the messages stay readable; dark colours stay.
pub fn night(c: u32) -> u32 {
    let (a, r, g, b) = (c >> 24, (c >> 16) & 0xFF, (c >> 8) & 0xFF, c & 0xFF);
    let light = (r * 299 + g * 587 + b * 114) / 1000;
    if light < 128 {
        return c;
    }
    // Keep the tint (how far each channel is from the brightest), on a
    // dark base.
    let top = r.max(g).max(b);
    let deep = |v: u32| 22 + (v + 255 - top) * 18 / 255 + (255 - top) / 2;
    let (r, g, b) = (deep(r).min(255), deep(g).min(255), deep(b).min(255));
    (a << 24) | (r << 16) | (g << 8) | b
}

const BG_PLAIN: u64 = 0;
const BG_PATTERN: u64 = 1;
const BG_COLOR: u64 = 2;
const BG_GRADIENT: u64 = 3;
const BG_PHOTO: u64 = 4;
const PIC_EMOJI: u64 = 0;
const PIC_PHOTO: u64 = 1;

impl Theme {
    /// The plain look, chosen at `set_ms` (to take a theme away).
    pub fn plain(set_ms: u64) -> Self {
        Self {
            background: Background::Plain,
            accent: None,
            picture: None,
            set_ms,
        }
    }

    /// Whether it changes anything at all.
    pub fn is_plain(&self) -> bool {
        self.background == Background::Plain && self.accent.is_none() && self.picture.is_none()
    }

    /// Checks what a theme may hold: photos within their limits, and of
    /// kinds whose metadata is stripped; an emoji that is short and
    /// printable. Senders strip photos before sending (see
    /// [`crate::media::strip`]); receivers drop a theme that fails.
    pub fn check(&self) -> Result<()> {
        let image = |b: &[u8], max: usize| -> Result<()> {
            if b.len() > max {
                return Err(Error::Malformed("theme photo too large"));
            }
            match crate::media::kind(b) {
                Some(
                    crate::media::Kind::Jpeg | crate::media::Kind::Png | crate::media::Kind::Webp,
                ) => Ok(()),
                _ => Err(Error::Malformed("theme photo kind")),
            }
        };
        if let Background::Photo(b) = &self.background {
            image(b, MAX_BACKGROUND_PHOTO)?;
        }
        match &self.picture {
            Some(Picture::Photo(b)) => image(b, MAX_PICTURE_PHOTO)?,
            Some(Picture::Emoji { emoji, .. })
                if emoji.is_empty()
                    || emoji.len() > MAX_EMOJI
                    || emoji.chars().any(char::is_control) =>
            {
                return Err(Error::Malformed("theme emoji"));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let photo = match &self.background {
            Background::Photo(b) => b.len(),
            _ => 0,
        } + match &self.picture {
            Some(Picture::Photo(b)) => b.len(),
            _ => 0,
        };
        cbor::to_vec(photo + 128, |e| {
            e.map_len(
                2 + usize::from(self.accent.is_some()) + usize::from(self.picture.is_some()),
            )?;
            e.u8(0)?.u64(self.set_ms)?;
            e.u8(1)?;
            match &self.background {
                Background::Plain => {
                    e.map_len(1)?.u8(0)?.uint(BG_PLAIN)?;
                }
                Background::Pattern {
                    pattern,
                    ink,
                    paper,
                } => {
                    e.map_len(4)?.u8(0)?.uint(BG_PATTERN)?;
                    e.u8(1)?.uint(*pattern as u64)?;
                    e.u8(2)?.u32(*ink)?;
                    e.u8(3)?.u32(*paper)?;
                }
                Background::Color(c) => {
                    e.map_len(2)?.u8(0)?.uint(BG_COLOR)?;
                    e.u8(2)?.u32(*c)?;
                }
                Background::Gradient(a, b) => {
                    e.map_len(3)?.u8(0)?.uint(BG_GRADIENT)?;
                    e.u8(2)?.u32(*a)?;
                    e.u8(3)?.u32(*b)?;
                }
                Background::Photo(b) => {
                    e.map_len(2)?.u8(0)?.uint(BG_PHOTO)?;
                    e.u8(4)?.bytes(b)?;
                }
            }
            if let Some(a) = self.accent {
                e.u8(2)?.u32(a)?;
            }
            if let Some(p) = &self.picture {
                e.u8(3)?;
                match p {
                    Picture::Emoji { emoji, color } => {
                        e.map_len(3)?.u8(0)?.uint(PIC_EMOJI)?;
                        e.u8(1)?.str(emoji)?;
                        e.u8(2)?.u32(*color)?;
                    }
                    Picture::Photo(b) => {
                        e.map_len(2)?.u8(0)?.uint(PIC_PHOTO)?;
                        e.u8(3)?.bytes(b)?;
                    }
                }
            }
            Ok(())
        })
    }

    /// The theme in `b`, checked ([`Theme::check`]). A background or
    /// picture of a kind this version doesn't know reads as none.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = const_cbor::Decoder::new(b);
        let (mut set_ms, mut background, mut accent, mut picture) = (None, None, None, None);
        cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => set_ms = Some(d.u64()?),
                1 => background = Some(decode_background(d)?),
                2 => accent = Some(d.u32()?),
                3 => picture = decode_picture(d)?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        cbor::finish(&dec)?;
        let t = Self {
            background: background.unwrap_or_default(),
            accent,
            picture,
            set_ms: cbor::required(set_ms, "theme time")?,
        };
        t.check()?;
        Ok(t)
    }
}

fn decode_background(d: &mut const_cbor::Decoder<'_>) -> Result<Background> {
    let (mut kind, mut pattern, mut two, mut three, mut photo) = (0, 0, 0, 0, None);
    cbor::read_map(d, |k, d| {
        match k {
            0 => kind = d.u64()?,
            1 => pattern = d.u64()?,
            2 => two = d.u32()?,
            3 => three = d.u32()?,
            4 => photo = Some(d.bytes()?.to_vec()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(match kind {
        BG_PATTERN => match Pattern::from_id(pattern) {
            Some(pattern) => Background::Pattern {
                pattern,
                ink: two,
                paper: three,
            },
            None => Background::Plain,
        },
        BG_COLOR => Background::Color(two),
        BG_GRADIENT => Background::Gradient(two, three),
        BG_PHOTO => photo.map_or(Background::Plain, Background::Photo),
        _ => Background::Plain,
    })
}

fn decode_picture(d: &mut const_cbor::Decoder<'_>) -> Result<Option<Picture>> {
    let (mut kind, mut emoji, mut color, mut photo) = (0, None, 0, None);
    cbor::read_map(d, |k, d| {
        match k {
            0 => kind = d.u64()?,
            1 => emoji = Some(d.str()?.to_owned()),
            2 => color = d.u32()?,
            3 => photo = Some(d.bytes()?.to_vec()),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    Ok(match kind {
        PIC_EMOJI => emoji.map(|emoji| Picture::Emoji { emoji, color }),
        PIC_PHOTO => photo.map(Picture::Photo),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10];

    #[test]
    fn themes_round_trip_and_check_what_they_carry() {
        for t in packs().into_iter().map(|p| p.theme).chain([
            Theme::plain(9),
            Theme {
                background: Background::Gradient(1, 2),
                accent: None,
                picture: Some(Picture::Photo(JPEG.to_vec())),
                set_ms: 5,
            },
            Theme {
                background: Background::Photo(JPEG.to_vec()),
                accent: Some(0xFF00_0000),
                picture: None,
                set_ms: 7,
            },
        ]) {
            assert_eq!(Theme::decode(&t.encode().unwrap()).unwrap(), t);
        }
        let big = Theme {
            background: Background::Photo([JPEG, &vec![0; MAX_BACKGROUND_PHOTO]].concat()),
            ..Theme::plain(1)
        };
        assert!(Theme::decode(&big.encode().unwrap()).is_err(), "too large");
        let odd = Theme {
            background: Background::Photo(b"GIF89a....".to_vec()),
            ..Theme::plain(1)
        };
        assert!(
            Theme::decode(&odd.encode().unwrap()).is_err(),
            "not a still photo"
        );
        let control = Theme {
            picture: Some(Picture::Emoji {
                emoji: "\u{7}".into(),
                color: 0,
            }),
            ..Theme::plain(1)
        };
        assert!(control.check().is_err());
        assert!(Theme::plain(3).is_plain());
    }

    #[test]
    fn every_pattern_has_shapes_inside_its_tile() {
        for p in Pattern::ALL {
            assert_eq!(Pattern::from_id(p as u64), Some(p));
            let shapes = p.shapes();
            assert!(!shapes.is_empty(), "{}", p.name());
            let inside = |x: f32, y: f32| (-0.2..=1.2).contains(&x) && (-0.2..=1.2).contains(&y);
            for s in shapes {
                match s {
                    Shape::Circle { x, y, .. } => assert!(inside(x, y)),
                    Shape::Line { x1, y1, x2, y2, .. } => assert!(inside(x1, y1) && inside(x2, y2)),
                    Shape::Polygon { points } => {
                        assert!(points.iter().all(|(x, y)| inside(*x, *y)))
                    }
                }
            }
        }
    }

    #[test]
    fn night_deepens_light_colours_and_keeps_dark_ones() {
        let n = night(0xFFFDECEC);
        let light = |c: u32| {
            ((c >> 16) & 0xFF) * 299 / 1000
                + ((c >> 8) & 0xFF) * 587 / 1000
                + (c & 0xFF) * 114 / 1000
        };
        assert!(light(n) < 60, "{n:x}");
        assert!((n >> 16) & 0xFF > n & 0xFF, "the red tint stays");
        assert_eq!(night(0xFF1E2A4A), 0xFF1E2A4A);
    }
}
