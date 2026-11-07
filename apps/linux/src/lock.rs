//! App lock (Appendix C, *App lock*) and "Erase all Threnody data" for
//! the desktop: the lock page in front of the window, when it locks
//! (leaving, idling, the screen saver, logind's lock and sleep), the
//! duress PIN, and erasing everything this app keeps.
//!
//! The decisions are in Rust's `threnody-core` (`AppLockClock`, the lock
//! file); this feeds the clock what the window does and shows the page.
//! It is a gate on this window, not encryption: the node keeps running
//! behind it, and anyone who can run programs as the user gets past it.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use threnody_ffi::{AppLockClock, LockKind, LockOutcome, LockSettings};

use crate::keyring;
use crate::ui;
use crate::window::App;

/// Lock-after-leaving choices, in seconds.
const AWAY: &[(&str, u32)] = &[
    ("Immediately", 0),
    ("After 1 minute", 60),
    ("After 5 minutes", 300),
    ("After 15 minutes", 900),
    ("After 1 hour", 3600),
];
/// Lock-when-idle choices, in seconds (None: never).
const IDLE: &[(&str, Option<u32>)] = &[
    ("Never", None),
    ("After 1 minute", Some(60)),
    ("After 5 minutes", Some(300)),
    ("After 15 minutes", Some(900)),
];

const DURESS_HELP: &str = "If someone makes you unlock Threnody, enter this instead of your PIN. \
     Everything on this device is erased without warning, and Threnody opens as if newly \
     installed. Your other devices and your contacts are not told.";

fn now() -> u64 {
    u64::try_from(glib::real_time() / 1000).unwrap_or(0)
}

/// The pepper for the lock file of `home`: its own keyring entry, which
/// exists only for keyring-sealed homes (Appendix C, *App lock*).
fn pepper(home: &Path) -> Option<String> {
    keyring::lock_pepper(home, false).map(|s| s.0.clone())
}

/// The pepper to turn app lock on with: made now for a keyring-sealed
/// home, none for a passphrase-sealed or unsealed one.
fn new_pepper(home: &Path) -> Option<String> {
    keyring::get(home)?;
    keyring::lock_pepper(home, true).map(|s| s.0.clone())
}

fn noun(kind: Option<LockKind>) -> &'static str {
    match kind {
        Some(LockKind::Passphrase) => "passphrase",
        _ => "PIN",
    }
}

pub struct Lock {
    home: PathBuf,
    clock: Arc<AppLockClock>,
    settings: RefCell<LockSettings>,
    /// The lock file is there but can't be read: stay locked, offer erasing.
    unreadable: Cell<bool>,
    pub stack: gtk::Stack,
    entry: gtk::PasswordEntry,
    status: gtk::Label,
    unlock: gtk::Button,
    erase: gtk::Button,
    /// Other windows hidden while locked, shown again on unlock.
    hidden: RefCell<Vec<gtk::Window>>,
    /// Window actions disabled while locked, enabled again on unlock.
    disabled: RefCell<Vec<gio::SimpleAction>>,
    /// A chat a notification asked to open while locked.
    pub queued: RefCell<Option<String>>,
    last_activity: Cell<u64>,
    busy: Cell<bool>,
    app: RefCell<Weak<App>>,
    subs: RefCell<Vec<gio::SignalSubscription>>,
}

impl Lock {
    /// Puts the lock page and `content` (the app) in one stack as the
    /// window's content, the lock page in front if app lock is on, so no
    /// frame of the app is drawn first.
    pub fn new(
        home: &Path,
        window: &adw::ApplicationWindow,
        content: &impl IsA<gtk::Widget>,
    ) -> Rc<Self> {
        let on = threnody_ffi::app_lock_enabled(home.display().to_string());
        // Always locked at start while on, even after the identity's
        // passphrase: a coerced user must have somewhere to type the
        // duress PIN.
        let clock = AppLockClock::new(on, false, 0, Some(300), now());
        let entry = gtk::PasswordEntry::builder()
            .placeholder_text("PIN")
            .show_peek_icon(false)
            .activates_default(true)
            .width_request(260)
            .build();
        let status = ui::label("", &["dim-label"]);
        status.set_xalign(0.5);
        let unlock = gtk::Button::with_label("Unlock");
        unlock.add_css_class("pill");
        unlock.add_css_class("suggested-action");
        let erase = gtk::Button::with_label("Erase all Threnody data…");
        erase.add_css_class("pill");
        erase.add_css_class("destructive-action");
        erase.set_visible(false);
        let column = gtk::Box::new(gtk::Orientation::Vertical, 12);
        column.set_halign(gtk::Align::Center);
        column.append(&entry);
        column.append(&status);
        column.append(&unlock);
        column.append(&erase);
        let page = adw::StatusPage::builder()
            .icon_name(crate::APP_ID)
            .title("Threnody")
            .child(&column)
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        let stack = gtk::Stack::new();
        stack.add_named(content, Some("app"));
        stack.add_named(&view, Some("lock"));
        stack.set_visible_child_name(if on { "lock" } else { "app" });
        window.set_content(Some(&stack));
        Rc::new(Self {
            home: home.to_path_buf(),
            clock,
            settings: RefCell::new(off()),
            unreadable: Cell::new(false),
            stack,
            entry,
            status,
            unlock,
            erase,
            hidden: RefCell::new(Vec::new()),
            disabled: RefCell::new(Vec::new()),
            queued: RefCell::new(None),
            last_activity: Cell::new(0),
            busy: Cell::new(false),
            app: RefCell::new(Weak::new()),
            subs: RefCell::new(Vec::new()),
        })
    }

