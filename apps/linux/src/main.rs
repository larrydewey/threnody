//! Threnody for the Linux desktop: a GTK 4 / libadwaita messenger on
//! `threnody-ffi`, the API the Android app uses. It shares the CLI's data
//! directory and keyring entry, so an identity made with `threnody init`
//! opens here as it is.

mod autostart;
mod backup;
mod call;
mod camera;
mod chat;
mod clip;
mod core;
mod editor;
mod folders;
mod gifs;
mod keyring;
mod links;
mod lists;
mod lock;
#[cfg(feature = "nfc")]
mod nfc;
mod scan;
mod schedule;
mod settings;
mod sketch;
mod ui;
mod viewer;
mod window;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::core::{Core, OpenError, Protect};
use crate::window::App;

pub const APP_ID: &str = "org.threnody.Threnody";

/// Set after erasing everything: start afresh once the app has quit.
static RESTART: AtomicBool = AtomicBool::new(false);

/// Quits and starts a new process, which finds no identity and begins as
/// on a first run (after "Erase all Threnody data" or the duress PIN).
pub fn restart(app: &adw::Application) {
    RESTART.store(true, Ordering::SeqCst);
    app.quit();
}

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
    /// Links opened before the window was ready.
    static PENDING: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Started at login ([`autostart::FLAG`]): the window stays hidden unless
/// something needs the user.
static START_HIDDEN: AtomicBool = AtomicBool::new(false);

fn main() -> glib::ExitCode {
    // Our own flag, kept from GApplication, which would take it for a file.
    START_HIDDEN.store(
        std::env::args_os().skip(1).any(|a| a == autostart::FLAG),
        Ordering::SeqCst,
    );
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN | gio::ApplicationFlags::CAN_OVERRIDE_APP_ID)
        .build();
    app.connect_startup(|app| {
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        // The icon, when running from the source tree.
        if let Some(display) = gtk::gdk::Display::default() {
            let theme = gtk::IconTheme::for_display(&display);
            theme.add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));
        }
        gtk::Window::set_default_icon_name(APP_ID);
        app_actions(app);
    });
    app.connect_activate(activate);
    app.connect_open(|app, files, _| {
        for f in files {
            PENDING.with_borrow_mut(|p| p.push(f.uri().to_string()));
        }
        activate(app);
    });
    app.connect_shutdown(|_| {
        if let Some(a) = APP.with_borrow_mut(Option::take) {
            a.core.shutdown();
        }
    });
    let code = if START_HIDDEN.load(Ordering::SeqCst) {
        // Started at login, with nothing else to open.
        app.run_with_args(&[std::env::args().next().unwrap_or_default()])
    } else {
        app.run()
    };
    if !RESTART.load(Ordering::SeqCst) {
        return code;
    }
    // GApplication gives up its bus name when finalized: let it go before
    // the new process asks for it. Files and sockets close at exec.
    drop(app);
    if let Ok(exe) = std::env::current_exe() {
        use std::os::unix::process::CommandExt;
        // Links and files given at start were handled (or erased) already.
        let args = std::env::args_os()
            .skip(1)
            .filter(|a| a.to_str().is_some_and(|a| a.starts_with('-')) && a != autostart::FLAG);
        let e = std::process::Command::new(exe).args(args).exec();
        eprintln!("threnody-desktop: restarting: {e}");
    }
    code
}

