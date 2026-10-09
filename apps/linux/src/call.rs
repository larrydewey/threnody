//! Calls: the bar across the top of the window while a call rings or runs,
//! the dialog for an incoming one, and the video while either side sends
//! it. The audio is the node's (WebRTC on the default microphone and
//! speaker); our video comes from the camera ([`crate::camera`]) and the
//! peer's from the node, drawn here.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::camera::Camera;
use crate::core::Core;
use crate::ui::{self, bg, bg_quiet};

/// The call on screen.
struct Shown {
    persona: Option<String>,
    id: u64,
    title: String,
    /// When audio connected (for the timer); `None` before.
    since: Option<std::time::Instant>,
    dialog: Option<adw::AlertDialog>,
    /// We want to send video once the call runs.
    video: bool,
    /// The peer's video reaches us while this is set.
    watching: Arc<AtomicBool>,
}

/// A frame for one of the two pictures.
struct Picture {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

pub struct CallBar {
    core: Arc<Core>,
    pub widget: gtk::Revealer,
    who: gtk::Label,
    status: gtk::Label,
    mute: gtk::ToggleButton,
    camera_button: gtk::ToggleButton,
    /// Next camera, when there's more than one.
    flip: gtk::Button,
    /// Which camera we send (of `camera::count`).
    which: Cell<usize>,
    video: gtk::Overlay,
    remote: gtk::Picture,
    local: gtk::Picture,
    shown: RefCell<Option<Shown>>,
    camera: RefCell<Option<Camera>>,
    peer_video: Cell<bool>,
    /// The one-second timer while a call runs.
    ticking: Cell<bool>,
    /// Set while `camera_button` is changed from here, not by a click.
    syncing: Cell<bool>,
}

impl CallBar {
    pub fn new(core: Arc<Core>) -> Rc<Self> {
        let who = ui::label("", &["heading"]);
        who.set_wrap(false);
        who.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let status = ui::label("", &["dim-label", "numeric"]);
        status.set_wrap(false);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        text.append(&who);
        text.append(&status);

        let round = |icon: &str, tip: &str| {
            let b = gtk::ToggleButton::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .valign(gtk::Align::Center)
                .build();
            b.add_css_class("circular");
            b
        };
        let mute = round("microphone-sensitivity-high-symbolic", "Mute");
        let camera_button = round("camera-disabled-symbolic", "Turn camera on");
        let flip = gtk::Button::builder()
            .icon_name("camera-switch-symbolic")
            .tooltip_text("Switch camera")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        flip.add_css_class("circular");
        let hangup = gtk::Button::builder()
            .icon_name("call-stop-symbolic")
            .tooltip_text("Hang up")
            .valign(gtk::Align::Center)
            .build();
        hangup.add_css_class("circular");
        hangup.add_css_class("destructive-action");

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        row.add_css_class("call-bar");
        row.append(&gtk::Image::from_icon_name("call-start-symbolic"));
        row.append(&text);
        row.append(&flip);
        row.append(&camera_button);
        row.append(&mute);
        row.append(&hangup);

        // The peer fills the area; we're in the corner.
        let remote = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();
        let local = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .width_request(160)
            .height_request(120)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_end(12)
            .margin_bottom(12)
            .build();
        local.add_css_class("call-self");
        local.set_visible(false);
        let video = gtk::Overlay::builder()
            .child(&remote)
            .height_request(360)
            .build();
        video.add_css_class("call-video");
        video.add_overlay(&local);
        video.set_visible(false);

        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        column.append(&row);
        column.append(&video);
        let widget = gtk::Revealer::builder()
            .child(&column)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();

        let this = Rc::new(Self {
            core,
            widget,
            who,
            status,
            mute,
            camera_button,
            flip,
            which: Cell::new(0),
            video,
            remote,
            local,
            shown: RefCell::new(None),
            camera: RefCell::new(None),
            peer_video: Cell::new(false),
            ticking: Cell::new(false),
            syncing: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        hangup.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.hang_up();
            }
        });
        let weak = Rc::downgrade(&this);
        this.mute.connect_toggled(move |b| {
            let Some(this) = weak.upgrade() else { return };
            let muted = b.is_active();
            b.set_icon_name(if muted {
                "microphone-sensitivity-muted-symbolic"
            } else {
                "microphone-sensitivity-high-symbolic"
            });
            b.set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
            if let Some(node) = this.node() {
                bg_quiet(move || node.set_call_muted(muted));
            }
        });
        let weak = Rc::downgrade(&this);
        this.flip.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.switch_camera();
            }
        });
        let weak = Rc::downgrade(&this);
        this.camera_button.connect_toggled(move |b| {
            let Some(this) = weak.upgrade() else { return };
            if !this.syncing.get() {
                this.set_video(b.is_active());
            }
        });
        this
    }

    fn node(&self) -> Option<crate::core::Node> {
        let persona = self.shown.borrow().as_ref()?.persona.clone();
        self.core.node(persona.as_deref()).ok()
    }

    /// Whether a call is on screen (ringing or running).
    pub fn busy(&self) -> bool {
        self.shown.borrow().is_some()
    }

    fn show(&self, persona: Option<String>, id: u64, title: &str, status: &str, video: bool) {
        self.who.set_label(title);
        self.status.set_label(status);
        self.mute.set_active(false);
        self.mute.set_visible(false);
        self.camera_button.set_visible(false);
        self.show_camera_button(false);
        self.peer_video.set(false);
        self.widget.set_reveal_child(true);
        *self.shown.borrow_mut() = Some(Shown {
            persona,
            id,
            title: title.to_owned(),
            since: None,
            dialog: None,
            video,
            watching: Arc::default(),
        });
    }

    fn is(&self, id: u64) -> bool {
        self.shown.borrow().as_ref().is_some_and(|s| s.id == id)
    }

    /// We called (with video or not): shown until it's answered or ends.
    pub fn outgoing(&self, persona: Option<String>, id: u64, title: &str, video: bool) {
        let what = if video {
            "Video calling…"
        } else {
            "Calling…"
        };
        self.show(persona, id, title, what, video);
    }

    /// Someone calls: the bar, and a dialog to answer (with video too, if
    /// they offer theirs) or decline.
    pub fn incoming(
        self: &Rc<Self>,
        persona: Option<String>,
        id: u64,
        title: &str,
        video: bool,
        parent: &impl IsA<gtk::Widget>,
    ) {
        let what = if video {
            "Incoming video call"
        } else {
            "Incoming call"
        };
        self.show(persona, id, title, what, false);
        let mut responses = vec![("decline", "Decline"), ("answer", "Voice")];
        if video {
            responses.push(("video", "Video"));
        } else {
            responses[1].1 = "Answer";
        }
        let d = ui::alert(
            &format!("{title} is {}calling", if video { "video " } else { "" }),
            "Answer with your microphone and speaker; with Video, your camera too.",
            &responses,
        );
        d.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
        d.set_response_appearance(
            if video { "video" } else { "answer" },
            adw::ResponseAppearance::Suggested,
        );
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(s) = this.shown.borrow_mut().as_mut() {
                s.dialog = None;
            }
            match r {
                "answer" => this.answer(id, false),
                "video" => this.answer(id, true),
                _ => this.hang_up(),
            }
        });
        d.present(Some(parent));
        if let Some(s) = self.shown.borrow_mut().as_mut() {
            s.dialog = Some(d);
        }
    }

    fn answer(self: &Rc<Self>, id: u64, video: bool) {
        let Some(node) = self.node() else { return };
        self.status.set_label("Connecting…");
        if let Some(s) = self.shown.borrow_mut().as_mut() {
            s.video = video;
        }
        let weak = Rc::downgrade(self);
        bg(
            move || node.answer_call(id, video),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                match r {
                    Err(e) => this.status.set_label(&format!("Couldn't answer: {e}")),
                    Ok(()) => this.running(),
                }
            },
        );
    }

    fn hang_up(&self) {
        let Some(id) = self.shown.borrow().as_ref().map(|s| s.id) else {
            return;
        };
        if let Some(node) = self.node() {
            bg_quiet(move || node.hangup_call(id));
        }
    }

    pub fn ringing(&self, id: u64) {
        if self.is(id) {
            self.status.set_label("Ringing…");
        }
    }

    /// Our call was answered; `peer_video` says whether they send video.
    pub fn started(self: &Rc<Self>, id: u64, peer_video: bool) {
        if self.is(id) {
            self.status.set_label("Connecting…");
            self.peer_video.set(peer_video);
            self.running();
        }
    }

    /// The call runs: the controls, the peer's video, and our camera if
    /// we wanted it.
    fn running(self: &Rc<Self>) {
        self.mute.set_visible(true);
        self.camera_button.set_visible(true);
        self.watch_video();
        let wanted = self.shown.borrow().as_ref().is_some_and(|s| s.video);
        if wanted {
            self.start_camera();
        }
        self.layout();
    }

    /// The peer turned its video on or off.
    pub fn peer_video(&self, id: u64, on: bool) {
        if self.is(id) {
            self.peer_video.set(on);
            if !on {
                self.remote.set_paintable(gdk::Paintable::NONE);
            }
            self.layout();
        }
    }

    /// The next camera, if there's more than one.
    fn switch_camera(self: &Rc<Self>) {
        let n = crate::camera::count();
        if n < 2 || self.camera.borrow().is_none() {
            return;
        }
        self.which.set((self.which.get() + 1) % n);
        // Close this one first: some can't run alongside another.
        self.camera.borrow_mut().take();
        self.start_camera();
    }

    /// Shows the video area while either side sends video.
    fn layout(&self) {
        let ours = self.camera.borrow().is_some();
        self.flip.set_visible(ours && crate::camera::count() > 1);
        self.local.set_visible(ours);
        self.video.set_visible(ours || self.peer_video.get());
    }

    fn show_camera_button(&self, on: bool) {
        self.syncing.set(true);
        self.camera_button.set_active(on);
        self.syncing.set(false);
        self.camera_button.set_icon_name(if on {
            "camera-video-symbolic"
        } else {
            "camera-disabled-symbolic"
        });
        self.camera_button.set_tooltip_text(Some(if on {
            "Turn camera off"
        } else {
            "Turn camera on"
        }));
    }

    /// Turns our camera on or off, telling the peer.
    fn set_video(self: &Rc<Self>, on: bool) {
        if on {
            self.start_camera();
        } else {
            self.camera.borrow_mut().take();
            self.local.set_paintable(gdk::Paintable::NONE);
            self.show_camera_button(false);
            if let Some(node) = self.node() {
                bg_quiet(move || {
                    let _ = node.set_call_video(false);
                });
            }
        }
        self.layout();
    }

    fn start_camera(self: &Rc<Self>) {
        let Some(node) = self.node() else { return };
        if self.camera.borrow().is_some() {
            return;
        }
        let (tx, rx) = async_channel::bounded::<Picture>(1);
        let sender = node.clone();
        let started = Camera::start(
            self.which.get(),
            Box::new(move |w, h, i420| {
                sender.send_video_frame(w, h, 0, i420.to_vec());
            }),
            Box::new(move |width, height, rgba| {
                // The screen takes what it can; a busy one skips frames.
                let _ = tx.try_send(Picture {
                    width,
                    height,
                    rgba: rgba.to_vec(),
                });
            }),
        );
        match started {
            Ok(c) => {
                *self.camera.borrow_mut() = Some(c);
                self.show_camera_button(true);
                bg_quiet(move || {
                    let _ = node.set_call_video(true);
                });
                let local = self.local.clone();
                glib::spawn_future_local(async move {
                    while let Ok(p) = rx.recv().await {
                        local.set_paintable(Some(&texture(p)));
                    }
                });
            }
            Err(e) => {
                self.show_camera_button(false);
                self.status.set_label(&format!("No camera: {e}"));
            }
        }
        self.layout();
    }

    /// Draws the peer's video while the call lasts (a worker waits for its
    /// frames; only the newest is drawn).
    fn watch_video(&self) {
        let Some(node) = self.node() else { return };
        let Some(watching) = self.shown.borrow().as_ref().map(|s| s.watching.clone()) else {
            return;
        };
        if watching.swap(true, Ordering::SeqCst) {
            return;
        }
        let (tx, rx) = async_channel::bounded::<Picture>(1);
        let w = watching.clone();
        std::thread::spawn(move || {
            while w.load(Ordering::SeqCst) {
                if let Some(f) = node.next_video_frame(250) {
                    let (width, height, rgba) = upright(f.width, f.height, f.rotation, f.rgba);
                    let _ = tx.try_send(Picture {
                        width,
                        height,
                        rgba,
                    });
                }
            }
        });
        let remote = self.remote.clone();
        glib::spawn_future_local(async move {
            while let Ok(p) = rx.recv().await {
                remote.set_paintable(Some(&texture(p)));
            }
        });
    }

    /// The call's audio: "connected", "interrupted" or "failed".
    pub fn media(self: &Rc<Self>, id: u64, state: &str) {
        if !self.is(id) {
            return;
        }
        match state {
            "connected" => {
                if let Some(s) = self.shown.borrow_mut().as_mut()
                    && s.since.is_none()
                {
                    s.since = Some(std::time::Instant::now());
                    self.log_media(id);
                }
                self.tick();
            }
            "interrupted" => self.status.set_label("Reconnecting…"),
            _ => self.status.set_label("Call failed"),
        }
    }

    /// For the diagnostics log, every 5 s while call `id` lasts: how its
    /// media moves and how it's doing (counts, rates and delays only).
    fn log_media(&self, id: u64) {
        let Some(node) = self.node() else { return };
        let core = self.core.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                if node.current_call().is_none_or(|c| c.id != id) {
                    return;
                }
                if let Some(s) = node.call_stats() {
                    core.say(format!(
                        "* call media: sent {} datagrams, {} in stream, {} failed; \
                         got {}, {} rejected, {} dropped",
                        s.sent_datagrams,
                        s.sent_stream,
                        s.send_failed,
                        s.received,
                        s.rejected,
                        s.dropped
                    ));
                }
                if let Some(r) = node.call_media_report() {
                    core.say(format!("* call quality: {r}"));
                }
            }
        });
    }

    /// Shows the call's length, every second while it lasts.
    fn tick(self: &Rc<Self>) {
        self.show_time();
        if self.ticking.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_seconds_local(1, move || {
            let Some(this) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !this.busy() {
                this.ticking.set(false);
                return glib::ControlFlow::Break;
            }
            this.show_time();
            glib::ControlFlow::Continue
        });
    }

    fn show_time(&self) {
        let Some(since) = self.shown.borrow().as_ref().and_then(|s| s.since) else {
            return;
        };
        let s = since.elapsed().as_secs();
        let t = if s >= 3600 {
            format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
        } else {
            format!("{}:{:02}", s / 60, s % 60)
        };
        self.status.set_label(&t);
    }

    /// The call is over; returns what to tell the user, if anything.
    pub fn ended(&self, id: u64, reason: &str, by_us: bool) -> Option<String> {
        if !self.is(id) {
            return None;
        }
        let s = self.shown.borrow_mut().take()?;
        s.watching.store(false, Ordering::SeqCst);
        if let Some(d) = s.dialog {
            d.force_close();
        }
        // The camera light goes off with the call.
        self.camera.borrow_mut().take();
        self.remote.set_paintable(gdk::Paintable::NONE);
        self.local.set_paintable(gdk::Paintable::NONE);
        self.peer_video.set(false);
        self.layout();
        self.widget.set_reveal_child(false);
        let what = match (reason, by_us) {
            ("declined", false) => "declined",
            ("busy", _) => "is busy",
            ("unanswered", false) => "didn't answer",
            ("unanswered", true) => "missed call",
            ("failed", _) => "call failed",
            _ if s.since.is_none() && !by_us => "missed call",
            _ => return None,
        };
        Some(format!("{}: {what}", s.title))
    }
}