    /// Whether app lock is on (its file exists). Cheap; fails closed.
    pub fn enabled(&self) -> bool {
        threnody_ffi::app_lock_enabled(self.home.display().to_string())
    }

    pub fn is_locked(&self) -> bool {
        self.clock.is_locked()
    }

    fn app(&self) -> Option<Rc<App>> {
        self.app.borrow().upgrade()
    }

    /// Reads the settings (the keyring and Argon2id work off the main
    /// thread), then applies their times and shows what unlocks.
    fn reload(self: &Rc<Self>) {
        let home = self.home.clone();
        let weak = Rc::downgrade(self);
        ui::bg(
            move || threnody_ffi::app_lock(home.display().to_string(), pepper(&home)),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                match r {
                    Ok(s) => this.apply(s),
                    Err(e) => {
                        this.unreadable.set(this.enabled());
                        if this.unreadable.get() {
                            this.status
                                .set_text(&format!("App lock settings can't be read ({e})."));
                            this.erase.set_visible(true);
                            this.entry.set_visible(false);
                            this.unlock.set_visible(false);
                        }
                    }
                }
            },
        );
    }

    fn apply(self: &Rc<Self>, s: LockSettings) {
        self.unreadable.set(false);
        self.clock.set_policy(s.biometric, s.away_secs, s.idle_secs);
        // Turned on or changed in front: the idle time counts from now.
        if !self.clock.is_locked() {
            self.clock.unlocked(now());
        }
        let kind = s.kind;
        self.entry
            .set_placeholder_text(Some(if kind == Some(LockKind::Passphrase) {
                "Passphrase"
            } else {
                "PIN"
            }));
        self.entry.set_visible(true);
        self.unlock.set_visible(true);
        self.erase.set_visible(false);
        self.wait_until(s.retry_at_ms);
        *self.settings.borrow_mut() = s;
    }

    /// Refuses entries until `retry_at_ms` (wall clock), with a countdown.
    fn wait_until(self: &Rc<Self>, retry_at_ms: u64) {
        let left = retry_at_ms.saturating_sub(threnody_core::now_ms());
        if left == 0 {
            self.entry.set_sensitive(true);
            self.unlock.set_sensitive(true);
            return;
        }
        self.entry.set_sensitive(false);
        self.unlock.set_sensitive(false);
        self.status
            .set_text(&format!("Try again in {} s", left.div_ceil(1000)));
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(left.min(1000)),
            move || {
                if let Some(this) = weak.upgrade() {
                    this.wait_until(retry_at_ms);
                }
            },
        );
    }

    /// Shows the lock page: dialogs close, other windows hide, and the
    /// window's actions (and their shortcuts) stop working until unlock.
    pub fn show_lock(&self) {
        let Some(app) = self.app() else { return };
        self.stack.set_visible_child_name("lock");
        for _ in 0..16 {
            let Some(d) = app.window.visible_dialog() else {
                break;
            };
            d.force_close();
        }
        let main: gtk::Window = app.window.clone().upcast();
        for w in app.app.windows() {
            if w != main && w.is_visible() {
                w.set_visible(false);
                self.hidden.borrow_mut().push(w);
            }
        }
        for name in app.window.list_actions() {
            if let Some(a) = app
                .window
                .lookup_action(&name)
                .and_then(|a| a.downcast::<gio::SimpleAction>().ok())
                && a.is_enabled()
            {
                a.set_enabled(false);
                self.disabled.borrow_mut().push(a);
            }
        }
        self.entry.set_text("");
        if self.enabled() && !self.unreadable.get() {
            self.entry.grab_focus();
        }
    }

    /// Back to the app: what was held while locked goes on.
    fn open_up(&self) {
        let Some(app) = self.app() else { return };
        self.clock.unlocked(now());
        self.stack.set_visible_child_name("app");
        for a in self.disabled.take() {
            a.set_enabled(true);
        }
        for w in self.hidden.take() {
            w.set_visible(true);
        }
        self.status.set_text("");
        app.app.withdraw_notification("message");
        if let Some(v) = self.queued.take() {
            app.open_variant(&v);
        }
        crate::handle_links(&app);
    }

    /// Locks now ("Lock now", the screen saver, logind).
    pub fn lock_now(&self) {
        if !self.enabled() {
            return;
        }
        self.clock.lock_now();
        self.show_lock();
    }

    fn activity(&self) {
        let t = now();
        if t.saturating_sub(self.last_activity.get()) >= 1000 {
            self.last_activity.set(t);
            self.clock.activity(t);
        }
    }

    fn submit(self: &Rc<Self>) {
        let text = self.entry.text().to_string();
        if self.busy.get() || text.is_empty() || !self.entry.is_sensitive() {
            return;
        }
        self.busy.set(true);
        // The same for every entry: the duress PIN looks like the right one.
        self.status.set_text("Unlocking…");
        self.entry.remove_css_class("error");
        let home = self.home.clone();
        let weak = Rc::downgrade(self);
        ui::bg(
            move || threnody_ffi::unlock_app(home.display().to_string(), pepper(&home), text),
            move |r| {
                let Some(this) = weak.upgrade() else { return };
                this.busy.set(false);
                this.entry.set_text("");
                match r {
                    Ok(LockOutcome::Unlocked) => this.open_up(),
                    Ok(LockOutcome::Wrong { retry_at_ms }) => {
                        this.entry.add_css_class("error");
                        this.status.set_text("Wrong PIN");
                        this.wait_until(retry_at_ms);
                    }
                    Ok(LockOutcome::Wait { retry_at_ms }) => this.wait_until(retry_at_ms),
                    Ok(LockOutcome::Erased { homes }) => {
                        if let Some(app) = this.app() {
                            finish_erase(&app, &homes);
                        }
                    }
                    Err(e) => this.status.set_text(&e.to_string()),
                }
            },
        );
    }
}