fn app_actions(app: &adw::Application) {
    let quit = gio::SimpleAction::new("quit", None);
    let a = app.clone();
    // Asks first if scheduled messages would then not be sent.
    quit.connect_activate(move |_, _| match APP.with_borrow(Clone::clone) {
        Some(app) => app.confirm_quit(),
        None => a.quit(),
    });
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Ctrl>q"]);
    app.set_accels_for_action("window.close", &["<Ctrl>w"]);

    let open = gio::SimpleAction::new("open-chat", Some(glib::VariantTy::STRING));
    open.connect_activate(|_, v| {
        let Some(s) = v.and_then(glib::Variant::str) else {
            return;
        };
        if let Some(a) = APP.with_borrow(Clone::clone) {
            a.open_variant(s);
        }
    });
    app.add_action(&open);

    // The call notification's buttons, with the call's id.
    for (name, answer) in [
        ("call-voice", Some(false)),
        ("call-video", Some(true)),
        ("call-decline", None),
    ] {
        let a = gio::SimpleAction::new(name, Some(glib::VariantTy::UINT64));
        a.connect_activate(move |_, v| {
            let Some(call) = v.and_then(glib::Variant::get::<u64>) else {
                return;
            };
            if let Some(app) = APP.with_borrow(Clone::clone) {
                app.respond_call(call, answer);
            }
        });
        app.add_action(&a);
    }
}

fn activate(app: &adw::Application) {
    if let Some(a) = APP.with_borrow(Clone::clone) {
        a.window.present();
        handle_links(&a);
        // Started hidden at login: the offer waited for the window.
        lock::offer(&a);
        return;
    }
    if let Some(w) = app.active_window() {
        w.present();
        return;
    }
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Threnody")
        .default_width(1000)
        .default_height(700)
        .width_request(360)
        .height_request(400)
        .build();
    let spinner = gtk::Spinner::builder().spinning(true).build();
    spinner.set_size_request(32, 32);
    let status = adw::StatusPage::builder()
        .title("Opening Threnody…")
        .child(&spinner)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&status));
    window.set_content(Some(&view));
    if !hidden() {
        window.present();
    }
    // Keeps running for messages while the window is hidden.
    std::mem::forget(app.hold());
    unlock(app, &window, None, None);
}

/// Opens the identity: with the keyring's key, else a passphrase the user
/// types. A new identity is sealed with a keyring key, or a passphrase
/// when there is no keyring.
fn unlock(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    protect: Option<Protect>,
    passphrase: Option<String>,
) {
    let home = settings::default_home();
    let lock = match lock_home(&home) {
        Ok(l) => l,
        Err(e) => return fatal(window, &e),
    };
    let (app, window) = (app.clone(), window.clone());
    let h = home.clone();
    ui::bg(
        move || core::open_main(&h, protect, passphrase),
        move |r| match r {
            Ok(node) => start(&app, &window, home, node, lock),
            Err(e) => {
                // Asking, or saying it failed: the user has to see it.
                window.present();
                not_opened(&app, &window, e);
            }
        },
    );
}

/// Whether to keep the window hidden: started at login, and closing the
/// window keeps Threnody running (else hidden would mean unreachable
/// controls).
fn hidden() -> bool {
    START_HIDDEN.load(Ordering::SeqCst) && settings::Settings::load().flag(settings::BACKGROUND)
}

fn not_opened(app: &adw::Application, window: &adw::ApplicationWindow, e: OpenError) {
    match e {
        OpenError::NeedPassphrase => ask_passphrase(app, window, false),
        OpenError::NoKeyring(e) => no_keyring(app, window, &e),
        OpenError::Failed(e) if e == "Wrong passphrase" => ask_passphrase(app, window, true),
        OpenError::Failed(e) => fatal(window, &format!("Couldn't open your identity: {e}")),
    }
}

/// One app per data directory: two nodes with one identity would fight.
pub(crate) fn lock_home(home: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(home)
        .map_err(|e| format!("Couldn't create {}: {e}", home.display()))?;
    let f = std::fs::File::create(home.join("desktop.lock"))
        .map_err(|e| format!("Couldn't open the data directory: {e}"))?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(_) => Err(format!(
            "Threnody is already running with {}.",
            home.display()
        )),
    }
}

