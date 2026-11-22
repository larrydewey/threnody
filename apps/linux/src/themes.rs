//! Chat themes (Appendix W): drawing a chat's shared look behind its
//! messages, and the dialog that chooses one. The same shapes and colours
//! as on every other device; in the dark style, light backgrounds deepen.

use adw::prelude::*;
use gtk::gdk;
use gtk::gdk::gdk_pixbuf::{Pixbuf, PixbufLoader};
use std::cell::RefCell;
use std::rc::Rc;
use threnody_ffi::{BackgroundInfo, PatternInfo, PictureInfo, ShapeInfo, ThemeInfo};

/// Accent colours on offer (the same on every app).
pub const ACCENTS: [u32; 8] = [
    0xFF4F46E5, 0xFF1E6FB8, 0xFF00897B, 0xFF2E7D32, 0xFFE65100, 0xFFC62828, 0xFFD81B60, 0xFF6A1B9A,
];

fn dark() -> bool {
    adw::StyleManager::default().is_dark()
}

/// `c` as RGBA floats.
fn rgba(c: u32) -> (f64, f64, f64, f64) {
    let ch = |s: u32| f64::from((c >> s) & 0xFF) / 255.0;
    (ch(16), ch(8), ch(0), ch(24))
}

/// `c` for the current style: deepened in the dark one.
fn shade(c: u32) -> u32 {
    if dark() {
        threnody_ffi::night_color(c)
    } else {
        c
    }
}

/// A pattern's ink in the dark style: lighter, to show on deep paper.
fn ink_for(c: u32) -> u32 {
    if !dark() {
        return c;
    }
    let mix = |s: u32| {
        let v = (c >> s) & 0xFF;
        (v + (255 - v) / 2) << s
    };
    (c & 0xFF00_0000) | mix(16) | mix(8) | mix(0)
}

/// `#rrggbb`, for CSS.
pub fn hex(c: u32) -> String {
    format!("#{:06x}", c & 0x00FF_FFFF)
}

/// Text on `c`: white on a dark colour, near black on a light one.
pub fn on_color(c: u32) -> u32 {
    let (r, g, b, _) = rgba(c);
    if 0.299 * r + 0.587 * g + 0.114 * b > 0.63 {
        0xFF1B1B1F
    } else {
        0xFFFFFFFF
    }
}

/// One tile of `pattern`, `side` pixels across, in `ink` on `paper`.
fn tile(pattern: PatternInfo, ink: u32, paper: u32, side: i32) -> Option<gtk::cairo::ImageSurface> {
    let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, side, side).ok()?;
    let cr = gtk::cairo::Context::new(&surface).ok()?;
    let (r, g, b, a) = rgba(paper);
    cr.set_source_rgba(r, g, b, a);
    cr.paint().ok()?;
    let (r, g, b, a) = rgba(ink);
    cr.set_source_rgba(r, g, b, a);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    let s = f64::from(side);
    let shapes = threnody_ffi::pattern_shapes(pattern);
    // Shapes that cross an edge come in again at the other.
    for dx in -1..=1 {
        for dy in -1..=1 {
            cr.save().ok()?;
            cr.translate(f64::from(dx) * s, f64::from(dy) * s);
            for sh in &shapes {
                match sh {
                    ShapeInfo::Circle { x, y, r } => {
                        cr.arc(
                            f64::from(*x) * s,
                            f64::from(*y) * s,
                            f64::from(*r) * s,
                            0.0,
                            std::f64::consts::TAU,
                        );
                        cr.fill().ok()?;
                    }
                    ShapeInfo::Line {
                        x1,
                        y1,
                        x2,
                        y2,
                        width,
                    } => {
                        cr.set_line_width(f64::from(*width) * s);
                        cr.move_to(f64::from(*x1) * s, f64::from(*y1) * s);
                        cr.line_to(f64::from(*x2) * s, f64::from(*y2) * s);
                        cr.stroke().ok()?;
                    }
                    ShapeInfo::Polygon { xs, ys } => {
                        for (i, (x, y)) in xs.iter().zip(ys).enumerate() {
                            let (x, y) = (f64::from(*x) * s, f64::from(*y) * s);
                            if i == 0 {
                                cr.move_to(x, y)
                            } else {
                                cr.line_to(x, y)
                            }
                        }
                        cr.close_path();
                        cr.fill().ok()?;
                    }
                }
            }
            cr.restore().ok()?;
        }
    }
    drop(cr);
    Some(surface)
}

