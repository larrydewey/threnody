//! Scanning QR codes: invites, device link codes and directory links,
//! from the camera, an image file or a pasted image. Camera frames go to
//! the preview and the decoder only; nothing is kept, and the camera
//! stops (light off) when the scanner is dropped.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gtk::gdk::gdk_pixbuf::{Pixbuf, PixbufLoader};
use gtk::{gdk, gio, glib};

use crate::{camera, ui, viewer};

/// Decoding size: enough for a QR code, cheap to decode (Android: up to
/// 1920×1080).
const DECODE_MAX: i32 = 1280;
pub const PREVIEW_WIDTH: i32 = 640;
/// Most bytes of a pasted image read from the clipboard.
pub const PASTE_MAX: usize = 32 << 20;
/// Pasted image types `PixbufLoader` reads.
const IMAGE_TYPES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/bmp",
    "image/gif",
    "image/webp",
];

/// What the camera's frames go through: one branch of greyscale frames
/// for the decoder, one of mirrored RGBA frames for the preview, each
/// dropping frames while the last is still being handled.
fn pipeline() -> String {
    format!(
        "videoconvert name=head ! tee name=t \
         t. ! queue leaky=downstream max-size-buffers=1 ! videoscale ! videoconvert \
            ! video/x-raw,format=GRAY8,width=[16,{DECODE_MAX}],height=[16,{DECODE_MAX}],pixel-aspect-ratio=1/1 \
            ! appsink name=decode sync=false max-buffers=1 drop=true \
         t. ! queue leaky=downstream max-size-buffers=1 ! videoflip method=horizontal-flip \
            ! videoscale ! videoconvert ! video/x-raw,format=RGBA,width={PREVIEW_WIDTH},pixel-aspect-ratio=1/1 \
            ! appsink name=preview sync=false max-buffers=1 drop=true"
    )
}

/// `(width, height, RGBA bytes)` of a preview frame.
type OnPreview = Box<dyn Fn(u32, u32, &[u8]) + Send + Sync>;
type OnFound = Box<dyn Fn(String) + Send + Sync>;
/// An error the running pipeline reported (a busy or unplugged camera,
/// say), once.
type OnFailed = Box<dyn Fn(String) + Send + Sync>;

/// A camera reading QR codes.
pub struct Scanner {
    pipeline: gst::Pipeline,
}

/// Width and height from a sample's caps.
fn sample_size(sample: &gst::Sample) -> Option<(usize, usize)> {
    let s = sample.caps()?.structure(0)?;
    let w = usize::try_from(s.get::<i32>("width").ok()?).ok()?;
    let h = usize::try_from(s.get::<i32>("height").ok()?).ok()?;
    Some((w, h))
}

impl Scanner {
    /// Starts `source` (`camera::source(which)` in the app; tests pass a
    /// still image). `preview` gets mirrored RGBA frames, `found` the
    /// first code's text, once, and `failed` the first error reported
    /// after the start (a camera fails late: busy, unplugged, or with no
    /// format the pipeline takes).
    pub fn start(
        source: gst::Element,
        preview: OnPreview,
        found: OnFound,
        failed: OnFailed,
    ) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let pipeline = gst::parse::launch(&pipeline())
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_owned())?;
        let head = pipeline.by_name("head").ok_or("no head")?;
        pipeline.add(&source).map_err(|e| e.to_string())?;
        source.link(&head).map_err(|e| e.to_string())?;
        let sink = |name: &str| {
            pipeline
                .by_name(name)
                .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
                .ok_or("no appsink")
        };
        let done = AtomicBool::new(false);
        sink("decode")?.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if done.load(Ordering::Relaxed) {
                        return Ok(gst::FlowSuccess::Ok);
                    }
                    let (Some((w, h)), Some(buf)) = (sample_size(&sample), sample.buffer()) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    // GRAY8 rows from videoconvert are 4-byte aligned.
                    let stride = (w + 3) & !3;
                    if let Ok(map) = buf.map_readable()
                        && map.len() >= stride * h
                        && let (Ok(w), Ok(h), Ok(st)) =
                            (u32::try_from(w), u32::try_from(h), u32::try_from(stride))
                        && let Some(text) = threnody_ffi::decode_qr(w, h, st, map.to_vec())
                        && !done.swap(true, Ordering::Relaxed)
                    {
                        found(text);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        sink("preview")?.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let (Some((w, h)), Some(buf)) = (sample_size(&sample), sample.buffer())
                        && let Ok(map) = buf.map_readable()
                        && map.len() >= w * 4 * h
                        && let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h))
                    {
                        preview(w, h, map.as_slice());
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        // Nothing else reads the bus: errors go to `failed`, the rest is
        // dropped rather than queued for as long as the camera runs.
        let bus = pipeline.bus().ok_or("no bus")?;
        let reported = AtomicBool::new(false);
        bus.set_sync_handler(move |_, msg| {
            if let gst::MessageView::Error(e) = msg.view()
                && !reported.swap(true, Ordering::Relaxed)
            {
                failed(e.error().to_string());
            }
            gst::BusSyncReply::Drop
        });
        let scanner = Self { pipeline };
        scanner
            .pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "the camera didn't start (is another app using it?)".to_owned())?;
        Ok(scanner)
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Greyscale (one byte a pixel, BT.601) from RGB or RGBA rows
/// (`channels`: 3 or 4) `stride` bytes apart.
pub fn luma(src: &[u8], w: usize, h: usize, stride: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        let Some(row) = src.get(y * stride..y * stride + w * channels) else {
            out.resize(w * h, 0);
            break;
        };
        for px in row.chunks_exact(channels) {
            let v = 77 * u32::from(px[0]) + 150 * u32::from(px[1]) + 29 * u32::from(px[2]);
            out.push((v >> 8) as u8);
        }
    }
    out
}

