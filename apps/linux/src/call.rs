//! Voice calls: the bar across the top of the window while a call rings
//! or runs, and the dialog for an incoming one. The audio itself is the
//! node's (WebRTC on the default microphone and speaker); this only shows
//! the call and passes on the user's choices.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;

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
}

pub struct CallBar {
    core: Arc<Core>,
    pub widget: gtk::Revealer,
    who: gtk::Label,
    status: gtk::Label,
    mute: gtk::ToggleButton,
    shown: RefCell<Option<Shown>>,
    /// The one-second timer while a call runs.
    ticking: Cell<bool>,
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

        let mute = gtk::ToggleButton::builder()
            .icon_name("microphone-sensitivity-high-symbolic")
            .tooltip_text("Mute")
            .valign(gtk::Align::Center)
            .build();
        mute.add_css_class("circular");
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
        row.append(&mute);
        row.append(&hangup);
        let widget = gtk::Revealer::builder()
            .child(&row)
            .transition_type(gtk::RevealerTransitionType::SlideDown)
            .reveal_child(false)
            .build();

        let this = Rc::new(Self {
            core,
            widget,
            who,
            status,
            mute,
            shown: RefCell::new(None),
            ticking: Cell::new(false),
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

    fn show(&self, persona: Option<String>, id: u64, title: &str, status: &str) {
        self.who.set_label(title);
        self.status.set_label(status);
        self.mute.set_active(false);
        self.mute.set_visible(false);
        self.widget.set_reveal_child(true);
        *self.shown.borrow_mut() = Some(Shown {
            persona,
            id,
            title: title.to_owned(),
            since: None,
            dialog: None,
        });
    }

    fn is(&self, id: u64) -> bool {
        self.shown.borrow().as_ref().is_some_and(|s| s.id == id)
    }

    /// We called: shown until it's answered or ends.
    pub fn outgoing(&self, persona: Option<String>, id: u64, title: &str) {
        self.show(persona, id, title, "Calling…");
    }

    /// Someone calls: the bar, and a dialog to answer or decline.
    pub fn incoming(
        self: &Rc<Self>,
        persona: Option<String>,
        id: u64,
        title: &str,
        parent: &impl IsA<gtk::Widget>,
    ) {
        self.show(persona, id, title, "Incoming call");
        let d = ui::alert(
            &format!("{title} is calling"),
            "Answer with your microphone and speaker.",
            &[("decline", "Decline"), ("answer", "Answer")],
        );
        d.set_response_appearance("decline", adw::ResponseAppearance::Destructive);
        d.set_response_appearance("answer", adw::ResponseAppearance::Suggested);
        let weak = Rc::downgrade(self);
        d.connect_response(None, move |_, r| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(s) = this.shown.borrow_mut().as_mut() {
                s.dialog = None;
            }
            if r == "answer" {
                this.answer(id);
            } else {
                this.hang_up();
            }
        });
        d.present(Some(parent));
        if let Some(s) = self.shown.borrow_mut().as_mut() {
            s.dialog = Some(d);
        }
    }

    fn answer(self: &Rc<Self>, id: u64) {
        let Some(node) = self.node() else { return };
        self.status.set_label("Connecting…");
        let weak = Rc::downgrade(self);
        bg(
            move || node.answer_call(id, false),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                if let Err(e) = r {
                    this.status.set_label(&format!("Couldn't answer: {e}"));
                } else {
                    this.mute.set_visible(true);
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

    pub fn started(&self, id: u64) {
        if self.is(id) {
            self.status.set_label("Connecting…");
            self.mute.set_visible(true);
        }
    }

    /// The call's audio: "connected", "interrupted" or "failed".
    pub fn media(self: &Rc<Self>, id: u64, state: &str) {
        if !self.is(id) {
            return;
        }
        match state {
            "connected" => {
                if let Some(s) = self.shown.borrow_mut().as_mut() {
                    s.since.get_or_insert_with(std::time::Instant::now);
                }
                self.tick();
            }
            "interrupted" => self.status.set_label("Reconnecting…"),
            _ => self.status.set_label("Call failed"),
        }
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
        if let Some(d) = s.dialog {
            d.force_close();
        }
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