fn pixbuf(data: &[u8]) -> Option<Pixbuf> {
    let loader = PixbufLoader::new();
    loader.write(data).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
}

/// Draws `bg` over `w`×`h`, the pattern tile `tile_px` across; nothing for
/// a plain background.
pub fn draw(cr: &gtk::cairo::Context, w: i32, h: i32, bg: &BackgroundInfo, tile_px: i32) {
    let (w, h) = (f64::from(w), f64::from(h));
    match bg {
        BackgroundInfo::Plain => {}
        BackgroundInfo::Color { color } => {
            let (r, g, b, a) = rgba(shade(*color));
            cr.set_source_rgba(r, g, b, a);
            let _ = cr.paint();
        }
        BackgroundInfo::Gradient { top, bottom } => {
            let grad = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
            let (r, g, b, a) = rgba(shade(*top));
            grad.add_color_stop_rgba(0.0, r, g, b, a);
            let (r, g, b, a) = rgba(shade(*bottom));
            grad.add_color_stop_rgba(1.0, r, g, b, a);
            let _ = cr.set_source(&grad);
            let _ = cr.paint();
        }
        BackgroundInfo::Pattern {
            pattern,
            ink,
            paper,
        } => {
            if let Some(t) = tile(*pattern, ink_for(*ink), shade(*paper), tile_px) {
                let p = gtk::cairo::SurfacePattern::create(&t);
                p.set_extend(gtk::cairo::Extend::Repeat);
                let _ = cr.set_source(&p);
                let _ = cr.paint();
            }
        }
        BackgroundInfo::Photo { data } => {
            let Some(pb) = pixbuf(data) else { return };
            let scale = (w / f64::from(pb.width())).max(h / f64::from(pb.height()));
            let _ = cr.save();
            cr.translate(
                (w - f64::from(pb.width()) * scale) / 2.0,
                (h - f64::from(pb.height()) * scale) / 2.0,
            );
            cr.scale(scale, scale);
            cr.set_source_pixbuf(&pb, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
            // Dimmed, so the messages read.
            if dark() {
                cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
            } else {
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.4);
            }
            let _ = cr.paint();
        }
    }
}

/// A group's picture as an image for an avatar: an emoji on its colour,
/// or the photo.
pub fn picture_texture(picture: &PictureInfo, side: i32) -> Option<gdk::Texture> {
    match picture {
        PictureInfo::Photo { data } => Some(gdk::Texture::for_pixbuf(&pixbuf(data)?)),
        PictureInfo::Emoji { emoji, color } => {
            let surface =
                gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, side, side).ok()?;
            {
                let cr = gtk::cairo::Context::new(&surface).ok()?;
                let (r, g, b, a) = rgba(*color);
                cr.set_source_rgba(r, g, b, a);
                cr.paint().ok()?;
                let layout = pangocairo::functions::create_layout(&cr);
                let mut font = gtk::pango::FontDescription::new();
                font.set_absolute_size(f64::from(side) * 0.55 * f64::from(gtk::pango::SCALE));
                layout.set_font_description(Some(&font));
                layout.set_text(emoji);
                let (_, ext) = layout.pixel_extents();
                cr.move_to(
                    (f64::from(side) - f64::from(ext.width())) / 2.0 - f64::from(ext.x()),
                    (f64::from(side) - f64::from(ext.height())) / 2.0 - f64::from(ext.y()),
                );
                pangocairo::functions::show_layout(&cr, &layout);
            }
            surface.flush();
            let stride = surface.stride() as usize;
            let data = surface.take_data().ok()?.to_vec();
            let bytes = gtk::glib::Bytes::from_owned(data);
            Some(
                gdk::MemoryTexture::new(
                    side,
                    side,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &bytes,
                    stride,
                )
                .upcast(),
            )
        }
    }
}

/// Shows a group's picture on `a`; false (and the initial) if none.
pub fn show_picture(a: &adw::Avatar, picture: Option<&PictureInfo>) -> bool {
    let tex = picture.and_then(|p| picture_texture(p, 128));
    let has = tex.is_some();
    a.set_custom_image(tex.as_ref());
    has
}