fn start(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    home: PathBuf,
    node: core::Node,
    lock: std::fs::File,
) {
    let (tx, rx) = async_channel::unbounded();
    let (app, window) = (app.clone(), window.clone());
    ui::bg(
        move || Core::start(home, node, lock, tx),
        move |core| {
            let a = App::new(&app, &window, core, rx);
            APP.with_borrow_mut(|slot| *slot = Some(a.clone()));
            handle_links(&a);
            // Hidden at login: offered once the window is opened.
            if a.window.is_visible() {
                lock::offer(&a);
            }
            let at_login = a.core.settings().flag(settings::START_AT_LOGIN);
            if let Err(e) = autostart::set(at_login) {
                eprintln!("threnody-desktop: start at login: {e}");
            }
        },
    );
}

/// Offers `threnody://` invites, `threnody-link://` codes and
/// `threnody-dir://` directories that were opened (clicked links, or
/// command-line arguments).
pub(crate) fn handle_links(a: &Rc<App>) {
    // Kept until unlocked: no dialog opens over the lock page.
    if a.lock.is_locked() {
        return;
    }
    for link in PENDING.with_borrow_mut(std::mem::take) {
        a.open_link(&link, links::LinkFrom::Opened);
    }
}

fn ask_passphrase(app: &adw::Application, window: &adw::ApplicationWindow, wrong: bool) {
    let d = ui::alert(
        "Unlock Threnody",
        if wrong {
            "That passphrase didn't open your identity. Try again."
        } else {
            "Your identity is sealed with a passphrase."
        },
        &[("quit", "Quit"), ("unlock", "Unlock")],
    );
    d.set_response_appearance("unlock", adw::ResponseAppearance::Suggested);
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    d.set_extra_child(Some(&entry));
    let (app, w) = (app.clone(), window.clone());
    d.connect_response(None, move |_, r| {
        if r == "unlock" {
            unlock(&app, &w, None, Some(entry.text().to_string()));
        } else {
            app.quit();
        }
    });
    d.present(Some(window));
}

/// There is no keyring for a new identity: protect it with a passphrase,
/// or store it unprotected only if the user says so.
fn no_keyring(app: &adw::Application, window: &adw::ApplicationWindow, why: &str) {
    let d = ui::alert(
        "Protect your identity",
        &format!(
            "There is no system keyring to keep your identity's key in ({why}). \
             Choose a passphrase to seal it instead; you'll type it each time Threnody starts."
        ),
        &[("plain", "Store unprotected"), ("seal", "Use passphrase")],
    );
    d.set_response_appearance("seal", adw::ResponseAppearance::Suggested);
    d.set_response_appearance("plain", adw::ResponseAppearance::Destructive);
    d.set_close_response("none");
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let first = gtk::PasswordEntry::builder()
        .placeholder_text("Passphrase")
        .show_peek_icon(true)
        .build();
    let second = gtk::PasswordEntry::builder()
        .placeholder_text("Again")
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    b.append(&first);
    b.append(&second);
    d.set_extra_child(Some(&b));
    let (app, w) = (app.clone(), window.clone());
    d.connect_response(None, move |_, r| match r {
        "seal" => {
            let (a, b) = (first.text().to_string(), second.text().to_string());
            if a.is_empty() || a != b {
                let why = if a.is_empty() {
                    "empty passphrase"
                } else {
                    "the passphrases differ"
                };
                no_keyring(&app, &w, why);
            } else {
                unlock(&app, &w, Some(Protect::Passphrase(a)), None);
            }
        }
        "plain" => unlock(&app, &w, Some(Protect::Nothing), None),
        _ => app.quit(),
    });
    d.present(Some(window));
}

fn fatal(window: &adw::ApplicationWindow, text: &str) {
    let d = ui::alert("Threnody can't start", text, &[("quit", "Quit")]);
    let w = window.clone();
    d.connect_response(None, move |_, _| {
        if let Some(app) = w.application() {
            app.quit();
        }
    });
    d.present(Some(window));
}