fn off() -> LockSettings {
    LockSettings {
        enabled: false,
        kind: None,
        biometric: false,
        away_secs: 0,
        idle_secs: Some(300),
        duress: false,
        erase_after_failures: false,
        retry_at_ms: 0,
    }
}

/// Wires the lock to the window once the app exists: unlocking, input,
/// leaving, the idle timer, the screen saver and logind.
pub fn install(app: &Rc<App>) {
    let lock = app.lock.clone();
    *lock.app.borrow_mut() = Rc::downgrade(app);
    if lock.enabled() {
        lock.show_lock();
        lock.reload();
    }

    // A dialog that opens while locked (a prompt an event brought, say)
    // closes at once: only the lock page's own may show.
    let weak = Rc::downgrade(&lock);
    app.window.connect_visible_dialog_notify(move |w| {
        let Some(l) = weak.upgrade() else { return };
        if l.is_locked()
            && let Some(d) = w.visible_dialog()
            && d.widget_name() != LOCK_DIALOG
        {
            d.force_close();
        }
    });

    let weak = Rc::downgrade(&lock);
    lock.entry.connect_activate(move |_| {
        if let Some(l) = weak.upgrade() {
            l.submit();
        }
    });
    let weak = Rc::downgrade(&lock);
    lock.unlock.connect_clicked(move |_| {
        if let Some(l) = weak.upgrade() {
            l.submit();
        }
    });
    let weak = Rc::downgrade(app);
    lock.erase.connect_clicked(move |_| {
        if let Some(a) = weak.upgrade() {
            confirm_erase(&a);
        }
    });

    // Input in front postpones the idle lock.
    let window = &app.window;
    let key = gtk::EventControllerKey::new();
    let click = gtk::GestureClick::new();
    let motion = gtk::EventControllerMotion::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    motion.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&lock);
    key.connect_key_pressed(move |_, _, _, _| {
        if let Some(l) = weak.upgrade() {
            l.activity();
        }
        glib::Propagation::Proceed
    });
    let weak = Rc::downgrade(&lock);
    click.connect_pressed(move |_, _, _, _| {
        if let Some(l) = weak.upgrade() {
            l.activity();
        }
    });
    let weak = Rc::downgrade(&lock);
    motion.connect_motion(move |_, _, _| {
        if let Some(l) = weak.upgrade() {
            l.activity();
        }
    });
    window.add_controller(key);
    window.add_controller(click);
    window.add_controller(motion);

    // Leaving: hidden (closed to the background) or minimized.
    let left = {
        let weak = Rc::downgrade(&lock);
        move |w: &adw::ApplicationWindow| {
            let Some(l) = weak.upgrade() else { return };
            if !l.enabled() {
                return;
            }
            if !w.is_visible() || w.is_suspended() {
                l.clock.left(now());
                if l.clock.is_locked() {
                    l.show_lock();
                }
            } else if l.clock.returned(now()) {
                l.show_lock();
            }
        }
    };
    let l2 = left.clone();
    window.connect_visible_notify(move |w| l2(w));
    window.connect_suspended_notify(move |w| left(w));

    let weak = Rc::downgrade(&lock);
    glib::timeout_add_seconds_local(15, move || {
        let Some(l) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if l.enabled() && !l.is_locked() && l.clock.tick(now()) {
            l.show_lock();
        }
        glib::ControlFlow::Continue
    });

    watch_session(&lock);
}