/// The CSS for a chat's accent, on widgets under `.{class}`.
pub fn accent_css(class: &str, accent: Option<u32>) -> String {
    let Some(c) = accent else {
        return String::new();
    };
    let (bg, fg) = (hex(c), hex(on_color(c)));
    format!(
        ".{class} .bubble.outgoing {{ background-color: {bg}; color: {fg}; }}\n\
         .{class} .bubble.outgoing .dim-label, .{class} .bubble.outgoing .accent {{ color: {fg}; }}\n\
         .{class} .composer button.suggested-action {{ background-color: {bg}; color: {fg}; }}\n"
    )
}

/// The dialog that chooses a chat's look, with a preview of it. `can_set`
/// false (a member who isn't an owner or admin): only keeping one's own
/// look. `set` gets the theme chosen (`None`: plain).
#[allow(clippy::too_many_arguments)]
pub fn pick(
    parent: &impl IsA<gtk::Widget>,
    current: Option<ThemeInfo>,
    group: bool,
    can_set: bool,
    own: bool,
    title: &str,
    set_own: impl Fn(bool) + 'static,
    set: impl Fn(Option<ThemeInfo>) + 'static,
) {
    let chosen = Rc::new(RefCell::new(current.unwrap_or(ThemeInfo {
        background: BackgroundInfo::Plain,
        accent: None,
        picture: None,
    })));
    let dialog = adw::Dialog::builder()
        .title(if group { "Group theme" } else { "Chat theme" })
        .content_width(520)
        .content_height(640)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 10);
    body.set_margin_start(18);
    body.set_margin_end(18);
    body.set_margin_bottom(18);

    // The preview: the background, a bubble each way, and the picture.
    let preview = gtk::Overlay::new();
    preview.add_css_class("theme-preview");
    preview.set_overflow(gtk::Overflow::Hidden);
    let area = gtk::DrawingArea::new();
    area.set_content_height(if group { 170 } else { 120 });
    let c = chosen.clone();
    area.set_draw_func(move |_, cr, w, h| draw(cr, w, h, &c.borrow().background, 84));
    preview.set_child(Some(&area));
    let bubbles = gtk::Box::new(gtk::Orientation::Vertical, 6);
    bubbles.set_margin_start(14);
    bubbles.set_margin_end(14);
    bubbles.set_margin_top(14);
    let face = adw::Avatar::new(40, Some(title), true);
    face.set_halign(gtk::Align::Start);
    face.set_visible(group);
    bubbles.append(&face);
    let theirs = gtk::Label::new(Some("Merry Christmas! 🎄"));
    theirs.add_css_class("bubble");
    theirs.add_css_class("incoming");
    theirs.set_halign(gtk::Align::Start);
    bubbles.append(&theirs);
    let mine = gtk::Label::new(Some("Same to you!"));
    mine.add_css_class("bubble");
    mine.add_css_class("outgoing");
    mine.set_halign(gtk::Align::End);
    bubbles.append(&mine);
    preview.add_overlay(&bubbles);
    body.append(&preview);
    let css = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    preview.add_css_class("look-preview");
    let redraw = {
        let (c, area, face, css) = (chosen.clone(), area.clone(), face.clone(), css.clone());
        Rc::new(move || {
            area.queue_draw();
            css.load_from_string(&accent_css("look-preview", c.borrow().accent));
            show_picture(&face, c.borrow().picture.as_ref());
        })
    };
    redraw();

    let heading = |t: &str| {
        let l = gtk::Label::new(Some(t));
        l.add_css_class("heading");
        l.add_css_class("accent");
        l.set_halign(gtk::Align::Start);
        l.set_margin_top(6);
        l
    };
    let row = || {
        gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(8)
            .row_spacing(8)
            .max_children_per_line(10)
            .build()
    };
    // A round swatch drawing `bg`, choosing what `pick` sets.
    let swatch = |bg: BackgroundInfo, tip: &str, pick: Box<dyn Fn(&mut ThemeInfo)>| {
        let a = gtk::DrawingArea::new();
        a.set_content_width(44);
        a.set_content_height(44);
        a.set_draw_func(move |_, cr, w, h| {
            let r = f64::from(w.min(h)) / 2.0;
            cr.arc(
                f64::from(w) / 2.0,
                f64::from(h) / 2.0,
                r,
                0.0,
                std::f64::consts::TAU,
            );
            cr.clip();
            cr.set_source_rgba(0.9, 0.9, 0.92, 1.0);
            let _ = cr.paint();
            draw(cr, w, h, &bg, 40);
        });
        let b = gtk::Button::builder().child(&a).tooltip_text(tip).build();
        b.add_css_class("flat");
        b.add_css_class("circular");
        b.add_css_class("theme-swatch");
        let (c, redraw) = (chosen.clone(), redraw.clone());
        b.connect_clicked(move |_| {
            pick(&mut c.borrow_mut());
            redraw();
        });
        b
    };

    if can_set {
        body.append(&heading("Theme packs"));
        let packs = row();
        for p in threnody_ffi::theme_packs() {
            let tile = gtk::Overlay::new();
            let a = gtk::DrawingArea::new();
            a.set_content_width(72);
            a.set_content_height(72);
            let bg = p.theme.background.clone();
            a.set_draw_func(move |_, cr, w, h| draw(cr, w, h, &bg, 44));
            tile.set_child(Some(&a));
            let e = gtk::Label::new(Some(&p.emoji));
            e.add_css_class("title-1");
            tile.add_overlay(&e);
            tile.set_overflow(gtk::Overflow::Hidden);
            tile.add_css_class("theme-pack");
            let v = gtk::Box::new(gtk::Orientation::Vertical, 4);
            v.append(&tile);
            v.append(&gtk::Label::new(Some(&p.name)));
            let b = gtk::Button::builder()
                .child(&v)
                .tooltip_text(&p.name)
                .build();
            b.add_css_class("flat");
            let (c, redraw, theme) = (chosen.clone(), redraw.clone(), p.theme.clone());
            b.connect_clicked(move |_| {
                let mut t = theme.clone();
                if !group {
                    t.picture = None;
                }
                *c.borrow_mut() = t;
                redraw();
            });
            packs.append(&b);
        }
        body.append(&packs);

        body.append(&heading("Patterns"));
        let patterns = row();
        for pat in threnody_ffi::theme_patterns() {
            let paper = 0xFFF3_F4F8;
            let shown = BackgroundInfo::Pattern {
                pattern: pat,
                ink: 0x404F_46E5,
                paper,
            };
            patterns.append(&swatch(
                shown,
                &threnody_ffi::pattern_name(pat),
                Box::new(move |t| {
                    let acc = t.accent.unwrap_or(ACCENTS[0]);
                    t.background = BackgroundInfo::Pattern {
                        pattern: pat,
                        ink: (acc & 0x00FF_FFFF) | 0x4000_0000,
                        paper,
                    };
                }),
            ));
        }
        body.append(&patterns);

        body.append(&heading("Colours"));
        let colors = row();
        for c in threnody_ffi::theme_colors() {
            colors.append(&swatch(
                BackgroundInfo::Color { color: c },
                "Colour",
                Box::new(move |t| t.background = BackgroundInfo::Color { color: c }),
            ));
        }
        let g = threnody_ffi::theme_gradients();
        for pair in g.chunks(2) {
            let (top, bottom) = (pair[0], pair[1]);
            colors.append(&swatch(
                BackgroundInfo::Gradient { top, bottom },
                "Gradient",
                Box::new(move |t| t.background = BackgroundInfo::Gradient { top, bottom }),
            ));
        }
        let photo = gtk::Button::from_icon_name("image-x-generic-symbolic");
        photo.set_tooltip_text(Some("A photo…"));
        photo.add_css_class("circular");
        {
            let (c, redraw, d) = (chosen.clone(), redraw.clone(), dialog.clone());
            photo.connect_clicked(move |_| {
                let (c, redraw) = (c.clone(), redraw.clone());
                choose_photo(&d, false, move |bytes| {
                    c.borrow_mut().background = BackgroundInfo::Photo { data: bytes };
                    redraw();
                });
            });
        }
        colors.append(&photo);
        body.append(&colors);

        body.append(&heading("Accent"));
        let accents = row();
        accents.append(&swatch(
            BackgroundInfo::Plain,
            "The app's own",
            Box::new(|t| t.accent = None),
        ));
        for a in ACCENTS {
            accents.append(&swatch(
                BackgroundInfo::Color { color: a },
                "Accent",
                Box::new(move |t| t.accent = Some(a)),
            ));
        }
        body.append(&accents);

        if group {
            body.append(&heading("Picture"));
            let pics = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let emoji = gtk::MenuButton::builder().label("Emoji").build();
            emoji.add_css_class("pill");
            let chooser = gtk::EmojiChooser::new();
            {
                let (c, redraw) = (chosen.clone(), redraw.clone());
                chooser.connect_emoji_picked(move |_, e| {
                    let acc = c.borrow().accent.unwrap_or(ACCENTS[0]);
                    // The accent, softly, as the picture's field.
                    let soft = |s: u32| ((((acc >> s) & 0xFF) + 3 * 255) / 4) << s;
                    let field = 0xFF00_0000 | soft(16) | soft(8) | soft(0);
                    c.borrow_mut().picture = Some(PictureInfo::Emoji {
                        emoji: e.to_owned(),
                        color: field,
                    });
                    redraw();
                });
            }
            emoji.set_popover(Some(&chooser));
            pics.append(&emoji);
            let photo = gtk::Button::with_label("Photo");
            photo.add_css_class("pill");
            {
                let (c, redraw, d) = (chosen.clone(), redraw.clone(), dialog.clone());
                photo.connect_clicked(move |_| {
                    let (c, redraw) = (c.clone(), redraw.clone());
                    choose_photo(&d, true, move |bytes| {
                        c.borrow_mut().picture = Some(PictureInfo::Photo { data: bytes });
                        redraw();
                    });
                });
            }
            pics.append(&photo);
            let none = gtk::Button::with_label("None");
            none.add_css_class("pill");
            {
                let (c, redraw) = (chosen.clone(), redraw.clone());
                none.connect_clicked(move |_| {
                    c.borrow_mut().picture = None;
                    redraw();
                });
            }
            pics.append(&none);
            body.append(&pics);
        }
    } else {
        let l = gtk::Label::new(Some(
            "Only the group's owners and admins can change its theme.",
        ));
        l.add_css_class("dim-label");
        l.set_wrap(true);
        l.set_xalign(0.0);
        body.append(&l);
    }

    // One's own look, here only.
    let own_row = adw::SwitchRow::builder()
        .title("Keep my own look here")
        .subtitle("The plain background on your devices, whatever the theme. No one else is told.")
        .active(own)
        .build();
    own_row.connect_active_notify(move |r| set_own(r.is_active()));
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.append(&own_row);
    body.append(&list);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .build();
    view.set_content(Some(&scroll));
    if can_set {
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::End);
        buttons.set_margin_top(8);
        buttons.set_margin_bottom(12);
        buttons.set_margin_end(18);
        let plain = gtk::Button::with_label("Plain");
        plain.add_css_class("flat");
        let ok = gtk::Button::with_label("Set theme");
        ok.add_css_class("suggested-action");
        ok.add_css_class("pill");
        buttons.append(&plain);
        buttons.append(&ok);
        view.add_bottom_bar(&buttons);
        let set = Rc::new(set);
        {
            let (d, set) = (dialog.clone(), set.clone());
            plain.connect_clicked(move |_| {
                d.close();
                set(None);
            });
        }
        {
            let (d, c) = (dialog.clone(), chosen.clone());
            ok.connect_clicked(move |_| {
                d.close();
                set(Some(c.borrow().clone()));
            });
        }
    }
    dialog.set_child(Some(&view));
    dialog.present(Some(parent));
}

