//! The photo editor: draw on a photo or put text on it, as the Android
//! app's editor does (no crop, no blur). The result is a fresh JPEG with
//! no metadata, kept privately as a draft until it is sent or saved.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, gdk, glib, pango};

use crate::sketch::{COLOURS, Colour, Label, Mark, PEN_PX, Sketch};
use crate::ui;
use crate::viewer;
use crate::window::App;

/// Text starts this many screen pixels high (Android: 30 dp), and is
/// kept between these sizes on screen.
const TEXT_PX: f64 = 30.0;
const TEXT_MIN_PX: f64 = 12.0;
const TEXT_MAX_PX: f64 = 160.0;
/// A press on text that moves less than this is a click, not a drag.
const CLICK_SLOP: f64 = 6.0;

/// Photo ↔ view mapping for a photo fitted (centred) in a view.
#[derive(Clone, Copy, Debug)]
pub struct Fit {
    pub scale: f64,
    pub dx: f64,
    pub dy: f64,
}

impl Fit {
    pub fn new(photo: (f64, f64), view: (f64, f64)) -> Self {
        let (pw, ph) = (photo.0.max(1.0), photo.1.max(1.0));
        let scale = (view.0 / pw).min(view.1 / ph).max(f64::MIN_POSITIVE);
        Self {
            scale,
            dx: (view.0 - pw * scale) / 2.0,
            dy: (view.1 - ph * scale) / 2.0,
        }
    }

    pub fn to_photo(self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.dx) / self.scale, (y - self.dy) / self.scale)
    }

    pub fn to_view(self, x: f64, y: f64) -> (f64, f64) {
        (x * self.scale + self.dx, y * self.scale + self.dy)
    }
}

fn rgb(c: Colour) -> (f64, f64, f64) {
    (
        f64::from((c >> 16) & 0xff) / 255.0,
        f64::from((c >> 8) & 0xff) / 255.0,
        f64::from(c & 0xff) / 255.0,
    )
}

/// The edge drawn around text: dark around light text, light around dark.
fn edge(c: Colour) -> Colour {
    let (r, g, b) = rgb(c);
    if 0.299 * r + 0.587 * g + 0.114 * b > 0.55 {
        0x00_0000
    } else {
        0xFF_FFFF
    }
}

/// Sets `layout` up for label `l` and returns its size in pixels.
fn lay_out(layout: &pango::Layout, l: &Label) -> (f64, f64) {
    let mut font = pango::FontDescription::from_string("Sans Bold");
    font.set_absolute_size(l.size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_alignment(pango::Alignment::Center);
    layout.set_text(&l.text);
    let (w, h) = layout.pixel_size();
    (f64::from(w), f64::from(h))
}

/// Draws every mark on `cr` (already in photo coordinates).
fn draw_marks(cr: &cairo::Context, sketch: &Sketch) {
    let layout = pangocairo::functions::create_layout(cr);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    for m in sketch.marks() {
        match m {
            Mark::Stroke(s) => {
                let Some((first, rest)) = s.points.split_first() else {
                    continue;
                };
                let (r, g, b) = rgb(s.colour);
                cr.set_source_rgb(r, g, b);
                cr.set_line_width(s.width);
                cr.move_to(first.0, first.1);
                for p in rest {
                    cr.line_to(p.0, p.1);
                }
                let _ = cr.stroke();
            }
            Mark::Label(l) => {
                let (w, h) = lay_out(&layout, l);
                let (x, y) = (l.x - w / 2.0, l.y - h / 2.0);
                cr.move_to(x, y);
                pangocairo::functions::layout_path(cr, &layout);
                let (r, g, b) = rgb(edge(l.colour));
                cr.set_source_rgb(r, g, b);
                cr.set_line_width(l.size * 0.14);
                let _ = cr.stroke();
                let (r, g, b) = rgb(l.colour);
                cr.set_source_rgb(r, g, b);
                cr.move_to(x, y);
                pangocairo::functions::show_layout(cr, &layout);
            }
        }
    }
}

/// The photo with its marks, at the photo's size, as RGB rows.
fn render(photo: &cairo::ImageSurface, sketch: &Sketch) -> Result<(Vec<u8>, i32, i32), String> {
    let (w, h) = (photo.width(), photo.height());
    let mut out =
        cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).map_err(|e| e.to_string())?;
    {
        let cr = cairo::Context::new(&out).map_err(|e| e.to_string())?;
        cr.set_source_surface(photo, 0.0, 0.0)
            .map_err(|e| e.to_string())?;
        cr.paint().map_err(|e| e.to_string())?;
        draw_marks(&cr, sketch);
    }
    out.flush();
    let stride = usize::try_from(out.stride()).map_err(|e| e.to_string())?;
    let data = out.data().map_err(|e| e.to_string())?;
    let (uw, uh) = (
        usize::try_from(w).map_err(|e| e.to_string())?,
        usize::try_from(h).map_err(|e| e.to_string())?,
    );
    Ok((argb_to_rgb(&data, uw, uh, stride), w, h))
}