/// Locks when the screen saver starts (GNOME, KDE), when logind asks the
/// session to lock (`loginctl lock-session`, hypridle, swayidle) and
/// before sleep. All optional: a missing service is ignored.
fn watch_session(lock: &Rc<Lock>) {
    let weak = Rc::downgrade(lock);
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |c| {
        let (Ok(conn), Some(l)) = (c, weak.upgrade()) else {
            return;
        };
        for iface in ["org.freedesktop.ScreenSaver", "org.gnome.ScreenSaver"] {
            let weak = Rc::downgrade(&l);
            let sub = conn.subscribe_to_signal(
                None,
                Some(iface),
                Some("ActiveChanged"),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |s| {
                    if s.parameters.get::<(bool,)>().is_some_and(|(on,)| on)
                        && let Some(l) = weak.upgrade()
                    {
                        l.lock_now();
                    }
                },
            );
            l.subs.borrow_mut().push(sub);
        }
    });
    let weak = Rc::downgrade(lock);
    gio::bus_get(gio::BusType::System, gio::Cancellable::NONE, move |c| {
        let (Ok(conn), Some(l)) = (c, weak.upgrade()) else {
            return;
        };
        const LOGIN1: &str = "org.freedesktop.login1";
        let w = Rc::downgrade(&l);
        let sleep = conn.subscribe_to_signal(
            Some(LOGIN1),
            Some("org.freedesktop.login1.Manager"),
            Some("PrepareForSleep"),
            Some("/org/freedesktop/login1"),
            None,
            gio::DBusSignalFlags::NONE,
            move |s| {
                if s.parameters.get::<(bool,)>().is_some_and(|(on,)| on)
                    && let Some(l) = w.upgrade()
                {
                    l.lock_now();
                }
            },
        );
        l.subs.borrow_mut().push(sleep);
        let weak = Rc::downgrade(&l);
        let c2 = conn.clone();
        conn.call(
            Some(LOGIN1),
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "GetSessionByPID",
            Some(&(std::process::id(),).to_variant()),
            Some(glib::VariantTy::new("(o)").expect("type")),
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            move |r| {
                let (Ok(v), Some(l)) = (r, weak.upgrade()) else {
                    return;
                };
                let Some(path) = v.child_value(0).str().map(str::to_owned) else {
                    return;
                };
                let w = Rc::downgrade(&l);
                let sub = c2.subscribe_to_signal(
                    Some(LOGIN1),
                    Some("org.freedesktop.login1.Session"),
                    Some("Lock"),
                    Some(&path),
                    None,
                    gio::DBusSignalFlags::NONE,
                    move |_| {
                        if let Some(l) = w.upgrade() {
                            l.lock_now();
                        }
                    },
                );
                l.subs.borrow_mut().push(sub);
            },
        );
    });
}

// ----- Preferences -----

// An activatable row: libadwaita 1.5 has no ButtonRow.
fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    row
}

fn secret_entry(placeholder: &str) -> gtk::PasswordEntry {
    gtk::PasswordEntry::builder()
        .placeholder_text(placeholder)
        .show_peek_icon(true)
        .activates_default(true)
        .build()
}