/// The QR code in a decoded image, if any.
fn decode_rgb(d: &viewer::Decoded) -> Option<String> {
    let (w, h) = (
        usize::try_from(d.width).ok()?,
        usize::try_from(d.height).ok()?,
    );
    let grey = luma(&d.bytes, w, h, d.stride, if d.has_alpha { 4 } else { 3 });
    threnody_ffi::decode_qr(
        u32::try_from(w).ok()?,
        u32::try_from(h).ok()?,
        u32::try_from(w).ok()?,
        grey,
    )
}

/// The QR code in an image file, if any (bounded like the viewer).
pub fn decode_file(path: &Path) -> Result<Option<String>, String> {
    Ok(decode_rgb(&viewer::load_bounded(path, DECODE_MAX)?))
}

/// The QR code in pasted image bytes of type `mime` (one of
/// [`IMAGE_TYPES`]), if any. Only that type's loader reads them: the
/// bytes aren't sniffed, so another app can't reach every image loader
/// installed. The loader learns the size first: an image over
/// [`viewer::MAX_PIXELS`] is refused before it is decoded, others are
/// scaled to at most [`DECODE_MAX`]. Blocking: run it on a worker thread.
pub fn decode_image_bytes(mime: &str, bytes: &[u8]) -> Result<Option<String>, String> {
    // Checked here too: a gdk-pixbuf built on glycin sniffs whatever
    // type it was told.
    if !is_image_type(mime, bytes) {
        return Err("not a picture this computer can read".into());
    }
    let loader = PixbufLoader::with_mime_type(mime)
        .map_err(|_| "not a picture this computer can read".to_owned())?;
    let too_big = Rc::new(Cell::new(false));
    let t = too_big.clone();
    loader.connect_size_prepared(move |l, w, h| {
        if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > viewer::MAX_PIXELS {
            t.set(true);
            // A size of 0 makes the loader stop before it decodes.
            l.set_size(0, 0);
        } else if w.max(h) > DECODE_MAX {
            let scale = f64::from(DECODE_MAX) / f64::from(w.max(h));
            #[allow(clippy::cast_possible_truncation)]
            l.set_size(
                ((f64::from(w) * scale).round() as i32).max(1),
                ((f64::from(h) * scale).round() as i32).max(1),
            );
        }
    });
    let wrote = loader.write(bytes);
    let closed = loader.close();
    if too_big.get() {
        return Err("too large a picture".into());
    }
    wrote
        .and(closed)
        .map_err(|_| "not a picture this computer can read".to_owned())?;
    let pixbuf: Pixbuf = loader
        .pixbuf()
        .ok_or("not a picture this computer can read")?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Ok(decode_rgb(&viewer::Decoded {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: usize::try_from(pixbuf.rowstride()).unwrap_or(0),
        has_alpha: pixbuf.has_alpha(),
        bytes: pixbuf.read_pixel_bytes(),
    }))
}