/// cairo ARGB32 (premultiplied, native-endian) → packed RGB.
pub fn argb_to_rgb(data: &[u8], w: usize, h: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            let i = y * stride + x * 4;
            let Some(px) = data.get(i..i + 4) else {
                out.extend_from_slice(&[0, 0, 0]);
                continue;
            };
            let v = u32::from_ne_bytes([px[0], px[1], px[2], px[3]]);
            let a = v >> 24;
            let un = |c: u32| -> u8 {
                match a {
                    0 => 0,
                    255 => c as u8,
                    a => ((c * 255 + a / 2) / a).min(255) as u8,
                }
            };
            out.extend_from_slice(&[un((v >> 16) & 0xff), un((v >> 8) & 0xff), un(v & 0xff)]);
        }
    }
    out
}

/// Encodes RGB rows as a JPEG (quality 92, Android's) into `path`, which
/// [`crate::core::Core::edit_draft_path`] made. gdk-pixbuf writes no Exif
/// unless asked, so the photo carries no metadata at all.
pub fn write_jpeg(path: &Path, rgb: Vec<u8>, w: i32, h: i32) -> Result<(), String> {
    use std::io::Write;
    let pixbuf = gdk::gdk_pixbuf::Pixbuf::from_bytes(
        &glib::Bytes::from_owned(rgb),
        gdk::gdk_pixbuf::Colorspace::Rgb,
        false,
        8,
        w,
        h,
        w * 3,
    );
    let jpeg = pixbuf
        .save_to_bufferv("jpeg", &[("quality", "92")])
        .map_err(|e| e.message().to_owned())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    f.write_all(&jpeg).map_err(|e| e.to_string())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Draw,
    Text,
}

struct Editor {
    app: std::rc::Weak<App>,
    persona: Option<String>,
    name: String,
    dialog: adw::Dialog,
    toasts: adw::ToastOverlay,
    area: gtk::DrawingArea,
    undo: gtk::Button,
    hint: gtk::Label,
    pens: gtk::Box,
    swatches: Vec<gtk::Button>,
    photo: RefCell<Option<cairo::ImageSurface>>,
    sketch: RefCell<Sketch>,
    tool: Cell<Tool>,
    pen: Cell<usize>,
    colour: Cell<Colour>,
    /// The label a drag started on, and whether it moved yet.
    dragging: Cell<Option<usize>>,
    moved: Cell<bool>,
    /// Done: the result went to `done`; closing doesn't cancel.
    finished: Cell<bool>,
    done: Box<dyn Fn(Option<PathBuf>)>,
}

impl Editor {
    fn fit(&self) -> Option<Fit> {
        let photo = self.photo.borrow();
        let p = photo.as_ref()?;
        Some(Fit::new(
            (f64::from(p.width()), f64::from(p.height())),
            (f64::from(self.area.width()), f64::from(self.area.height())),
        ))
    }

    /// Something changed: redraw, and ask before closing if there's
    /// anything to lose.
    fn changed(&self) {
        let dirty = self.sketch.borrow().can_undo();
        self.undo.set_sensitive(dirty);
        self.dialog.set_can_close(!dirty);
        self.area.queue_draw();
    }

    fn set_tool(&self, tool: Tool) {
        self.tool.set(tool);
        self.pens.set_visible(tool == Tool::Draw);
        if tool == Tool::Draw {
            self.sketch.borrow_mut().selected = None;
        }
        self.hint.set_text(match tool {
            Tool::Draw => "Draw on the photo with the mouse or a finger",
            Tool::Text => {
                "Click the photo to add text. Drag text to move it, scroll or pinch to resize it, \
                 click it to change it."
            }
        });
        self.area.queue_draw();
    }