/// Asks for the current secret.
fn ask_current(app: &Rc<App>, kind: Option<LockKind>, title: &str, run: impl Fn(String) + 'static) {
    let d = ui::alert(title, "", &[("cancel", "Cancel"), ("ok", "Continue")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let e = secret_entry(&format!("Current {}", noun(kind)));
    d.set_extra_child(Some(&e));
    d.connect_response(None, move |_, r| {
        if r == "ok" {
            run(e.text().to_string());
        }
    });
    d.present(Some(&app.window));
}

/// Asks for a new secret of `kind`, twice.
fn ask_new(app: &Rc<App>, kind: LockKind, title: &str, body: &str, run: impl Fn(String) + 'static) {
    let d = ui::alert(title, body, &[("cancel", "Cancel"), ("ok", "Save")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let first = secret_entry(if kind == LockKind::Passphrase {
        "At least 6 characters"
    } else {
        "4 to 16 digits"
    });
    let second = secret_entry("Again");
    b.append(&first);
    b.append(&second);
    d.set_extra_child(Some(&b));
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r != "ok" {
            return;
        }
        let s = first.text().to_string();
        if !threnody_ffi::lock_secret_ok(kind, s.clone()) {
            a.toast(if kind == LockKind::Passphrase {
                "A passphrase needs at least 6 characters"
            } else {
                "A PIN is 4 to 16 digits"
            });
        } else if s != second.text().as_str() {
            a.toast("They differ");
        } else {
            run(s);
        }
    });
    d.present(Some(&app.window));
}

/// Runs a change to the lock file off the main thread, then rereads the
/// settings, refreshes `group` and toasts `done`.
fn change(
    app: &Rc<App>,
    rebuild: &Rc<dyn Fn()>,
    done: &str,
    new: bool,
    work: impl FnOnce(String, Option<String>) -> Result<(), threnody_ffi::ThrenodyError>
    + Send
    + 'static,
) {
    let home = app.core.home.clone();
    let (a, rebuild, done) = (app.clone(), rebuild.clone(), done.to_owned());
    ui::bg(
        move || {
            let p = if new {
                new_pepper(&home)
            } else {
                pepper(&home)
            };
            let dir = home.display().to_string();
            work(dir.clone(), p.clone())?;
            threnody_ffi::app_lock(dir, p)
        },
        move |r| match r {
            Ok(s) => {
                a.lock.apply(s);
                rebuild();
                if !done.is_empty() {
                    a.toast(&done);
                }
            }
            Err(e) => a.toast(&e.to_string()),
        },
    );
}

/// Redraws the App Lock group after a change.
type Rebuild = Rc<dyn Fn()>;

/// The "App lock" group in Preferences.
pub fn group(app: &Rc<App>) -> adw::PreferencesGroup {
    let peppered = keyring::get(&app.core.home).is_some();
    let mut about = String::from(
        "Locks this window behind a PIN or passphrase. Messages still arrive while it is \
         locked, and notifications say only \u{201c}New message\u{201d}. App lock keeps people at \
         your screen out of this window. It doesn't stop someone who can run programs as you.",
    );
    if !peppered {
        about.push_str(
            " Your identity isn't kept in the keyring, so a short PIN could be guessed from a \
             copy of your files: choose a passphrase.",
        );
    }
    let group = adw::PreferencesGroup::builder()
        .title("App Lock")
        .description(about)
        .build();
    // Rebuilt after each change. Only weak references here, and the
    // group's own destroy handler holds the closure: no cycles.
    let rows: Rc<RefCell<Vec<glib::WeakRef<gtk::Widget>>>> = Rc::new(RefCell::new(Vec::new()));
    let slot: Rc<RefCell<Option<Rebuild>>> = Rc::new(RefCell::new(None));
    let (gw, aw, r, sw) = (
        group.downgrade(),
        Rc::downgrade(app),
        rows,
        Rc::downgrade(&slot),
    );
    let rebuild: Rc<dyn Fn()> = Rc::new(move || {
        let (Some(g), Some(app), Some(slot)) = (gw.upgrade(), aw.upgrade(), sw.upgrade()) else {
            return;
        };
        for w in r.take() {
            if let Some(w) = w.upgrade() {
                g.remove(&w);
            }
        }
        let Some(rebuild) = slot.borrow().clone() else {
            return;
        };
        fill(&app, &rebuild, |w: gtk::Widget| {
            g.add(&w);
            r.borrow_mut().push(w.downgrade());
        });
    });
    *slot.borrow_mut() = Some(rebuild.clone());
    rebuild();
    group.connect_destroy(move |_| {
        slot.take();
    });
    group
}

fn fill(app: &Rc<App>, rebuild: &Rc<dyn Fn()>, add: impl Fn(gtk::Widget)) {
    let s = app.lock.settings.borrow().clone();
    if !app.lock.enabled() {
        for kind in [LockKind::Digits, LockKind::Passphrase] {
            let r = row(
                &format!(
                    "Lock with an App {}…",
                    if kind == LockKind::Digits {
                        "PIN"
                    } else {
                        "Passphrase"
                    }
                ),
                "Threnody asks for it whenever it locks",
            );
            let (a, rb) = (app.clone(), rebuild.clone());
            r.connect_activated(move |_| {
                let (a2, rb2) = (a.clone(), rb.clone());
                ask_new(
                    &a,
                    kind,
                    &format!("App {}", noun(Some(kind))),
                    "",
                    move |secret| {
                        change(&a2, &rb2, "App lock on", true, move |h, p| {
                            threnody_ffi::set_app_lock(
                                h,
                                p,
                                None,
                                Some(secret),
                                Some(kind),
                                false,
                                0,
                                Some(300),
                                false,
                            )
                        });
                    },
                );
            });
            add(r.upcast());
        }
        return;
    }
    let kind = s.kind;
    let away = adw::ComboRow::builder()
        .title("Lock after leaving")
        .subtitle("Hidden or minimized")
        .model(&gtk::StringList::new(
            &AWAY.iter().map(|a| a.0).collect::<Vec<_>>(),
        ))
        .selected(AWAY.iter().position(|a| a.1 == s.away_secs).unwrap_or(0) as u32)
        .build();
    let (a, rb, s2) = (app.clone(), rebuild.clone(), s.clone());
    away.connect_selected_notify(move |c| {
        let secs = AWAY[c.selected() as usize % AWAY.len()].1;
        if secs == s2.away_secs {
            return;
        }
        let s = s2.clone();
        change(&a, &rb, "", false, move |h, p| {
            threnody_ffi::set_lock_policy(
                h,
                p,
                s.biometric,
                secs,
                s.idle_secs,
                s.erase_after_failures,
            )
        });
    });
    add(away.upcast());
    let idle = adw::ComboRow::builder()
        .title("Lock when idle")
        .model(&gtk::StringList::new(
            &IDLE.iter().map(|a| a.0).collect::<Vec<_>>(),
        ))
        .selected(IDLE.iter().position(|a| a.1 == s.idle_secs).unwrap_or(0) as u32)
        .build();
    let (a, rb, s2) = (app.clone(), rebuild.clone(), s.clone());
    idle.connect_selected_notify(move |c| {
        let secs = IDLE[c.selected() as usize % IDLE.len()].1;
        if secs == s2.idle_secs {
            return;
        }
        let s = s2.clone();
        change(&a, &rb, "", false, move |h, p| {
            threnody_ffi::set_lock_policy(
                h,
                p,
                s.biometric,
                s.away_secs,
                secs,
                s.erase_after_failures,
            )
        });
    });
    add(idle.upcast());

    if let Some(k) = kind {
        let r = row(&format!("Change {}…", noun(kind)), "");
        let (a, rb) = (app.clone(), rebuild.clone());
        r.connect_activated(move |_| {
            let (a2, rb2) = (a.clone(), rb.clone());
            ask_current(
                &a,
                Some(k),
                &format!("Change {}", noun(Some(k))),
                move |current| {
                    let (a3, rb3) = (a2.clone(), rb2.clone());
                    ask_new(
                        &a2,
                        k,
                        &format!("New {}", noun(Some(k))),
                        "",
                        move |secret| {
                            let s = a3.lock.settings.borrow().clone();
                            let current = current.clone();
                            change(&a3, &rb3, "Changed", false, move |h, p| {
                                threnody_ffi::set_app_lock(
                                    h,
                                    p,
                                    Some(current),
                                    Some(secret),
                                    Some(k),
                                    s.biometric,
                                    s.away_secs,
                                    s.idle_secs,
                                    s.erase_after_failures,
                                )
                            });
                        },
                    );
                },
            );
        });
        add(r.upcast());

        let r = row(
            &format!("Duress {}…", noun(kind)),
            if s.duress { "Set" } else { "Not set" },
        );
        let (a, rb) = (app.clone(), rebuild.clone());
        let set = s.duress;
        r.connect_activated(move |_| duress(&a, &rb, k, set));
        add(r.upcast());

        let erase_after = adw::SwitchRow::builder()
            .title(format!("Erase after 10 wrong {}s", noun(kind)))
            .subtitle("Everything here is erased after 10 wrong entries in a row")
            .active(s.erase_after_failures)
            .build();
        let (a, rb, s2) = (app.clone(), rebuild.clone(), s.clone());
        erase_after.connect_active_notify(move |sw| {
            let on = sw.is_active();
            if on == s2.erase_after_failures {
                return;
            }
            let s = s2.clone();
            change(&a, &rb, "", false, move |h, p| {
                threnody_ffi::set_lock_policy(h, p, s.biometric, s.away_secs, s.idle_secs, on)
            });
        });
        add(erase_after.upcast());
    }

    let now = row("Lock Now", "");
    let weak = Rc::downgrade(app);
    now.connect_activated(move |_| {
        if let Some(a) = weak.upgrade() {
            a.lock.lock_now();
        }
    });
    add(now.upcast());

    let off_row = row("Turn App Lock Off…", "");
    let (a, rb) = (app.clone(), rebuild.clone());
    off_row.connect_activated(move |_| {
        let (a2, rb2) = (a.clone(), rb.clone());
        ask_current(&a, kind, "Turn app lock off", move |current| {
            change(&a2, &rb2, "App lock off", false, move |h, p| {
                threnody_ffi::clear_app_lock(h, p, Some(current))
            });
        });
    });
    add(off_row.upcast());
}

fn duress(app: &Rc<App>, rebuild: &Rc<dyn Fn()>, kind: LockKind, set: bool) {
    let (a, rb) = (app.clone(), rebuild.clone());
    ask_current(
        app,
        Some(kind),
        &format!("Duress {}", noun(Some(kind))),
        move |current| {
            let new = {
                let (a, rb, current) = (a.clone(), rb.clone(), current.clone());
                move || {
                    let (a2, rb2, current) = (a.clone(), rb.clone(), current.clone());
                    ask_new(
                        &a,
                        kind,
                        &format!("Duress {}", noun(Some(kind))),
                        DURESS_HELP,
                        move |d| {
                            let current = current.clone();
                            change(&a2, &rb2, "Duress PIN set", false, move |h, p| {
                                threnody_ffi::set_duress_pin(h, p, current, Some(d))
                            });
                        },
                    );
                }
            };
            if !set {
                return new();
            }
            let d = ui::alert(
                &format!("Duress {}", noun(Some(kind))),
                DURESS_HELP,
                &[
                    ("cancel", "Cancel"),
                    ("remove", "Remove"),
                    ("change", "Change"),
                ],
            );
            d.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
            let (a2, rb2) = (a.clone(), rb.clone());
            let current = current.clone();
            d.connect_response(None, move |_, r| match r {
                "change" => new(),
                "remove" => {
                    let current = current.clone();
                    change(&a2, &rb2, "Duress PIN removed", false, move |h, p| {
                        threnody_ffi::set_duress_pin(h, p, current, None)
                    });
                }
                _ => {}
            });
            d.present(Some(&a.window));
        },
    );
}

/// The "Danger zone" group in Preferences.
pub fn danger_group(app: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Danger Zone")
        .build();
    let r = row(
        "Erase All Threnody Data…",
        "Your identity, contacts, history and files, from this computer",
    );
    let weak = Rc::downgrade(app);
    r.connect_activated(move |_| {
        if let Some(a) = weak.upgrade() {
            confirm_erase(&a);
        }
    });
    group.add(&r);
    group
}

/// The name of the dialogs the lock page itself shows (erasing), which
/// may show while locked.
const LOCK_DIALOG: &str = "lock-dialog";

/// The typed confirmation, then the erase.
pub fn confirm_erase(app: &Rc<App>) {
    let d = ui::alert(
        "Erase all Threnody data?",
        "This erases your identity, contacts, history, anonymous identities and every file \
         Threnody kept, including the Downloads/Threnody folder, then starts Threnody afresh. \
         Your linked devices and your contacts are not told. It can't be undone.",
        &[("cancel", "Cancel"), ("erase", "Erase")],
    );
    d.set_widget_name(LOCK_DIALOG);
    d.set_response_appearance("erase", adw::ResponseAppearance::Destructive);
    d.set_default_response(Some("cancel"));
    d.set_response_enabled("erase", false);
    let e = gtk::Entry::builder().placeholder_text("Type erase").build();
    let dd = d.clone();
    e.connect_changed(move |e| dd.set_response_enabled("erase", e.text().trim() == "erase"));
    d.set_extra_child(Some(&e));
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r == "erase" {
            erase_all(&a);
        }
    });
    let parent: gtk::Widget = if app.lock.is_locked() {
        app.lock.stack.clone().upcast()
    } else {
        app.window.clone().upcast()
    };
    d.present(Some(&parent));
}

