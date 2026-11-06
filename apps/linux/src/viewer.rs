//! Photos shown in the app rather than handed to another one (a sensitive
//! photo especially): bounded decoding off the main thread, thumbnails,
//! and the photo viewer with zoom, *Save a copy…* and *Edit*.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk::gdk_pixbuf::Pixbuf;
use gtk::{cairo, gdk, gio, glib};

use crate::ui;
use crate::window::App;

/// Larger images are refused rather than decoded (decompression bombs).
pub const MAX_PIXELS: i64 = 100_000_000;
/// Photos are edited and converted at most this size (Android: 4096).
pub const MAX_SIDE: i32 = 4096;
/// Zoomed in at most this many screen pixels to a decoded one.
const MAX_ZOOM: f64 = 8.0;

/// A decoded image, safe to hand between threads.
pub struct Decoded {
    pub width: i32,
    pub height: i32,
    pub stride: usize,
    pub has_alpha: bool,
    pub bytes: glib::Bytes,
}

/// Width and height of the picture in `path`, refusing what can't be
/// read and anything over [`MAX_PIXELS`]: only its header is read.
pub fn checked_size(path: &Path) -> Result<(i32, i32), String> {
    let (_, w, h) = Pixbuf::file_info(path).ok_or("not a picture this computer can read")?;
    if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > MAX_PIXELS {
        return Err("too large a picture".into());
    }
    Ok((w, h))
}

/// Decodes `path` no larger than `max_side` on its long side, upright
/// (embedded orientation applied); refuses more than [`MAX_PIXELS`].
/// Blocking: run it on a worker thread (`ui::bg`).
pub fn load_bounded(path: &Path, max_side: i32) -> Result<Decoded, String> {
    let (w, h) = checked_size(path)?;
    let max_side = max_side.max(1);
    let pixbuf = Pixbuf::from_file_at_scale(path, max_side.min(w), max_side.min(h), true)
        .map_err(|e| e.message().to_owned())?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Ok(Decoded {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: usize::try_from(pixbuf.rowstride()).unwrap_or(0),
        has_alpha: pixbuf.has_alpha(),
        bytes: pixbuf.read_pixel_bytes(),
    })
}

impl Decoded {
    pub fn texture(&self) -> gdk::Texture {
        gdk::MemoryTexture::new(
            self.width,
            self.height,
            if self.has_alpha {
                gdk::MemoryFormat::R8g8b8a8
            } else {
                gdk::MemoryFormat::R8g8b8
            },
            &self.bytes,
            self.stride,
        )
        .upcast()
    }

