//! Backups in Preferences: save one sealed under a passphrase, or replace
//! this computer's identity with one after losing the computer it came
//! from. A restore is staged and finished when Threnody next starts,
//! before the node opens (`core::open_main`).

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::keyring;
use crate::ui;
use crate::window::App;

pub fn group(app: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Backup")
        .description(
            "A backup holds your identity, contacts and message history, sealed under a \
             passphrase you choose. It doesn't hold anonymous identities or received files.",
        )
        .build();
    let save = row("Back Up…", "Save a backup to a file");
    let a = app.clone();
    save.connect_activated(move |_| back_up(&a));
    group.add(&save);
    let restore = row(
        "Restore from a Backup…",
        "Only after losing the device it came from",
    );
    let a = app.clone();
    restore.connect_activated(move |_| pick_backup(&a));
    group.add(&restore);
    group
}

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

fn entry(placeholder: &str) -> gtk::PasswordEntry {
    gtk::PasswordEntry::builder()
        .placeholder_text(placeholder)
        .show_peek_icon(true)
        .activates_default(true)
        .build()
}

fn back_up(app: &Rc<App>) {
    let min = threnody_ffi::backup_passphrase_min_chars();
    let d = ui::alert(
        "Back up",
        &format!(
            "Choose a passphrase of at least {min} characters. Anyone with the backup and its \
             passphrase can become you, and nobody can open it without the passphrase."
        ),
        &[("cancel", "Cancel"), ("ok", "Choose File…")],
    );
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let (first, second) = (entry("Passphrase"), entry("Again"));
    b.append(&first);
    b.append(&second);
    d.set_extra_child(Some(&b));
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r != "ok" {
            return;
        }
        let (pw, again) = (first.text().to_string(), second.text().to_string());
        if pw.chars().count() < min as usize {
            a.toast(&format!("The passphrase needs at least {min} characters"));
        } else if pw != again {
            a.toast("The passphrases differ");
        } else {
            save_to(&a, pw);
        }
    });
    d.present(Some(&app.window));
}

fn save_to(app: &Rc<App>, pw: String) {
    let day = glib::DateTime::now_local()
        .and_then(|t| t.format("%Y-%m-%d"))
        .map_or_else(|_| "backup".into(), |d| d.to_string());
    let dialog = gtk::FileDialog::builder()
        .title("Save Backup")
        .initial_name(format!("threnody-{day}.backup"))
        .modal(true)
        .build();
    let a = app.clone();
    dialog.save(Some(&app.window), gio::Cancellable::NONE, move |r| {
        let Some(path) = r.ok().and_then(|f| f.path()) else {
            return;
        };
        let core = a.core.clone();
        let a = a.clone();
        ui::bg(
            move || -> Result<PathBuf, String> {
                let data = core.main.export_backup(pw).map_err(|e| e.to_string())?;
                write_private(&path, &data).map_err(|e| e.to_string())?;
                Ok(path)
            },
            move |r| match r {
                Ok(p) => a.toast(&format!("Backup saved to {}", p.display())),
                Err(e) => a.toast(&format!("Couldn't back up: {e}")),
            },
        );
    });
}

fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

fn pick_backup(app: &Rc<App>) {
    let dialog = gtk::FileDialog::builder()
        .title("Restore from a Backup")
        .modal(true)
        .build();
    let a = app.clone();
    dialog.open(Some(&app.window), gio::Cancellable::NONE, move |r| {
        let Some(path) = r.ok().and_then(|f| f.path()) else {
            return;
        };
        match std::fs::read(&path) {
            Ok(data) => ask_passphrase(&a, data),
            Err(e) => a.toast(&format!("Couldn't read {}: {e}", path.display())),
        }
    });
}

fn ask_passphrase(app: &Rc<App>, data: Vec<u8>) {
    let d = ui::alert(
        "Restore from a backup",
        "Type the backup's passphrase.",
        &[("cancel", "Cancel"), ("ok", "Open")],
    );
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let pw = entry("Passphrase");
    d.set_extra_child(Some(&pw));
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r != "ok" {
            return;
        }
        let (pw, data) = (pw.text().to_string(), data.clone());
        let a = a.clone();
        ui::bg(
            {
                let (pw, data) = (pw.clone(), data.clone());
                move || threnody_ffi::inspect_backup(data, pw).map_err(|e| e.to_string())
            },
            move |r| match r {
                Ok(info) => confirm(&a, data, pw, &info),
                Err(e) => a.toast(&format!("Couldn't open the backup: {e}")),
            },
        );
    });
    d.present(Some(&app.window));
}

fn confirm(app: &Rc<App>, data: Vec<u8>, pw: String, info: &threnody_ffi::BackupInfo) {
    let made = i64::try_from(info.created_ms / 1000)
        .ok()
        .and_then(|s| glib::DateTime::from_unix_local(s).ok())
        .and_then(|t| t.format("%e %B %Y, %H:%M").ok())
        .map_or_else(|| "at an unknown time".into(), |d| d.trim().to_string());
    let same = info.fingerprint == app.core.main.device_fingerprint();
    let body = format!(
        "This backup of {} was made {made}.\n\n{}Restoring removes this computer's identity, \
         contacts, history and anonymous identities, and puts the backup's in their place. \
         Restore only when the device the backup came from is gone: to use two devices as one, \
         link them instead.\n\nThrenody closes to finish; open it again afterwards.",
        crate::core::short(&info.fingerprint),
        if same {
            "It is this computer's own identity: anything newer than the backup is lost. "
        } else {
            ""
        }
    );
    let a = app.clone();
    ui::confirm(
        &app.window,
        "Replace this computer's identity?",
        &body,
        "Restore",
        true,
        move || {
            let (data, pw) = (data.clone(), pw.clone());
            let home = a.core.home.clone();
            let a = a.clone();
            ui::bg(
                move || {
                    // Sealed like the identity it replaces; a plain one is
                    // sealed with a keyring key when Threnody next opens it.
                    let seal = keyring::get(&home).map(|s| s.0.clone());
                    threnody_ffi::stage_restore(home.display().to_string(), data, pw, seal)
                        .map_err(|e| e.to_string())
                },
                move |r| match r {
                    Ok(_) => {
                        let d = ui::alert(
                            "Ready to restore",
                            "Threnody will now close. Open it again to finish restoring.",
                            &[("quit", "Close Threnody")],
                        );
                        let w = a.window.clone();
                        d.connect_response(None, move |_, _| {
                            if let Some(app) = w.application() {
                                app.activate_action("quit", None);
                            }
                        });
                        d.present(Some(&a.window));
                    }
                    Err(e) => a.toast(&format!("Couldn't restore: {e}")),
                },
            );
        },
    );
}
