//! Chat themes (Appendix W): a chat's shared background, accent colour
//! and (for groups) picture, set by either side of a chat with a contact
//! and by a group's owners and admins; the newest wins. Each person can
//! keep their own plain look in any chat (`set_own_look`).
//!
//! Patterns come as shapes (`pattern_shapes`) that every app draws the
//! same way; photos travel inside the theme, stripped of metadata here
//! before they go.

use threnody_core::history::ConversationId;
use threnody_core::theme::{self, Background, Pattern, Picture, Shape, Theme};

use crate::{Result, ThrenodyNode, fail};

/// A chat's look, as the app shows or sets it. Colours are `0xAARRGGBB`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThemeInfo {
    pub background: BackgroundInfo,
    /// The chat's accent (our bubbles, its buttons); none keeps the app's.
    pub accent: Option<u32>,
    /// A group's picture, in place of its initial (ignored in a chat with
    /// a contact).
    pub picture: Option<PictureInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum BackgroundInfo {
    Plain,
    /// `pattern_shapes(pattern)` in `ink`, repeated on `paper`.
    Pattern {
        pattern: PatternInfo,
        ink: u32,
        paper: u32,
    },
    Color {
        color: u32,
    },
    Gradient {
        top: u32,
        bottom: u32,
    },
    /// A photo, drawn dimmed behind the messages.
    Photo {
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PictureInfo {
    /// An emoji on a round field of `color`.
    Emoji {
        emoji: String,
        color: u32,
    },
    Photo {
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PatternInfo {
    Snow,
    Pines,
    Hearts,
    Confetti,
    Leaves,
    Waves,
    Stars,
    Dots,
}

/// A shape of a pattern's tile, in a unit square (y downwards): filled in
/// the ink colour, the tile repeated.
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum ShapeInfo {
    Circle {
        x: f32,
        y: f32,
        r: f32,
    },
    /// A line with round ends.
    Line {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        width: f32,
    },
    Polygon {
        xs: Vec<f32>,
        ys: Vec<f32>,
    },
}

/// A ready-made theme, set in one tap.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThemePackInfo {
    pub name: String,
    pub emoji: String,
    pub theme: ThemeInfo,
}

impl From<Pattern> for PatternInfo {
    fn from(p: Pattern) -> Self {
        match p {
            Pattern::Snow => Self::Snow,
            Pattern::Pines => Self::Pines,
            Pattern::Hearts => Self::Hearts,
            Pattern::Confetti => Self::Confetti,
            Pattern::Leaves => Self::Leaves,
            Pattern::Waves => Self::Waves,
            Pattern::Stars => Self::Stars,
            Pattern::Dots => Self::Dots,
        }
    }
}

impl From<PatternInfo> for Pattern {
    fn from(p: PatternInfo) -> Self {
        match p {
            PatternInfo::Snow => Self::Snow,
            PatternInfo::Pines => Self::Pines,
            PatternInfo::Hearts => Self::Hearts,
            PatternInfo::Confetti => Self::Confetti,
            PatternInfo::Leaves => Self::Leaves,
            PatternInfo::Waves => Self::Waves,
            PatternInfo::Stars => Self::Stars,
            PatternInfo::Dots => Self::Dots,
        }
    }
}

impl From<&Theme> for ThemeInfo {
    fn from(t: &Theme) -> Self {
        Self {
            background: match &t.background {
                Background::Plain => BackgroundInfo::Plain,
                Background::Pattern {
                    pattern,
                    ink,
                    paper,
                } => BackgroundInfo::Pattern {
                    pattern: (*pattern).into(),
                    ink: *ink,
                    paper: *paper,
                },
                Background::Color(c) => BackgroundInfo::Color { color: *c },
                Background::Gradient(a, b) => BackgroundInfo::Gradient {
                    top: *a,
                    bottom: *b,
                },
                Background::Photo(d) => BackgroundInfo::Photo { data: d.clone() },
            },
            accent: t.accent,
            picture: t.picture.as_ref().map(|p| match p {
                Picture::Emoji { emoji, color } => PictureInfo::Emoji {
                    emoji: emoji.clone(),
                    color: *color,
                },
                Picture::Photo(d) => PictureInfo::Photo { data: d.clone() },
            }),
        }
    }
}

/// `t` as the node keeps it, its photos stripped of metadata.
fn to_theme(t: ThemeInfo) -> Result<Theme> {
    let strip = |d: Vec<u8>| -> Result<Vec<u8>> {
        match threnody_core::media::strip(d).map_err(fail)? {
            threnody_core::media::Stripped::Image(b) => Ok(b),
            threnody_core::media::Stripped::Unsupported(_) => {
                Err(fail("a theme's photo must be a JPEG, PNG or WebP"))
            }
        }
    };
    let theme = Theme {
        background: match t.background {
            BackgroundInfo::Plain => Background::Plain,
            BackgroundInfo::Pattern {
                pattern,
                ink,
                paper,
            } => Background::Pattern {
                pattern: pattern.into(),
                ink,
                paper,
            },
            BackgroundInfo::Color { color } => Background::Color(color),
            BackgroundInfo::Gradient { top, bottom } => Background::Gradient(top, bottom),
            BackgroundInfo::Photo { data } => Background::Photo(strip(data)?),
        },
        accent: t.accent,
        picture: match t.picture {
            None => None,
            Some(PictureInfo::Emoji { emoji, color }) => Some(Picture::Emoji { emoji, color }),
            Some(PictureInfo::Photo { data }) => Some(Picture::Photo(strip(data)?)),
        },
        set_ms: 0,
    };
    theme.check().map_err(fail)?;
    Ok(theme)
}

#[uniffi::export]
impl ThrenodyNode {
    /// Our chat with `peer`'s theme, if one was set.
    pub fn chat_theme(&self, peer: String) -> Result<Option<ThemeInfo>> {
        let p = self.resolve(&peer)?;
        Ok(self
            .node
            .chat_theme(self.node.conversation_for(&p))
            .as_ref()
            .map(Into::into))
    }

    /// Sets our chat with `peer`'s theme for both of us (`None`: back to
    /// plain). A photo in it must be a JPEG, PNG or WebP within the limits
    /// (`max_background_photo`); apps shrink one first.
    pub fn set_chat_theme(&self, peer: String, theme: Option<ThemeInfo>) -> Result<()> {
        let p = self.resolve(&peer)?;
        let t = match theme {
            Some(t) => to_theme(t)?,
            None => Theme::plain(0),
        };
        let _guard = self.rt.enter();
        self.node.set_chat_theme(&p, t).map_err(fail)
    }

    /// `group`'s theme, and picture, if set.
    pub fn group_theme(&self, group: String) -> Result<Option<ThemeInfo>> {
        let g = self.group_id(&group)?;
        Ok(self
            .node
            .chat_theme(ConversationId::Group(g))
            .as_ref()
            .map(Into::into))
    }

    /// Sets `group`'s theme and picture for every member (owners and
    /// admins only; `None`: back to plain).
    pub fn set_group_theme(&self, group: String, theme: Option<ThemeInfo>) -> Result<()> {
        let g = self.group_id(&group)?;
        let t = match theme {
            Some(t) => to_theme(t)?,
            None => Theme::plain(0),
        };
        let _guard = self.rt.enter();
        self.group_node().set_theme(&self.node, &g, t).map_err(fail)
    }

    /// Our notes' look: ours alone.
    pub fn note_theme(&self) -> Option<ThemeInfo> {
        self.node
            .chat_theme(self.node.notes())
            .as_ref()
            .map(Into::into)
    }

    pub fn set_note_theme(&self, theme: Option<ThemeInfo>) -> Result<()> {
        let mut t = match theme {
            Some(t) => to_theme(t)?,
            None => Theme::plain(0),
        };
        t.picture = None;
        t.set_ms = threnody_core::now_ms();
        let me = *self.node.identity().as_bytes();
        self.node.take_theme(self.node.notes(), t, me, true);
        Ok(())
    }

    /// Whether we keep our own plain look in our chat with `peer`.
    pub fn own_look(&self, peer: String) -> Result<bool> {
        let p = self.resolve(&peer)?;
        Ok(self.node.own_look(self.node.conversation_for(&p)))
    }

    /// Keeps (or stops keeping) our own plain look in our chat with
    /// `peer`, whatever its theme: only here, never sent.
    pub fn set_own_look(&self, peer: String, own: bool) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node
            .set_own_look(self.node.conversation_for(&p), own)
            .map_err(fail)
    }

    pub fn group_own_look(&self, group: String) -> Result<bool> {
        let g = self.group_id(&group)?;
        Ok(self.node.own_look(ConversationId::Group(g)))
    }

    pub fn set_group_own_look(&self, group: String, own: bool) -> Result<()> {
        let g = self.group_id(&group)?;
        self.node
            .set_own_look(ConversationId::Group(g), own)
            .map_err(fail)
    }

    /// Whether we may set `group`'s theme: its owners and admins may.
    pub fn may_theme_group(&self, group: String) -> Result<bool> {
        let g = self.group_id(&group)?;
        Ok(self.group_node().may_pin(&g, &self.node.identity()))
    }
}

/// The built-in theme packs.
#[uniffi::export]
pub fn theme_packs() -> Vec<ThemePackInfo> {
    theme::packs()
        .iter()
        .map(|p| ThemePackInfo {
            name: p.name.into(),
            emoji: p.emoji.into(),
            theme: (&p.theme).into(),
        })
        .collect()
}

/// The patterns on offer, in order.
#[uniffi::export]
pub fn theme_patterns() -> Vec<PatternInfo> {
    Pattern::ALL.into_iter().map(Into::into).collect()
}

/// A pattern's name, as the picker shows it.
#[uniffi::export]
pub fn pattern_name(pattern: PatternInfo) -> String {
    Pattern::from(pattern).name().into()
}

/// The solid colours offered.
#[uniffi::export]
pub fn theme_colors() -> Vec<u32> {
    theme::COLORS.to_vec()
}

/// The gradients offered: top and bottom, in pairs.
#[uniffi::export]
pub fn theme_gradients() -> Vec<u32> {
    theme::GRADIENTS
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .collect()
}

/// A pattern's tile, as shapes to fill in its ink and repeat.
#[uniffi::export]
pub fn pattern_shapes(pattern: PatternInfo) -> Vec<ShapeInfo> {
    Pattern::from(pattern)
        .shapes()
        .into_iter()
        .map(|s| match s {
            Shape::Circle { x, y, r } => ShapeInfo::Circle { x, y, r },
            Shape::Line {
                x1,
                y1,
                x2,
                y2,
                width,
            } => ShapeInfo::Line {
                x1,
                y1,
                x2,
                y2,
                width,
            },
            Shape::Polygon { points } => ShapeInfo::Polygon {
                xs: points.iter().map(|p| p.0).collect(),
                ys: points.iter().map(|p| p.1).collect(),
            },
        })
        .collect()
}

/// `color` (a theme's background colour) for the dark theme.
#[uniffi::export]
pub fn night_color(color: u32) -> u32 {
    theme::night(color)
}

/// The largest photo background and group picture, in bytes.
#[uniffi::export]
pub fn max_background_photo() -> u32 {
    theme::MAX_BACKGROUND_PHOTO as u32
}

#[uniffi::export]
pub fn max_picture_photo() -> u32 {
    theme::MAX_PICTURE_PHOTO as u32
}