    /// As a cairo surface, to draw on.
    pub fn surface(&self) -> Result<cairo::ImageSurface, String> {
        let (w, h) = (
            usize::try_from(self.width).unwrap_or(0),
            usize::try_from(self.height).unwrap_or(0),
        );
        let (data, stride) = to_cairo_argb(&self.bytes, w, h, self.stride, self.has_alpha);
        cairo::ImageSurface::create_for_data(
            data,
            cairo::Format::ARgb32,
            self.width,
            self.height,
            i32::try_from(stride).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
    }
}

/// RGB(A) rows → cairo ARGB32: premultiplied alpha, one native-endian
/// `u32` a pixel. Returns the data and its stride.
pub fn to_cairo_argb(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    alpha: bool,
) -> (Vec<u8>, usize) {
    let channels = if alpha { 4 } else { 3 };
    let out_stride = w * 4;
    let mut out = vec![0u8; out_stride * h];
    let premultiply = |c: u8, a: u8| ((u32::from(c) * u32::from(a) + 127) / 255) as u8;
    for y in 0..h {
        let Some(row) = src.get(y * stride..y * stride + w * channels) else {
            break;
        };
        for (x, px) in row.chunks_exact(channels).enumerate() {
            let a = if alpha { px[3] } else { 255 };
            let (r, g, b) = (
                premultiply(px[0], a),
                premultiply(px[1], a),
                premultiply(px[2], a),
            );
            let v =
                (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
            out[y * out_stride + x * 4..][..4].copy_from_slice(&v.to_ne_bytes());
        }
    }
    (out, out_stride)
}

/// Thumbnails decoded so far, by file, for as long as their view lives.
pub type Thumbs = Rc<RefCell<HashMap<PathBuf, gdk::Texture>>>;

/// Fills `pic` with a picture of `path` no more than `side` pixels on its
/// long side, decoded off the main thread. `bound` belongs to the cell
/// showing `pic` and holds the path it shows now: a list recycles its
/// cells while scrolling, so a late result is dropped unless the cell
/// still shows that photo.
pub fn thumbnail(
    pic: &gtk::Picture,
    bound: Rc<RefCell<Option<PathBuf>>>,
    path: PathBuf,
    side: i32,
    cache: Thumbs,
) {
    *bound.borrow_mut() = Some(path.clone());
    if let Some(t) = cache.borrow().get(&path) {
        pic.set_paintable(Some(t));
        return;
    }
    pic.set_paintable(gdk::Paintable::NONE);
    let weak = pic.downgrade();
    let p = path.clone();
    ui::bg(
        move || load_bounded(&p, side),
        move |r| {
            let Ok(d) = r else { return };
            let t = d.texture();
            cache.borrow_mut().insert(path.clone(), t.clone());
            if bound.borrow().as_ref() == Some(&path)
                && let Some(pic) = weak.upgrade()
            {
                pic.set_paintable(Some(&t));
            }
        },
    );
}

/// What the viewer shows. `sensitive`: the message was marked sensitive,
/// so an edited copy starts sensitive too.
#[derive(Clone)]
pub struct Photo {
    pub path: PathBuf,
    pub name: String,
    pub caption: String,
    pub sensitive: bool,
}

/// Sends an edited copy into the chat the photo was opened from (through
/// the send dialog): the draft, and whether *Sensitive* starts checked.
pub type SendEdited = Rc<dyn Fn(PathBuf, bool)>;

pub fn is_gif(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// The longest side to decode a photo at for this window: twice the
/// monitor's, as Android does with the screen, up to [`MAX_SIDE`].
fn screen_side(window: &impl IsA<gtk::Native>) -> i32 {
    let monitor = window
        .surface()
        .and_then(|s| s.display().monitor_at_surface(&s));
    let side = monitor.map_or(1920, |m| {
        let g = m.geometry();
        g.width().max(g.height()) * m.scale_factor()
    });
    (2 * side).clamp(1024, MAX_SIDE)
}

/// Zoom and pan state of the viewer.
struct Zoom {
    scroll: gtk::ScrolledWindow,
    pic: gtk::Picture,
    /// Display pixels per image pixel; 0 = fitted to the view.
    scale: Cell<f64>,
    /// Where the pointer last was, in view coordinates.
    pointer: Cell<(f64, f64)>,
}

impl Zoom {
    fn image(&self) -> Option<(f64, f64)> {
        let p = self.pic.paintable()?;
        let (w, h) = (
            f64::from(p.intrinsic_width()),
            f64::from(p.intrinsic_height()),
        );
        (w > 0.0 && h > 0.0).then_some((w, h))
    }

    fn view(&self) -> (f64, f64) {
        (
            f64::from(self.scroll.width().max(1)),
            f64::from(self.scroll.height().max(1)),
        )
    }

    fn fitted(&self) -> f64 {
        let Some((iw, ih)) = self.image() else {
            return 1.0;
        };
        let (vw, vh) = self.view();
        (vw / iw).min(vh / ih)
    }

    /// The scale on screen now.
    fn current(&self) -> f64 {
        match self.scale.get() {
            0.0 => self.fitted(),
            s => s,
        }
    }

    /// Zooms to `scale`, keeping the image point under view point `at`
    /// where it is.
    fn zoom_to(self: &Rc<Self>, scale: f64, at: (f64, f64)) {
        let Some((iw, ih)) = self.image() else { return };
        let (vw, vh) = self.view();
        let old = self.current();
        // Where the image starts in the view, on each axis.
        let origin = |len: f64, view: f64, adj: &gtk::Adjustment| {
            if len < view {
                (view - len) / 2.0
            } else {
                -adj.value()
            }
        };
        let (ox, oy) = (
            origin(iw * old, vw, &self.scroll.hadjustment()),
            origin(ih * old, vh, &self.scroll.vadjustment()),
        );
        let point = ((at.0 - ox) / old, (at.1 - oy) / old);
        let fitted = self.fitted();
        let scale = scale.min(MAX_ZOOM.max(fitted));
        if scale <= fitted * 1.001 {
            self.fit();
            return;
        }
        self.scale.set(scale);
        self.scroll
            .set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        self.pic.set_content_fit(gtk::ContentFit::Fill);
        self.pic.set_halign(gtk::Align::Center);
        self.pic.set_valign(gtk::Align::Center);
        #[allow(clippy::cast_possible_truncation)]
        self.pic
            .set_size_request((iw * scale).round() as i32, (ih * scale).round() as i32);
        // Scroll once the new size is laid out.
        let (want_x, want_y) = (point.0 * scale - at.0, point.1 * scale - at.1);
        let this = self.clone();
        let frames = Cell::new(0);
        self.pic.add_tick_callback(move |_, _| {
            frames.set(frames.get() + 1);
            let (h, v) = (this.scroll.hadjustment(), this.scroll.vadjustment());
            let laid_out = (h.upper() - iw * scale).abs() < 2.0 || iw * scale <= vw;
            if !laid_out && frames.get() < 10 {
                return glib::ControlFlow::Continue;
            }
            h.set_value(want_x.clamp(0.0, (h.upper() - h.page_size()).max(0.0)));
            v.set_value(want_y.clamp(0.0, (v.upper() - v.page_size()).max(0.0)));
            glib::ControlFlow::Break
        });
    }

    fn fit(&self) {
        self.scale.set(0.0);
        self.pic.set_size_request(-1, -1);
        self.pic.set_content_fit(gtk::ContentFit::Contain);
        self.pic.set_halign(gtk::Align::Fill);
        self.pic.set_valign(gtk::Align::Fill);
        self.scroll
            .set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
    }

    fn by(self: &Rc<Self>, factor: f64, at: (f64, f64)) {
        self.zoom_to(self.current() * factor, at);
    }

    fn centre(&self) -> (f64, f64) {
        let (w, h) = self.view();
        (w / 2.0, h / 2.0)
    }

    /// Double-click, Ctrl+scroll, pinch, drag to pan.
    fn controls(self: &Rc<Self>) {
        let motion = gtk::EventControllerMotion::new();
        let this = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(this) = this.upgrade() {
                this.pointer.set((x, y));
            }
        });
        self.scroll.add_controller(motion);

        let click = gtk::GestureClick::new();
        let this = Rc::downgrade(self);
        click.connect_pressed(move |_, n, x, y| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if n == 2 {
                if this.scale.get() == 0.0 {
                    this.zoom_to(this.fitted() * 2.5, (x, y));
                } else {
                    this.fit();
                }
            }
        });
        self.scroll.add_controller(click);

        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let this = Rc::downgrade(self);
        wheel.connect_scroll(move |c, _, dy| {
            let Some(this) = this.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if !c
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
            {
                return glib::Propagation::Proceed;
            }
            this.by(if dy < 0.0 { 1.1 } else { 1.0 / 1.1 }, this.pointer.get());
            glib::Propagation::Stop
        });
        self.scroll.add_controller(wheel);

        let pinch = gtk::GestureZoom::new();
        let start = Rc::new(Cell::new(1.0));
        let (this, s) = (Rc::downgrade(self), start.clone());
        pinch.connect_begin(move |_, _| {
            if let Some(this) = this.upgrade() {
                s.set(this.current());
            }
        });
        let this = Rc::downgrade(self);
        pinch.connect_scale_changed(move |g, scale| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let at = g.bounding_box_center().unwrap_or_else(|| this.centre());
            this.zoom_to(start.get() * scale, at);
        });
        self.scroll.add_controller(pinch);