fn texture(p: Picture) -> gdk::MemoryTexture {
    let stride = p.width as usize * 4;
    gdk::MemoryTexture::new(
        p.width as i32,
        p.height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(p.rgba),
        stride,
    )
}

/// Turns an RGBA picture `rotation` degrees clockwise.
fn upright(width: u32, height: u32, rotation: u32, rgba: Vec<u8>) -> (u32, u32, Vec<u8>) {
    let (w, h) = (width as usize, height as usize);
    if rotation.is_multiple_of(360) || rgba.len() < w * h * 4 {
        return (width, height, rgba);
    }
    let mut out = vec![0u8; w * h * 4];
    let (ow, oh) = if rotation % 180 == 90 { (h, w) } else { (w, h) };
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match rotation % 360 {
                90 => (h - 1 - y, x),
                180 => (w - 1 - x, h - 1 - y),
                _ => (y, w - 1 - x),
            };
            let (src, dst) = ((y * w + x) * 4, (ny * ow + nx) * 4);
            out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
        }
    }
    (ow as u32, oh as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_pictures_upright() {
        // 2×1: red, blue. Turned 90° clockwise: 1×2, red over blue.
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let px = [red, blue].concat();
        assert_eq!(upright(2, 1, 90, px.clone()), (1, 2, [red, blue].concat()));
        assert_eq!(upright(2, 1, 180, px.clone()), (2, 1, [blue, red].concat()));
        assert_eq!(upright(2, 1, 270, px.clone()), (1, 2, [blue, red].concat()));
        assert_eq!(upright(2, 1, 0, px.clone()).2, px);
    }
}