/// Whether `bytes` start as an image of type `mime` (one of
/// [`IMAGE_TYPES`]) does.
fn is_image_type(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/bmp" => bytes.starts_with(b"BM"),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

/// What the camera thread hands the dialog.
enum Seen {
    Frame(u32, u32, Vec<u8>),
    Found(String),
    Failed(String),
}

/// The scan dialog; `found` gets the text of the first code.
pub struct ScanDialog {
    dialog: adw::Dialog,
    preview: gtk::Picture,
    status: adw::StatusPage,
    note: gtk::Label,
    scanner: RefCell<Option<Scanner>>,
    which: Cell<usize>,
    /// Cameras found (counted off the main thread once).
    cameras: Cell<usize>,
    /// Bumped by each camera start: a slower, older start is dropped.
    started: Cell<u64>,
    /// Closed, or a code was found: no camera starts any more.
    done: Cell<bool>,
    found: Box<dyn Fn(String)>,
}

impl ScanDialog {
    pub fn open(parent: &adw::ApplicationWindow, found: impl Fn(String) + 'static) -> Rc<Self> {
        let dialog = adw::Dialog::builder()
            .title("Scan a QR code")
            .content_width(680)
            .content_height(620)
            .build();
        let title = adw::WindowTitle::new(
            "Scan a QR code",
            "An invite, a device link code or a directory link",
        );
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));
        let open_image = gtk::Button::from_icon_name("document-open-symbolic");
        open_image.set_tooltip_text(Some("Open an image… (or paste one with Ctrl+V)"));
        header.pack_start(&open_image);
        let switch = gtk::Button::from_icon_name("camera-switch-symbolic");
        switch.set_tooltip_text(Some("Switch camera"));
        switch.set_visible(false);
        header.pack_end(&switch);

        let preview = gtk::Picture::builder()
            .can_shrink(true)
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();
        let status = adw::StatusPage::builder()
            .icon_name("camera-web-symbolic")
            .title("Starting the camera…")
            .build();
        let open_button = gtk::Button::with_label("Open an image…");
        open_button.add_css_class("pill");
        open_button.set_halign(gtk::Align::Center);
        status.set_child(Some(&open_button));
        let overlay = gtk::Overlay::new();
        overlay.add_css_class("scan-preview");
        overlay.set_child(Some(&preview));
        overlay.add_overlay(&status);
        let note = ui::label(
            "Frames stay on this computer and aren't kept.",
            &["caption", "dim-label"],
        );
        note.set_xalign(0.5);
        note.set_justify(gtk::Justification::Center);
        note.set_margin_top(6);
        note.set_margin_bottom(6);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&overlay);
        column.append(&note);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&column));
        dialog.set_child(Some(&view));

        let this = Rc::new(Self {
            dialog: dialog.clone(),
            preview,
            status,
            note,
            scanner: RefCell::new(None),
            which: Cell::new(0),
            cameras: Cell::new(1),
            started: Cell::new(0),
            done: Cell::new(false),
            found: Box::new(found),
        });
        let weak = Rc::downgrade(&this);
        open_image.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.open_image();
            }
        });
        let weak = Rc::downgrade(&this);
        open_button.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.open_image();
            }
        });
        let weak = Rc::downgrade(&this);
        switch.connect_clicked(move |_| {
            if let Some(t) = weak.upgrade() {
                t.stop_camera();
                let n = t.cameras.get().max(1);
                t.which.set((t.which.get() + 1) % n);
                t.status.set_title("Starting the camera…");
                t.status.set_description(None);
                t.status.set_visible(true);
                t.start_camera();
            }
        });
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(&this);
        keys.connect_key_pressed(move |_, key, _, state| {
            if state.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::v | gdk::Key::V)
                && let Some(t) = weak.upgrade()
            {
                t.paste();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        dialog.add_controller(keys);
        // However it closes, the camera goes off.
        let weak = Rc::downgrade(&this);
        dialog.connect_closed(move |_| {
            if let Some(t) = weak.upgrade() {
                t.done.set(true);
                t.stop_camera();
            }
        });
        dialog.present(Some(parent));
        this.start_camera();
        // Device enumeration can take a moment: off the main thread.
        let weak = Rc::downgrade(&this);
        ui::bg(camera::count, move |n| {
            let Some(t) = weak.upgrade() else { return };
            t.cameras.set(n);
            if n > 1 && !t.done.get() {
                switch.set_visible(true);
            }
        });
        this
    }

    /// Present again (it is already open).
    pub fn present(&self, parent: &adw::ApplicationWindow) {
        if !self.done.get() {
            self.dialog.present(Some(parent));
        }
    }

    pub fn is_open(&self) -> bool {
        !self.done.get()
    }

    fn show_status(&self, title: &str, body: &str) {
        self.status.set_title(title);
        self.status.set_description(Some(body));
        self.status.set_visible(true);
    }

    /// A code was read: the dialog closes and `found` gets it.
    fn finish(&self, text: String) {
        if self.done.replace(true) {
            return;
        }
        self.stop_camera();
        self.dialog.close();
        (self.found)(text);
    }

    /// Stops the camera; the pipeline is torn down off the main thread
    /// (closing a v4l2 device can take a moment).
    fn stop_camera(&self) {
        if let Some(s) = self.scanner.borrow_mut().take() {
            ui::bg_quiet(move || drop(s));
        }
    }

    fn start_camera(self: &Rc<Self>) {
        let (tx, rx) = async_channel::bounded::<Seen>(1);
        let (frames, codes, errors) = (tx.clone(), tx.clone(), tx);
        let which = self.which.get();
        let generation = self.started.get() + 1;
        self.started.set(generation);
        // This start failed: a scanner arriving after its error is dropped.
        let failed = Rc::new(Cell::new(false));
        let failed2 = failed.clone();
        let weak = Rc::downgrade(self);
        ui::bg(
            move || {
                let source = camera::source(which)?;
                Scanner::start(
                    source,
                    Box::new(move |w, h, px| {
                        let _ = frames.try_send(Seen::Frame(w, h, px.to_vec()));
                    }),
                    Box::new(move |text| {
                        let _ = codes.send_blocking(Seen::Found(text));
                    }),
                    Box::new(move |e| {
                        let _ = errors.send_blocking(Seen::Failed(e));
                    }),
                )
            },
            move |r| {
                let Some(t) = weak.upgrade() else { return };
                let stale = t.done.get() || t.started.get() != generation || failed2.get();
                match r {
                    Ok(s) if stale => ui::bg_quiet(move || drop(s)),
                    Err(_) if stale => {}
                    Ok(s) => {
                        t.status.set_visible(false);
                        *t.scanner.borrow_mut() = Some(s);
                    }
                    Err(e) => t.show_status(
                        "No camera",
                        &format!("{e}. You can open an image of the code instead."),
                    ),
                }
            },
        );
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(seen) = rx.recv().await {
                let Some(t) = weak.upgrade() else { return };
                if t.done.get() || t.started.get() != generation {
                    return;
                }
                match seen {
                    Seen::Frame(w, h, px) => {
                        let (Ok(wi), Ok(hi)) = (i32::try_from(w), i32::try_from(h)) else {
                            continue;
                        };
                        let texture = gdk::MemoryTexture::new(
                            wi,
                            hi,
                            gdk::MemoryFormat::R8g8b8a8,
                            &glib::Bytes::from_owned(px),
                            w as usize * 4,
                        );
                        t.preview.set_paintable(Some(&texture));
                    }
                    Seen::Found(text) => {
                        t.finish(text);
                        return;
                    }
                    Seen::Failed(e) => {
                        failed.set(true);
                        t.stop_camera();
                        t.preview.set_paintable(gdk::Paintable::NONE);
                        t.show_status(
                            "No camera",
                            &format!("{e}. You can open an image of the code instead."),
                        );
                        return;
                    }
                }
            }
        });
    }

    /// *Open an image…*: a code in a picture file (a screenshot, say).
    fn open_image(self: &Rc<Self>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Images"));
        filter.add_pixbuf_formats();
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let d = gtk::FileDialog::builder()
            .title("Open an image of a QR code")
            .filters(&filters)
            .modal(true)
            .build();
        let weak = Rc::downgrade(self);
        let parent = self.dialog.root().and_downcast::<gtk::Window>();
        d.open(parent.as_ref(), gio::Cancellable::NONE, move |r| {
            let Some(path) = r.ok().and_then(|f| f.path()) else {
                return;
            };
            ui::bg(
                move || decode_file(&path),
                move |r| {
                    if let Some(t) = weak.upgrade() {
                        t.decoded(r);
                    }
                },
            );
        });
    }

    /// Ctrl+V: a code in a copied image, read with a bound and decoded
    /// off the main thread.
    fn paste(self: &Rc<Self>) {
        let cb = self.dialog.clipboard();
        let formats = cb.formats();
        let types: Vec<&str> = IMAGE_TYPES
            .into_iter()
            .filter(|m| formats.contain_mime_type(m))
            .collect();
        if types.is_empty() {
            self.note.set_text("There's no image on the clipboard.");
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok((stream, mime)) = cb.read_future(&types, glib::Priority::DEFAULT).await else {
                return;
            };
            let mime = mime.to_string();
            let read = stream
                .read_all_future(vec![0u8; PASTE_MAX + 1], glib::Priority::DEFAULT)
                .await;
            let _ = stream.close_future(glib::Priority::DEFAULT).await;
            let Some(t) = weak.upgrade() else { return };
            let bytes = match read {
                Ok((mut buf, n, None)) if n <= PASTE_MAX => {
                    buf.truncate(n);
                    buf
                }
                Ok((_, n, None)) if n > PASTE_MAX => {
                    t.note.set_text("That image is too large.");
                    return;
                }
                _ => {
                    t.note.set_text("Couldn't read the pasted image.");
                    return;
                }
            };
            let weak = Rc::downgrade(&t);
            ui::bg(
                move || decode_image_bytes(&mime, &bytes),
                move |r| {
                    if let Some(t) = weak.upgrade() {
                        t.decoded(r);
                    }
                },
            );
        });
    }

    fn decoded(&self, r: Result<Option<String>, String>) {
        match r {
            Ok(Some(text)) => self.finish(text),
            Ok(None) => self.note.set_text("No QR code found in that image"),
            Err(e) => self.note.set_text(&format!("Couldn't read it: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::tests::{fake_png, temp_dir};
    use std::sync::Arc;

    /// A PNG of `text` as a QR code: 6 pixels a module, 4 modules of quiet
    /// zone, black on white.
    fn qr_png(text: &str, path: &Path) {
        let q = threnody_ffi::qr_matrix(text.into()).unwrap();
        let (n, scale, quiet) = (q.size as usize, 6, 4);
        let side = (n + 2 * quiet) * scale;
        let p = Pixbuf::new(
            gdk::gdk_pixbuf::Colorspace::Rgb,
            false,
            8,
            side as i32,
            side as i32,
        )
        .unwrap();
        p.fill(0xffff_ffff);
        let black = Pixbuf::new(
            gdk::gdk_pixbuf::Colorspace::Rgb,
            false,
            8,
            scale as i32,
            scale as i32,
        )
        .unwrap();
        black.fill(0x0000_00ff);
        for y in 0..n {
            for x in 0..n {
                if q.dark[y * n + x] {
                    black.copy_area(
                        0,
                        0,
                        scale as i32,
                        scale as i32,
                        &p,
                        ((x + quiet) * scale) as i32,
                        ((y + quiet) * scale) as i32,
                    );
                }
            }
        }
        p.savev(path, "png", &[]).unwrap();
    }

    const LINK: &str = "threnody://ABCDEFGHJKMNPQRSTVWXYZ0123456789@192.0.2.7:7450";

    #[test]
    fn luma_handles_row_padding() {
        // 3×2 RGB, rows 12 bytes apart.
        let mut rgb = vec![0u8; 24];
        rgb[..9].copy_from_slice(&[255, 0, 0, 128, 128, 128, 0, 0, 0]);
        rgb[12..21].copy_from_slice(&[255, 255, 255, 0, 255, 0, 0, 0, 255]);
        let g = luma(&rgb, 3, 2, 12, 3);
        assert_eq!(g.len(), 6);
        assert!((i32::from(g[0]) - 76).abs() <= 1, "red: {}", g[0]);
        assert_eq!((g[1], g[2], g[3]), (128, 0, 255));
        // RGBA with padding: grey stays grey.
        let mut rgba = vec![0u8; 24];
        for (i, v) in [10u8, 200, 77].iter().enumerate() {
            rgba[i * 4..i * 4 + 4].copy_from_slice(&[*v, *v, *v, 255]);
        }
        let g = luma(&rgba, 3, 2, 12, 4);
        assert_eq!(&g[..3], &[10, 200, 77]);
    }

    #[test]
    fn qr_codes_in_image_files_decode() {
        let dir = temp_dir("scan-file");
        let qr = dir.join("qr.png");
        qr_png(LINK, &qr);
        assert_eq!(decode_file(&qr), Ok(Some(LINK.to_owned())));
        let blank = dir.join("blank.png");
        let p = Pixbuf::new(gdk::gdk_pixbuf::Colorspace::Rgb, false, 8, 200, 200).unwrap();
        p.fill(0xffff_ffff);
        p.savev(&blank, "png", &[]).unwrap();
        assert_eq!(decode_file(&blank), Ok(None));
        let bomb = dir.join("bomb.png");
        std::fs::write(&bomb, fake_png(20_000, 20_000)).unwrap();
        assert!(decode_file(&bomb).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pasted_images_decode_and_bombs_are_refused() {
        let dir = temp_dir("scan-paste");
        let qr = dir.join("qr.png");
        qr_png(LINK, &qr);
        let bytes = std::fs::read(&qr).unwrap();
        assert_eq!(
            decode_image_bytes("image/png", &bytes),
            Ok(Some(LINK.to_owned()))
        );
        assert!(decode_image_bytes("image/png", &fake_png(20_000, 20_000)).is_err());
        // Only the type asked for is read: a PNG offered as a JPEG, or
        // an SVG offered as a PNG, isn't sniffed and decoded anyway.
        assert!(decode_image_bytes("image/jpeg", &bytes).is_err());
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="white"/></svg>"#;
        assert!(decode_image_bytes("image/png", svg).is_err());
        assert!(decode_image_bytes("image/svg+xml", svg).is_err());
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        assert!(decode_image_bytes("image/png", &noise).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanner_finds_a_code_in_a_still_stream() {
        let needed = [
            "filesrc",
            "pngdec",
            "imagefreeze",
            "videoconvert",
            "videoscale",
            "videoflip",
            "tee",
            "queue",
            "appsink",
        ];
        if gst::init().is_err()
            || needed
                .iter()
                .any(|e| gst::ElementFactory::find(e).is_none())
        {
            eprintln!("skipped: GStreamer or one of {needed:?} is missing");
            return;
        }
        let dir = temp_dir("scan-stream");
        let qr = dir.join("qr.png");
        qr_png(LINK, &qr);
        let source = gst::parse::bin_from_description(
            &format!("filesrc location={} ! pngdec ! imagefreeze", qr.display()),
            true,
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let widths = Arc::new(std::sync::Mutex::new(Vec::new()));
        let w2 = widths.clone();
        let tx = std::sync::Mutex::new(tx);
        let scanner = Scanner::start(
            source.upcast(),
            Box::new(move |w, _, _| w2.lock().unwrap().push(w)),
            Box::new(move |text| {
                let _ = tx.lock().unwrap().send(text);
            }),
            Box::new(|_| {}),
        )
        .unwrap();
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert_eq!(got.as_deref(), Ok(LINK));
        // Only once.
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(500))
                .is_err()
        );
        let started = std::time::Instant::now();
        drop(scanner);
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        let widths = widths.lock().unwrap();
        assert!(!widths.is_empty() && widths.iter().all(|w| *w == PREVIEW_WIDTH as u32));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_camera_failing_after_the_start_is_reported() {
        let needed = ["videotestsrc", "identity", "videoconvert", "appsink"];
        if gst::init().is_err()
            || needed
                .iter()
                .any(|e| gst::ElementFactory::find(e).is_none())
        {
            eprintln!("skipped: GStreamer or one of {needed:?} is missing");
            return;
        }
        // A source that fails on its streaming thread after a few
        // frames, once the pipeline has started, as a busy or unplugged
        // camera does.
        let source =
            gst::parse::bin_from_description("videotestsrc ! identity error-after=3", true)
                .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let scanner = Scanner::start(
            source.upcast(),
            Box::new(|_, _, _| {}),
            Box::new(|_| {}),
            Box::new(move |e| {
                let _ = tx.lock().unwrap().send(e);
            }),
        )
        .unwrap();
        let got = rx.recv_timeout(std::time::Duration::from_secs(5));
        assert!(got.is_ok(), "no error reported");
        // Once, though the source fails too.
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err()
        );
        drop(scanner);
    }

    #[test]
    #[ignore = "needs a camera"]
    fn scanner_opens_the_camera() {
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f = frames.clone();
        let scanner = Scanner::start(
            camera::source(0).unwrap(),
            Box::new(move |_, _, _| {
                f.fetch_add(1, Ordering::Relaxed);
            }),
            Box::new(|_| {}),
            Box::new(|_| {}),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(scanner);
        assert!(frames.load(Ordering::Relaxed) >= 20);
    }
}