    fn set_colour(&self, i: usize) {
        self.colour.set(COLOURS[i]);
        for (j, s) in self.swatches.iter().enumerate() {
            if j == i {
                s.add_css_class("selected");
            } else {
                s.remove_css_class("selected");
            }
        }
        if self.tool.get() == Tool::Text {
            self.sketch.borrow_mut().recolour_selected(COLOURS[i]);
            self.changed();
        }
    }

    /// Width and height of a label, in photo pixels.
    fn extent(&self) -> impl Fn(&Label) -> (f64, f64) + use<> {
        let layout = self.area.create_pango_layout(None);
        move |l| lay_out(&layout, l)
    }

    fn text_sizes(fit: Fit) -> (f64, f64) {
        (TEXT_MIN_PX / fit.scale, TEXT_MAX_PX / fit.scale)
    }

    /// Resizes the selected label by `factor` (one undo step).
    fn resize_selected(&self, factor: f64) -> bool {
        let (Some(fit), Some(i)) = (self.fit(), self.sketch.borrow().selected) else {
            return false;
        };
        let (min, max) = Self::text_sizes(fit);
        {
            let mut s = self.sketch.borrow_mut();
            s.begin_place(i);
            s.place(i, 0.0, 0.0, factor, min, max);
            s.end_place(i);
        }
        self.changed();
        true
    }

    /// Asks for text to add at a point (`at`, photo pixels) or a change to
    /// label `i`.
    fn ask_text(self: &Rc<Self>, at: Option<(f64, f64)>, i: Option<usize>) {
        let existing = i.and_then(|i| self.sketch.borrow().label(i).map(|l| l.text.clone()));
        let mut responses = vec![("cancel", "Cancel")];
        if existing.is_some() {
            responses.push(("remove", "Remove"));
        }
        responses.push(("ok", "Done"));
        let d = ui::alert(
            if existing.is_some() {
                "Change text"
            } else {
                "Add text"
            },
            "",
            &responses,
        );
        d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
        if existing.is_some() {
            d.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        }
        let text = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(6)
            .right_margin(6)
            .build();
        text.buffer().set_text(existing.as_deref().unwrap_or(""));
        text.add_css_class("card");
        let scroll = gtk::ScrolledWindow::builder()
            .child(&text)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .min_content_height(36)
            .max_content_height(6 * 24)
            .build();
        d.set_extra_child(Some(&scroll));
        let this = self.clone();
        d.connect_response(None, move |_, r| {
            let buf = text.buffer();
            let typed = buf
                .text(&buf.start_iter(), &buf.end_iter(), false)
                .trim()
                .to_owned();
            match (r, i, at) {
                ("remove", Some(i), _) => this.sketch.borrow_mut().set_text(i, ""),
                ("ok", Some(i), _) => this.sketch.borrow_mut().set_text(i, &typed),
                ("ok", None, Some((x, y))) => {
                    let size = this.fit().map_or(TEXT_PX, |f| TEXT_PX / f.scale);
                    let colour = this.colour.get();
                    this.sketch
                        .borrow_mut()
                        .add_label(&typed, colour, x, y, size);
                }
                _ => return,
            }
            this.changed();
        });
        d.present(Some(&self.dialog));
        let tv = scroll.child();
        glib::idle_add_local_once(move || {
            if let Some(tv) = tv {
                tv.grab_focus();
            }
        });
    }

    fn draw(&self, cr: &cairo::Context) {
        let (Some(fit), Some(photo)) = (self.fit(), self.photo.borrow().clone()) else {
            return;
        };
        cr.save().ok();
        cr.translate(fit.dx, fit.dy);
        cr.scale(fit.scale, fit.scale);
        if cr.set_source_surface(&photo, 0.0, 0.0).is_ok() {
            let _ = cr.paint();
        }
        let sketch = self.sketch.borrow();
        draw_marks(cr, &sketch);
        cr.restore().ok();
        // The selected text, while placing text.
        if self.tool.get() == Tool::Text
            && let Some(l) = sketch.selected.and_then(|i| sketch.label(i))
        {
            let (w, h) = self.extent()(l);
            let pad = 8.0;
            let (x, y) = fit.to_view(l.x - w / 2.0, l.y - h / 2.0);
            let (w, h) = (w * fit.scale + 2.0 * pad, h * fit.scale + 2.0 * pad);
            let (x, y) = (x - pad, y - pad);
            let r = 8.0_f64.min(w / 2.0).min(h / 2.0);
            cr.new_sub_path();
            cr.arc(x + w - r, y + r, r, -std::f64::consts::FRAC_PI_2, 0.0);
            cr.arc(x + w - r, y + h - r, r, 0.0, std::f64::consts::FRAC_PI_2);
            cr.arc(
                x + r,
                y + h - r,
                r,
                std::f64::consts::FRAC_PI_2,
                std::f64::consts::PI,
            );
            cr.arc(
                x + r,
                y + r,
                r,
                std::f64::consts::PI,
                1.5 * std::f64::consts::PI,
            );
            cr.close_path();
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.set_line_width(2.0);
            cr.set_dash(&[6.0, 4.0], 0.0);
            let _ = cr.stroke();
        }
    }