/// Erases everything: the node's data directory (with its nodes shut
/// down), then what the app keeps elsewhere, then starts afresh.
pub fn erase_all(app: &Rc<App>) {
    erase(app, None);
}

/// After the duress PIN (or the last wrong entry) erased the data
/// directory: the nodes were still running meanwhile, so stop them and
/// erase again whatever they wrote (a received photo, say).
fn finish_erase(app: &Rc<App>, homes: &[String]) {
    erase(app, Some(homes.to_vec()));
}

/// Stops every node (nothing saves files from then on), erases the data
/// directory, then what the app keeps elsewhere, then starts afresh.
/// `erased` is the identity homes an earlier erase already removed. The
/// erase is idempotent: when it fails, it can simply be run again.
fn erase(app: &Rc<App>, erased: Option<Vec<String>>) {
    crate::core::WIPING.store(true, std::sync::atomic::Ordering::SeqCst);
    let core = app.core.clone();
    let home = app.core.home.display().to_string();
    let a = app.clone();
    let before = erased.clone();
    ui::bg(
        move || {
            core.shutdown_for_wipe();
            threnody_ffi::wipe_home(home).map(|now| before.unwrap_or(now))
        },
        move |r| match r {
            Ok(homes) => {
                wipe_platform(&a, &homes);
                crate::restart(&a.app);
            }
            Err(e) => erase_failed(&a, &e.to_string(), erased),
        },
    );
}