/// Picks a photo and shrinks it to fit (a group picture `picture`, else
/// a background), as a JPEG under the limit.
fn choose_photo(parent: &adw::Dialog, picture: bool, done: impl Fn(Vec<u8>) + 'static) {
    let filter = gtk::FileFilter::new();
    filter.add_pixbuf_formats();
    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let fd = gtk::FileDialog::builder()
        .title("Choose a photo")
        .filters(&filters)
        .build();
    let window = parent.root().and_downcast::<gtk::Window>();
    fd.open(window.as_ref(), gtk::gio::Cancellable::NONE, move |r| {
        let Ok(file) = r else { return };
        let Some(path) = file.path() else { return };
        let (side, max) = if picture {
            (512, threnody_ffi::max_picture_photo() as usize)
        } else {
            (1600, threnody_ffi::max_background_photo() as usize)
        };
        if let Some(bytes) = shrink(&path, side, max) {
            done(bytes);
        }
    });
}

/// `path`'s photo at most `side` pixels across, as a JPEG under `max`
/// bytes (stepping its quality down).
fn shrink(path: &std::path::Path, side: i32, max: usize) -> Option<Vec<u8>> {
    let pb = Pixbuf::from_file_at_scale(path, side, side, true).ok()?;
    let pb = pb.apply_embedded_orientation().unwrap_or(pb);
    let mut q = 85;
    while q > 20 {
        let bytes = pb
            .save_to_bufferv("jpeg", &[("quality", &q.to_string())])
            .ok()?;
        if bytes.len() <= max {
            return Some(bytes);
        }
        q -= 10;
    }
    None
}