    /// *Done*: draws the marks into the photo and writes it as a draft.
    fn finish(self: &Rc<Self>) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(photo) = self.photo.borrow().clone() else {
            return;
        };
        let rendered = render(&photo, &self.sketch.borrow());
        let stem = self
            .name
            .rsplit_once('.')
            .map_or(self.name.as_str(), |(s, _)| s);
        let draft = rendered.and_then(|r| {
            let path = app
                .core
                .edit_draft_path(self.persona.as_deref(), &format!("{stem}-edited.jpg"))?;
            Ok((r, path))
        });
        let ((rgb, w, h), path) = match draft {
            Ok(d) => d,
            Err(_) => {
                self.toasts
                    .add_toast(adw::Toast::new("Couldn't save the edited photo"));
                return;
            }
        };
        self.dialog.set_sensitive(false);
        let this = self.clone();
        let p = path.clone();
        ui::bg(
            move || write_jpeg(&p, rgb, w, h),
            move |r| {
                this.dialog.set_sensitive(true);
                match r {
                    Ok(()) => {
                        this.finished.set(true);
                        this.dialog.force_close();
                        (this.done)(Some(path));
                    }
                    Err(_) => {
                        let _ = std::fs::remove_file(&path);
                        this.toasts
                            .add_toast(adw::Toast::new("Couldn't save the edited photo"));
                    }
                }
            },
        );
    }

    fn gestures(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let this = Rc::downgrade(self);
        drag.connect_drag_begin(move |_, x, y| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let Some(fit) = this.fit() else { return };
            let (px, py) = fit.to_photo(x, y);
            this.moved.set(false);
            match this.tool.get() {
                Tool::Draw => {
                    let width = PEN_PX[this.pen.get()] / fit.scale;
                    this.sketch
                        .borrow_mut()
                        .begin_stroke(px, py, this.colour.get(), width);
                    this.area.queue_draw();
                }
                Tool::Text => {
                    let hit =
                        this.sketch
                            .borrow()
                            .label_at(px, py, 12.0 / fit.scale, this.extent());
                    this.dragging.set(hit);
                    if let Some(i) = hit {
                        let mut s = this.sketch.borrow_mut();
                        s.selected = Some(i);
                        s.begin_place(i);
                    }
                    this.area.queue_draw();
                }
            }
        });
        let this = Rc::downgrade(self);
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let (Some(fit), Some((x, y))) = (this.fit(), g.start_point()) else {
                return;
            };
            match this.tool.get() {
                Tool::Draw => {
                    let (px, py) = fit.to_photo(x + dx, y + dy);
                    this.sketch.borrow_mut().extend_stroke(px, py);
                }
                Tool::Text => {
                    let Some(i) = this.dragging.get() else { return };
                    if !this.moved.get() && dx.hypot(dy) < CLICK_SLOP {
                        return;
                    }
                    this.moved.set(true);
                    let (min, max) = Self::text_sizes(fit);
                    this.sketch.borrow_mut().place(
                        i,
                        dx / fit.scale,
                        dy / fit.scale,
                        1.0,
                        min,
                        max,
                    );
                }
            }
            this.area.queue_draw();
        });
        let this = Rc::downgrade(self);
        drag.connect_drag_end(move |g, dx, dy| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let (Some(fit), Some((x, y))) = (this.fit(), g.start_point()) else {
                return;
            };
            let (px, py) = fit.to_photo(x + dx, y + dy);
            match this.tool.get() {
                Tool::Draw => this.sketch.borrow_mut().end_stroke(px, py),
                Tool::Text => match this.dragging.take() {
                    Some(i) => {
                        this.sketch.borrow_mut().end_place(i);
                        if !this.moved.get() {
                            this.ask_text(None, Some(i));
                        }
                    }
                    None => {
                        let (sx, sy) = fit.to_photo(x, y);
                        this.sketch.borrow_mut().selected = None;
                        this.ask_text(Some((sx, sy)), None);
                    }
                },
            }
            this.changed();
        });
        let this = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.sketch.borrow_mut().cancel_stroke();
            if let Some(i) = this.dragging.take() {
                this.sketch.borrow_mut().end_place(i);
            }
            this.changed();
        });
        self.area.add_controller(drag);

        // Scroll or pinch over the photo resizes the selected text.
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let this = Rc::downgrade(self);
        wheel.connect_scroll(move |_, _, dy| {
            let Some(this) = this.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if this.tool.get() != Tool::Text || dy == 0.0 {
                return glib::Propagation::Proceed;
            }
            if this.resize_selected(if dy < 0.0 { 1.1 } else { 0.9 }) {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        self.area.add_controller(wheel);
        let pinch = gtk::GestureZoom::new();
        let this = Rc::downgrade(self);
        pinch.connect_begin(move |_, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if let Some(i) = this.sketch.borrow().selected {
                this.sketch.borrow_mut().begin_place(i);
            }
        });
        let this = Rc::downgrade(self);
        pinch.connect_scale_changed(move |_, scale| {
            let Some(this) = this.upgrade() else {
                return;
            };
            let (Some(fit), Some(i)) = (this.fit(), this.sketch.borrow().selected) else {
                return;
            };
            let (min, max) = Self::text_sizes(fit);
            this.sketch.borrow_mut().place(i, 0.0, 0.0, scale, min, max);
            this.area.queue_draw();
        });
        let this = Rc::downgrade(self);
        pinch.connect_end(move |_, _| {
            let Some(this) = this.upgrade() else {
                return;
            };
            if let Some(i) = this.sketch.borrow().selected {
                this.sketch.borrow_mut().end_place(i);
            }
            this.changed();
        });
        self.area.add_controller(pinch);

        let keys = gtk::EventControllerKey::new();
        let this = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = this.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if key == gdk::Key::Delete
                && let Some(i) = this.sketch.borrow().selected
            {
                this.sketch.borrow_mut().set_text(i, "");
                this.changed();
                return glib::Propagation::Stop;
            }
            if state.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::z | gdk::Key::Z)
            {
                this.sketch.borrow_mut().undo();
                this.changed();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.dialog.add_controller(keys);
    }
}

