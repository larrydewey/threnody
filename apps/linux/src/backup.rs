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
                move || current_seal(&home),
                move |seal| match seal {
                    Seal::Keyring(key) => stage(&a, data, pw, Some(key)),
                    Seal::Plain => stage(&a, data, pw, None),
                    Seal::Passphrase => ask_identity_passphrase(&a, data, pw),
                },
            );
        },
    );
}

/// How the restored identity is sealed: as the one it replaces.
#[derive(Debug, PartialEq)]
enum Seal {
    /// With the keyring's key for this data directory.
    Keyring(String),
    /// With the user's own passphrase, which only they know: asked for.
    Passphrase,
    /// Not at all, as the user chose (a keyring key seals it when
    /// Threnody next opens it, if there is a keyring by then).
    Plain,
}

/// `sealed`: whether the current identity is sealed. `keyring`: the
/// keyring's key for this data directory, and whether it opens it.
fn restore_seal(sealed: bool, keyring: Option<(String, bool)>) -> Seal {
    match keyring {
        Some((key, opens)) if opens || !sealed => Seal::Keyring(key),
        _ if sealed => Seal::Passphrase,
        _ => Seal::Plain,
    }
}

fn current_seal(home: &std::path::Path) -> Seal {
    let store = threnody_core::store::Home::new(home);
    let sealed = store.identity_is_sealed().unwrap_or(false);
    let key = keyring::get(home).map(|k| {
        let opens = sealed && store.load_identity(Some(k.0.as_bytes())).is_ok();
        (k.0.clone(), opens)
    });
    restore_seal(sealed, key)
}

/// The identity is sealed with the user's own passphrase, so the
/// restored one will be too: asks for it, checked against the identity.
fn ask_identity_passphrase(app: &Rc<App>, data: Vec<u8>, backup_pw: String) {
    let d = ui::alert(
        "Your passphrase",
        "Your identity is protected with a passphrase. Type it to protect the restored \
         identity the same way.",
        &[("cancel", "Cancel"), ("ok", "Restore")],
    );
    d.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    let pw = entry("Passphrase");
    d.set_extra_child(Some(&pw));
    let a = app.clone();
    d.connect_response(None, move |_, r| {
        if r != "ok" {
            return;
        }
        let (pw, data, backup_pw) = (pw.text().to_string(), data.clone(), backup_pw.clone());
        let home = a.core.home.clone();
        let a = a.clone();
        ui::bg(
            move || {
                let store = threnody_core::store::Home::new(&home);
                store.load_identity(Some(pw.as_bytes())).is_ok().then_some(pw)
            },
            move |r| match r {
                Some(pw) => stage(&a, data, backup_pw, Some(pw)),
                None => a.toast("Wrong passphrase"),
            },
        );
    });
    d.present(Some(&app.window));
}

/// Stages the restore, sealed under `seal`, and closes Threnody to finish.
fn stage(app: &Rc<App>, data: Vec<u8>, pw: String, seal: Option<String>) {
    let home = app.core.home.clone();
    let a = app.clone();
    ui::bg(
        move || {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_restore_is_sealed_like_the_identity_it_replaces() {
        let key = || Some(("k".to_owned(), true));
        assert_eq!(restore_seal(true, key()), Seal::Keyring("k".into()));
        // The user's own passphrase, with no keyring entry or a stale one.
        assert_eq!(restore_seal(true, None), Seal::Passphrase);
        assert_eq!(
            restore_seal(true, Some(("old".into(), false))),
            Seal::Passphrase
        );
        assert_eq!(
            restore_seal(false, Some(("k".into(), false))),
            Seal::Keyring("k".into())
        );
        assert_eq!(restore_seal(false, None), Seal::Plain);
    }
}