/// The nodes are stopped and the erase didn't finish: Threnody can't go
/// on as it was. Try again, or restart (the start-up flow takes over).
fn erase_failed(app: &Rc<App>, why: &str, erased: Option<Vec<String>>) {
    let d = ui::alert(
        "Couldn't erase everything",
        &format!(
            "{why}\n\nThrenody has stopped. Try again, or restart Threnody and erase it from \
             there."
        ),
        &[("restart", "Restart"), ("retry", "Try Again")],
    );
    d.set_widget_name(LOCK_DIALOG);
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r == "retry" {
            erase(&a, erased.clone());
        } else {
            crate::restart(&a.app);
        }
    });
    let parent: gtk::Widget = if app.lock.is_locked() {
        app.lock.stack.clone().upcast()
    } else {
        app.window.clone().upcast()
    };
    d.present(Some(&parent));
}

/// What the files to remove besides the data directory are: the
/// Downloads/Threnody folder, when there is an XDG Downloads directory
/// (without one, received files went into the data directory).
pub fn erase_targets(downloads: Option<&Path>) -> Vec<PathBuf> {
    downloads.map(|d| d.join("Threnody")).into_iter().collect()
}

fn wipe_platform(app: &Rc<App>, homes: &[String]) {
    for h in homes {
        keyring::delete(Path::new(h));
        keyring::delete_lock_pepper(Path::new(h));
    }
    app.core.settings().erase();
    crate::autostart::remove();
    let downloads = glib::user_special_dir(glib::UserDirectory::Downloads);
    for p in erase_targets(downloads.as_deref()) {
        let _ = std::fs::remove_dir_all(p);
    }
    for c in app.conversation_ids() {
        // A message's, and a scheduled message's that wasn't sent.
        app.app.withdraw_notification(&format!("scheduled-{c}"));
        app.app.withdraw_notification(&c);
    }
    app.app.withdraw_notification("message");
    app.app.withdraw_notification("call");
}