        let drag = gtk::GestureDrag::new();
        let from = Rc::new(Cell::new((0.0, 0.0)));
        let (this, f) = (Rc::downgrade(self), from.clone());
        drag.connect_drag_begin(move |_, _, _| {
            let Some(this) = this.upgrade() else { return };
            f.set((
                this.scroll.hadjustment().value(),
                this.scroll.vadjustment().value(),
            ));
        });
        let this = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if this.scale.get() == 0.0 {
                return;
            }
            let (x, y) = from.get();
            this.scroll.hadjustment().set_value(x - dx);
            this.scroll.vadjustment().set_value(y - dy);
        });
        self.scroll.add_controller(drag);
    }
}

/// Shows `photo` in the app. `send_edited`, given when opened from a
/// chat, sends an edited copy there.
pub fn show(app: &Rc<App>, persona: Option<String>, photo: Photo, send_edited: Option<SendEdited>) {
    let gif = is_gif(&photo.name);
    let d = adw::Dialog::builder()
        .title(&photo.name)
        .content_width(900)
        .content_height(700)
        .build();
    let header = adw::HeaderBar::new();
    let toasts = adw::ToastOverlay::new();
    let pic = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .hexpand(true)
        .vexpand(true)
        .build();
    pic.add_css_class("viewer-photo");
    let scroll = gtk::ScrolledWindow::builder()
        .child(&pic)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let zoom = Rc::new(Zoom {
        scroll: scroll.clone(),
        pic: pic.clone(),
        scale: Cell::new(0.0),
        pointer: Cell::new((0.0, 0.0)),
    });
    zoom.controls();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&scroll);
    if !photo.caption.is_empty() {
        let caption = ui::label(&photo.caption, &["viewer-caption"]);
        caption.set_selectable(true);
        caption.set_xalign(0.5);
        caption.set_justify(gtk::Justification::Center);
        column.append(&caption);
    }
    toasts.set_child(Some(&column));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));
    d.set_child(Some(&view));

    // Header: Edit (not GIFs) and a menu.
    if !gif {
        let edit = gtk::Button::from_icon_name("document-edit-symbolic");
        edit.set_tooltip_text(Some("Edit: draw or add text"));
        let (weak, dd, p, persona, send) = (
            Rc::downgrade(app),
            d.clone(),
            photo.clone(),
            persona.clone(),
            send_edited.clone(),
        );
        edit.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                edit_photo(&app, &dd, persona.clone(), &p, send.clone());
            }
        });
        header.pack_start(&edit);
    }
    let menu = gio::Menu::new();
    menu.append(Some("Save a copy…"), Some("viewer.save"));
    menu.append(Some("Open in another app"), Some("viewer.open"));
    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More")
        .menu_model(&menu)
        .build();
    header.pack_end(&more);
    let actions = gio::SimpleActionGroup::new();
    let save = gio::SimpleAction::new("save", None);
    let (weak, p, t) = (Rc::downgrade(app), photo.path.clone(), toasts.clone());
    let name = photo.name.clone();
    save.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            let t = t.clone();
            save_copy(&app, &p, &name, move |msg| {
                t.add_toast(adw::Toast::new(&msg));
            });
        }
    });
    actions.add_action(&save);
    let open = gio::SimpleAction::new("open", None);
    let (weak, p, t) = (Rc::downgrade(app), photo.path.clone(), toasts.clone());
    open.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            let t = t.clone();
            ui::open_externally(&app.window, &p, move || {
                t.add_toast(adw::Toast::new("No application can open this file"));
            });
        }
    });
    actions.add_action(&open);
    d.insert_action_group("viewer", Some(&actions));

    // +, - and 0 zoom in, out and back. This keeps the zoom state for
    // as long as the dialog lives (the controls hold it weakly).
    let keys = gtk::EventControllerKey::new();
    let z = zoom.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        use gdk::Key;
        match key {
            Key::plus | Key::KP_Add | Key::equal => z.by(1.25, z.centre()),
            Key::minus | Key::KP_Subtract => z.by(0.8, z.centre()),
            Key::_0 | Key::KP_0 => z.fit(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    d.add_controller(keys);

    d.present(Some(&app.window));
    let failed = {
        let (weak, dd) = (Rc::downgrade(app), d.clone());
        move || {
            if let Some(app) = weak.upgrade() {
                app.toast("Couldn't show this image");
            }
            dd.close();
        }
    };
    if gif {
        // GTK decodes it here, at full size: only once its size is known
        // to be reasonable.
        if checked_size(&photo.path).is_err() {
            failed();
            return;
        }
        let file = gtk::Picture::for_filename(&photo.path);
        pic.set_paintable(file.paintable().as_ref());
        ui::animate(&pic, &photo.path);
        return;
    }
    let side = screen_side(&app.window);
    let path = photo.path.clone();
    ui::bg(
        move || load_bounded(&path, side),
        move |r| match r {
            Ok(decoded) => pic.set_paintable(Some(&decoded.texture())),
            Err(_) => failed(),
        },
    );
}

/// Saves a copy of `path` where the user picks (Downloads to start with);
/// `said` gets what to tell them.
pub fn save_copy(app: &Rc<App>, path: &Path, name: &str, said: impl Fn(String) + 'static) {
    save_copy_then(app, path, name, move |r| {
        if let Some(msg) = r {
            said(msg);
        }
    });
}

/// As [`save_copy`]; `done` gets None when the user cancelled.
fn save_copy_then(
    app: &Rc<App>,
    path: &Path,
    name: &str,
    done: impl FnOnce(Option<String>) + 'static,
) {
    let mut b = gtk::FileDialog::builder()
        .title("Save a copy")
        .initial_name(name)
        .modal(true);
    if let Some(dl) = glib::user_special_dir(glib::UserDirectory::Downloads) {
        b = b.initial_folder(&gio::File::for_path(dl));
    }
    let from = path.to_owned();
    b.build()
        .save(Some(&app.window), gio::Cancellable::NONE, move |r| {
            let Some(to) = r.ok().and_then(|f| f.path()) else {
                done(None);
                return;
            };
            ui::bg(
                move || std::fs::copy(&from, &to).map(|_| ()),
                move |r| {
                    done(Some(match r {
                        Ok(()) => "Saved".to_owned(),
                        Err(e) => format!("Couldn't save: {e}"),
                    }));
                },
            );
        });
}

/// *Edit* in the viewer: the editor, then what to do with the result.
fn edit_photo(
    app: &Rc<App>,
    viewer: &adw::Dialog,
    persona: Option<String>,
    photo: &Photo,
    send_edited: Option<SendEdited>,
) {
    let (weak, viewer, sensitive) = (Rc::downgrade(app), viewer.clone(), photo.sensitive);
    let name = photo.name.clone();
    crate::editor::open(app, persona, photo.path.clone(), name, move |draft| {
        let (Some(app), Some(draft)) = (weak.upgrade(), draft) else {
            return;
        };
        edited(&app, &viewer, draft, sensitive, send_edited.clone());
    });
}

/// "Edited photo": send it in the chat, save a copy, or discard it.
/// Closing the question any other way discards it.
fn edited(
    app: &Rc<App>,
    viewer: &adw::Dialog,
    draft: PathBuf,
    sensitive: bool,
    send_edited: Option<SendEdited>,
) {
    let mut responses = vec![("discard", "Discard"), ("save", "Save a copy…")];
    if send_edited.is_some() {
        responses.push(("send", "Send in this chat"));
    }
    let d = ui::alert(
        "Edited photo",
        "Send it, or keep a copy. Otherwise it's thrown away.",
        &responses,
    );
    d.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
    if send_edited.is_some() {
        d.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    }
    let (weak, viewer) = (Rc::downgrade(app), viewer.clone());
    d.connect_response(None, move |_, r| {
        let Some(app) = weak.upgrade() else {
            let _ = std::fs::remove_file(&draft);
            return;
        };
        match (r, &send_edited) {
            ("send", Some(send)) => {
                viewer.close();
                send(draft.clone(), sensitive);
            }
            ("save", _) => {
                let name = draft
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "edited.jpg".into());
                let (weak, v, d2, send) = (
                    Rc::downgrade(&app),
                    viewer.clone(),
                    draft.clone(),
                    send_edited.clone(),
                );
                save_copy_then(&app, &draft, &name, move |r| {
                    let Some(app) = weak.upgrade() else { return };
                    match r {
                        // Cancelled: ask again.
                        None => edited(&app, &v, d2, sensitive, send),
                        Some(msg) => {
                            let _ = std::fs::remove_file(&d2);
                            app.toast(&msg);
                        }
                    }
                });
            }
            _ => {
                let _ = std::fs::remove_file(&draft);
            }
        }
    });
    d.present(Some(&app.window));
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn temp_dir(what: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = &N;
        let dir = std::env::temp_dir().join(format!(
            "threnody-{what}-{}-{}",
            std::process::id(),
            n.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A PNG whose header claims `w`×`h` pixels, cut short after the
    /// header: a decompression bomb, without the bomb.
    pub fn fake_png(w: u32, h: u32) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut c = 0xffff_ffff_u32;
            for &b in data {
                c ^= u32::from(b);
                for _ in 0..8 {
                    c = if c & 1 == 1 {
                        0xedb8_8320 ^ (c >> 1)
                    } else {
                        c >> 1
                    };
                }
            }
            !c
        }
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // An IDAT that ends early.
        png.extend_from_slice(&100u32.to_be_bytes());
        png.extend_from_slice(b"IDAT\x78\x9c");
        png
    }

    #[test]
    fn bounded_loading_scales_and_keeps_aspect() {
        let dir = temp_dir("viewer");
        let path = dir.join("a.png");
        let p = Pixbuf::new(gdk::gdk_pixbuf::Colorspace::Rgb, false, 8, 400, 300).unwrap();
        p.fill(0x2040_60ff);
        p.savev(&path, "png", &[]).unwrap();
        let d = load_bounded(&path, 200).unwrap();
        assert_eq!((d.width, d.height), (200, 150));
        let channels = if d.has_alpha { 4 } else { 3 };
        assert!(d.stride >= 200 * channels);
        assert!(d.bytes.len() >= d.stride * 149 + 200 * channels);
        // Smaller than the bound: as it is.
        let d = load_bounded(&path, 1000).unwrap();
        assert_eq!((d.width, d.height), (400, 300));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decompression_bombs_are_refused() {
        let dir = temp_dir("bomb");
        let path = dir.join("bomb.png");
        std::fs::write(&path, fake_png(20_000, 20_000)).unwrap();
        assert!(load_bounded(&path, 200).is_err());
        assert!(checked_size(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unreadable_files_are_refused() {
        let dir = temp_dir("unreadable");
        let path = dir.join("x.jpg");
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        std::fs::write(&path, noise).unwrap();
        assert!(load_bounded(&path, 200).is_err());
        assert!(load_bounded(&dir.join("missing.png"), 200).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rgba_becomes_premultiplied_cairo() {
        // One row of two RGBA pixels, padded to 12 bytes; then a second row.
        let src = [
            200, 100, 50, 255, /**/ 200, 100, 50, 128, /**/ 9, 9, 9, 9, //
            10, 20, 30, 0, /**/ 255, 255, 255, 255, /**/ 0, 0, 0, 0,
        ];
        let (out, stride) = to_cairo_argb(&src, 2, 2, 12, true);
        assert_eq!(stride, 8);
        let px = |i: usize| u32::from_ne_bytes(out[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(px(0), 0xffc8_6432);
        // Half alpha: about half of each channel.
        assert_eq!(px(1), 0x8064_3219);
        // No alpha at all: nothing.
        assert_eq!(px(2), 0);
        assert_eq!(px(3), 0xffff_ffff);
        // RGB, no alpha: opaque.
        let (out, _) = to_cairo_argb(&[1, 2, 3], 1, 1, 3, false);
        assert_eq!(
            u32::from_ne_bytes(out[..4].try_into().unwrap()),
            0xff01_0203
        );
    }
}