/// Edits the photo at `source` (not a GIF); `done` gets the edited copy
/// (a draft in the identity's `edited/` directory) or None if cancelled.
pub fn open(
    app: &Rc<App>,
    persona: Option<String>,
    source: PathBuf,
    name: String,
    done: impl Fn(Option<PathBuf>) + 'static,
) {
    let dialog = adw::Dialog::builder()
        .title("Edit photo")
        .content_width(900)
        .content_height(700)
        .build();
    let header = adw::HeaderBar::new();
    let undo = gtk::Button::from_icon_name("edit-undo-symbolic");
    undo.set_tooltip_text(Some("Undo (Ctrl+Z)"));
    undo.set_sensitive(false);
    header.pack_start(&undo);
    let finish = gtk::Button::with_label("Done");
    finish.add_css_class("suggested-action");
    header.pack_end(&finish);

    let area = gtk::DrawingArea::builder()
        .hexpand(true)
        .vexpand(true)
        .focusable(true)
        .build();
    area.add_css_class("editor-canvas");

    let panel = gtk::Box::new(gtk::Orientation::Vertical, 8);
    panel.add_css_class("editor-panel");
    let tools = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let kinds = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    kinds.add_css_class("linked");
    let draw = gtk::ToggleButton::with_label("Draw");
    let text = gtk::ToggleButton::with_label("Text");
    text.set_group(Some(&draw));
    draw.set_active(true);
    kinds.append(&draw);
    kinds.append(&text);
    tools.append(&kinds);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    tools.append(&spacer);
    let pens = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    pens.add_css_class("linked");
    let mut pen_buttons: Vec<gtk::ToggleButton> = Vec::new();
    for (i, (dot, tip)) in [(7, "Thin pen"), (10, "Medium pen"), (16, "Thick pen")]
        .into_iter()
        .enumerate()
    {
        let d = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        d.set_size_request(dot, dot);
        d.set_halign(gtk::Align::Center);
        d.set_valign(gtk::Align::Center);
        d.add_css_class("pen-dot");
        let b = gtk::ToggleButton::builder()
            .child(&d)
            .tooltip_text(tip)
            .build();
        if let Some(first) = pen_buttons.first() {
            b.set_group(Some(first));
        }
        b.set_active(i == 1);
        pens.append(&b);
        pen_buttons.push(b);
    }
    tools.append(&pens);
    panel.append(&tools);
    let swatch_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    swatch_row.set_halign(gtk::Align::Center);
    let swatches: Vec<gtk::Button> = (0..COLOURS.len())
        .map(|i| {
            let s = gtk::Button::new();
            s.add_css_class("swatch");
            s.add_css_class(&format!("swatch-{i}"));
            s.set_tooltip_text(Some("Colour"));
            swatch_row.append(&s);
            s
        })
        .collect();
    panel.append(&swatch_row);
    let hint = ui::label("", &["caption"]);
    hint.set_xalign(0.5);
    hint.set_justify(gtk::Justification::Center);
    panel.append(&hint);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.append(&area);
    column.append(&panel);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&column));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));
    dialog.set_child(Some(&view));

    let ed = Rc::new(Editor {
        app: Rc::downgrade(app),
        persona,
        name,
        dialog: dialog.clone(),
        toasts,
        area: area.clone(),
        undo: undo.clone(),
        hint,
        pens,
        swatches,
        photo: RefCell::new(None),
        sketch: RefCell::new(Sketch::default()),
        tool: Cell::new(Tool::Draw),
        pen: Cell::new(1),
        colour: Cell::new(COLOURS[2]),
        dragging: Cell::new(None),
        moved: Cell::new(false),
        finished: Cell::new(false),
        done: Box::new(done),
    });
    ed.set_tool(Tool::Draw);
    ed.set_colour(2);
    ed.gestures();

    let e = Rc::downgrade(&ed);
    area.set_draw_func(move |_, cr, _, _| {
        if let Some(e) = e.upgrade() {
            e.draw(cr);
        }
    });
    let e = Rc::downgrade(&ed);
    undo.connect_clicked(move |_| {
        let Some(e) = e.upgrade() else {
            return;
        };
        e.sketch.borrow_mut().undo();
        e.changed();
    });
    let e = Rc::downgrade(&ed);
    finish.connect_clicked(move |_| {
        if let Some(e) = e.upgrade() {
            e.finish();
        }
    });
    let e = Rc::downgrade(&ed);
    draw.connect_toggled(move |b| {
        let Some(e) = e.upgrade() else {
            return;
        };
        if b.is_active() {
            e.set_tool(Tool::Draw);
        }
    });
    let e = Rc::downgrade(&ed);
    text.connect_toggled(move |b| {
        let Some(e) = e.upgrade() else {
            return;
        };
        if b.is_active() {
            e.set_tool(Tool::Text);
        }
    });
    for (i, b) in pen_buttons.iter().enumerate() {
        let e = Rc::downgrade(&ed);
        b.connect_toggled(move |b| {
            let Some(e) = e.upgrade() else {
                return;
            };
            if b.is_active() {
                e.pen.set(i);
            }
        });
    }
    for (i, s) in ed.swatches.iter().enumerate() {
        let e = Rc::downgrade(&ed);
        s.connect_clicked(move |_| {
            if let Some(e) = e.upgrade() {
                e.set_colour(i);
            }
        });
    }
    dialog.connect_close_attempt(move |d| {
        let d2 = d.clone();
        let q = ui::alert(
            "Discard your changes?",
            "",
            &[("keep", "Keep editing"), ("discard", "Discard")],
        );
        q.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        q.connect_response(Some("discard"), move |_, _| d2.force_close());
        q.present(Some(d));
    });
    // Widgets' handlers hold the editor weakly: this keeps it while the
    // dialog is open. However it closes, it's cancelled unless Done
    // finished it.
    let e = RefCell::new(Some(ed.clone()));
    dialog.connect_closed(move |_| {
        if let Some(e) = e.take()
            && !e.finished.get()
        {
            (e.done)(None);
        }
    });

    dialog.present(Some(&app.window));
    let (weak, dd) = (Rc::downgrade(&ed), dialog.clone());
    drop(ed);
    ui::bg(
        move || viewer::load_bounded(&source, viewer::MAX_SIDE),
        move |r| {
            let Some(ed) = weak.upgrade() else { return };
            match r.and_then(|d| d.surface()) {
                Ok(s) => {
                    *ed.photo.borrow_mut() = Some(s);
                    ed.area.queue_draw();
                    ed.area.grab_focus();
                }
                Err(_) => {
                    if let Some(app) = ed.app.upgrade() {
                        app.toast("Couldn't open this image");
                    }
                    dd.force_close();
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_maps_view_and_photo_points_both_ways() {
        // A landscape photo in a portrait view: letterboxed top and bottom.
        let f = Fit::new((400.0, 200.0), (200.0, 400.0));
        assert!((f.scale - 0.5).abs() < 1e-12);
        assert!((f.dx).abs() < 1e-12 && (f.dy - 150.0).abs() < 1e-12);
        for p in [(0.0, 0.0), (37.5, 81.25), (200.0, 400.0)] {
            let (x, y) = f.to_photo(p.0, p.1);
            let (bx, by) = f.to_view(x, y);
            assert!((bx - p.0).abs() < 1e-9 && (by - p.1).abs() < 1e-9);
        }
        assert_eq!(f.to_view(0.0, 0.0), (0.0, 150.0));
        assert_eq!(f.to_view(400.0, 200.0), (200.0, 250.0));
    }

    #[test]
    fn rendered_pixels_become_rgb() {
        // Two pixels a row, padded to 12 bytes: opaque orange, half-alpha red.
        let mut data = vec![0u8; 12];
        data[..4].copy_from_slice(&0xffff_8000_u32.to_ne_bytes());
        data[4..8].copy_from_slice(&0x8080_0000_u32.to_ne_bytes());
        data[8..12].copy_from_slice(&[9, 9, 9, 9]);
        assert_eq!(argb_to_rgb(&data, 2, 1, 12), vec![255, 128, 0, 255, 0, 0]);
        // Fully transparent: black.
        assert_eq!(argb_to_rgb(&[0, 0, 0, 0], 1, 1, 4), vec![0, 0, 0]);
    }

    #[test]
    fn edited_photo_is_a_plain_jpeg() {
        let dir = viewer::tests::temp_dir("edited");
        let mut photo = cairo::ImageSurface::create(cairo::Format::ARgb32, 64, 48).unwrap();
        {
            let cr = cairo::Context::new(&photo).unwrap();
            cr.set_source_rgb(0.0, 0.0, 1.0);
            cr.paint().unwrap();
        }
        photo.flush();
        let _ = photo.data();
        let mut sketch = Sketch::default();
        sketch.begin_stroke(0.0, 24.0, 0xE5_3935, 8.0);
        sketch.end_stroke(64.0, 24.0);
        let (rgb, w, h) = render(&photo, &sketch).unwrap();
        assert_eq!((w, h), (64, 48));
        let path = dir.join("x-edited.jpg");
        std::fs::write(&path, b"").unwrap();
        write_jpeg(&path, rgb, w, h).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xff, 0xd8]);
        // No APP1 (Exif) segment among the headers.
        let mut i = 2;
        while i + 4 <= bytes.len() && bytes[i] == 0xff && bytes[i + 1] != 0xda {
            assert_ne!(bytes[i + 1], 0xe1, "an Exif segment");
            i += 2 + usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
        }
        let back = gdk::gdk_pixbuf::Pixbuf::from_file(&path).unwrap();
        assert_eq!((back.width(), back.height()), (64, 48));
        let px = back.read_pixel_bytes();
        let (stride, n) = (back.rowstride() as usize, back.n_channels() as usize);
        let at = |x: usize, y: usize| {
            let i = y * stride + x * n;
            (px[i], px[i + 1], px[i + 2])
        };
        let (r, g, b) = at(32, 24);
        assert!(r > 180 && g < 100 && b < 100, "stroke: {:?}", (r, g, b));
        let (r, g, b) = at(2, 2);
        assert!(b > 180 && r < 60 && g < 60, "corner: {:?}", (r, g, b));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