/// Once per identity (and once on installs from before app lock): offers
/// to lock with a PIN or passphrase, "Set up" being the default. The app
/// can't turn the lock on by itself: it has no secret to use.
pub fn offer(app: &Rc<App>) {
    {
        let mut s = app.core.settings();
        if s.opted_in(crate::settings::LOCK_OFFERED) {
            return;
        }
        s.set_flag(crate::settings::LOCK_OFFERED, true);
    }
    if app.lock.enabled() {
        return;
    }
    let d = ui::alert(
        "Lock Threnody?",
        "Lock Threnody with a PIN or passphrase, so that people at your computer can't read \
         your messages. It locks when you leave it, and when your screen locks.",
        &[("later", "Not now"), ("setup", "Set Up")],
    );
    d.set_response_appearance("setup", adw::ResponseAppearance::Suggested);
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r != "setup" {
            return;
        }
        let a2 = a.clone();
        let rb: Rc<dyn Fn()> = Rc::new(|| {});
        ask_new(
            &a,
            LockKind::Digits,
            "App PIN",
            "Threnody asks for it whenever it locks.",
            move |secret| {
                change(&a2, &rb, "App lock on", true, move |h, p| {
                    threnody_ffi::set_app_lock(
                        h,
                        p,
                        None,
                        Some(secret),
                        Some(LockKind::Digits),
                        false,
                        0,
                        Some(300),
                        false,
                    )
                });
            },
        );
    });
    d.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erase_targets_lists_the_downloads_folder() {
        assert_eq!(
            erase_targets(Some(Path::new("/home/u/Downloads"))),
            vec![PathBuf::from("/home/u/Downloads/Threnody")]
        );
        assert!(erase_targets(None).is_empty());
    }
}
