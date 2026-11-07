//! Erasing a data directory (spec §16, "physical seizure"; Appendix C,
//! *Erasing*): the identity first, then everything else, with every
//! [`Home`] opened on it before the erase refusing to write again.
//!
//! The erase removes only the names Threnody writes into a data
//! directory, never the directory tree as a whole: a mistaken
//! `THRENODY_HOME=$HOME` must not take the user's own files with it.
//! What else it finds stays, and is listed in [`Erased::left`].

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::backup::staging_dir;
use crate::error::{Error, Result};
use crate::store::{Home, revoke, secure_delete};

/// Files Threnody writes at the top of a data directory (besides
/// `*.state` and `*.tmp`).
const FILES: &[&str] = &[
    "identity.cbor",
    "contacts.cbor",
    crate::applock::FILE,
    // The persona marker (`persona.rs`).
    "persona",
    // The node lock every node holds (the apps' and the CLI's).
    "desktop.lock",
    // The CLI daemon's control socket.
    "control.sock",
    // A staged restore's completion marker (`backup.rs`).
    "restore-ready",
    // The iOS app's settings, kept beside the identity, out of backups.
    "ios-settings.plist",
];

/// Directories Threnody makes in a data directory, removed whole.
const DIRS: &[&str] = &[
    "personas",
    // Received photos and clips (the Linux app).
    "media",
    // Edited photos not yet sent (the Linux app's editor).
    "edited",
    // Received files (the CLI, and the Linux app without XDG Downloads).
    "downloads",
    // WireGuard configurations (`threnody tunnel`).
    "wireguard",
];

/// What an erase removed: the identity homes whose platform secrets
/// (keyring entries) the caller must forget too, main home first; and
/// what it found in the home but left because Threnody didn't write it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Erased {
    pub homes: Vec<PathBuf>,
    pub left: Vec<PathBuf>,
}

/// Whether Threnody writes `name` at the top of a data directory.
fn ours(name: &str, dir: bool) -> bool {
    if dir {
        DIRS.contains(&name)
    } else {
        FILES.contains(&name) || name.ends_with(".state") || name.ends_with(".tmp")
    }
}

fn remove_file(p: &Path) -> std::io::Result<()> {
    match fs::remove_file(p) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn remove_tree(p: &Path) -> std::io::Result<()> {
    match fs::remove_dir_all(p) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The entries of `dir` that Threnody wrote (`ours`) and the rest.
fn sort_entries(dir: &Path) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (mut mine, mut other) = (Vec::new(), Vec::new());
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok((mine, other)),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let is_dir = entry.file_type()?.is_dir();
        let name = entry.file_name();
        if name.to_str().is_some_and(|n| ours(n, is_dir)) {
            mine.push(entry.path());
        } else {
            other.push(entry.path());
        }
    }
    mine.sort();
    other.sort();
    Ok((mine, other))
}

impl Home {
    /// Erases Threnody's files in this data directory: its identity,
    /// contacts, state, history, personas, received media and downloads
    /// kept inside it, the app-lock file, the staged-restore directory
    /// beside it, then the directory itself if nothing else is left in
    /// it. Safe with a node still running here: it can no longer write
    /// ([`Error::Wiped`]). Fails, having removed what it could, if any of
    /// Threnody's files remain.
    pub fn erase(&self) -> Result<Erased> {
        let dir = self.dir();
        let staging = staging_dir(dir)?;
        let link = fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink());
        let real = match fs::canonicalize(dir) {
            Ok(r) => r,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // Nothing here (or a link to nothing): only the staging
                // directory may be left.
                revoke(dir);
                remove_tree(&staging)?;
                if link {
                    remove_file(dir)?;
                }
                return Ok(Erased::default());
            }
            Err(e) => return Err(e.into()),
        };
        revoke(dir);
        revoke(&real);

        // Personas, by reading the directory: the encrypted list would
        // need the identity.
        let mut personas: Vec<String> = match fs::read_dir(real.join("personas")) {
            Ok(entries) => entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("identity.cbor").exists())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect(),
            Err(_) => Vec::new(),
        };
        personas.sort();

        // The identities first: without the seeds, every state file left
        // by an interruption is unreadable.
        secure_delete(&real.join("identity.cbor"))?;
        for p in &personas {
            secure_delete(&real.join("personas").join(p).join("identity.cbor"))?;
        }

        let (mine, _) = sort_entries(&real)?;
        for p in &mine {
            if fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()) {
                remove_tree(p)?;
            } else {
                remove_file(p)?;
            }
        }
        let (_, left) = sort_entries(&real)?;
        // Named under the directory as given, not its canonical path
        // (macOS's /private/var, Windows's \\?\ prefix).
        let left: Vec<PathBuf> = left
            .iter()
            .filter_map(|p| p.file_name().map(|n| dir.join(n)))
            .collect();
        if left.is_empty() {
            match fs::remove_dir(&real) {
                Err(e) if e.kind() != ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            if link {
                remove_file(dir)?;
            }
        }
        remove_tree(&staging)?;

        let (still, _) = sort_entries(&real)?;
        if !still.is_empty() || staging.exists() {
            return Err(Error::Malformed("data directory not fully erased"));
        }
        let mut homes = vec![dir.to_path_buf()];
        homes.extend(personas.iter().map(|p| dir.join("personas").join(p)));
        Ok(Erased { homes, left })
    }

    /// Before a new identity is created: removes what an interrupted
    /// erase left (contacts, state files, personas, the lock file), so
    /// the new identity doesn't trip over state encrypted under the old
    /// one. Leaves other files (desktop.lock, downloads). Returns whether
    /// it removed anything. No-op when an identity exists.
    pub fn clear_orphans(&self) -> Result<bool> {
        if self.has_identity() {
            return Ok(false);
        }
        let lock = self.dir().join(crate::applock::FILE);
        let had_lock = lock.exists();
        remove_file(&lock)?;
        Ok(self.remove_identity_files()? || had_lock)
    }

    /// Removes the identity (first: without it the rest is unreadable),
    /// contacts, state files and personas. Other files, such as
    /// downloads and the app-lock file, stay. No node may have this home
    /// open. Returns whether it removed anything.
    pub fn remove_identity_files(&self) -> Result<bool> {
        if !self.dir().exists() {
            return Ok(false);
        }
        let mut any = false;
        let mut remove = |p: &Path| -> Result<()> {
            if p.exists() {
                any = true;
            }
            remove_file(p)?;
            Ok(())
        };
        remove(&self.identity_path())?;
        remove(&self.contacts_path())?;
        for name in self.state_names()? {
            remove(&self.state_path(&name)?)?;
        }
        let personas = self.dir().join("personas");
        if personas.exists() {
            any = true;
            fs::remove_dir_all(personas)?;
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::staging_dir;
    use crate::persona::Personas;
    use crate::sealed::TEST_PARAMS;
    use crate::store::Contacts;

    /// A home with everything a node and the apps keep in it, and a
    /// persona; its main identity and the persona's home.
    fn used_home(dir: &Path) -> (Home, crate::Identity, PathBuf) {
        let home = Home::new(dir);
        let id = home.create_identity_with(None, TEST_PARAMS).unwrap();
        home.save_contacts(&id, &Contacts::default()).unwrap();
        for n in ["a", "b", "c"] {
            home.save_state(&id, n, b"x").unwrap();
        }
        let (_, ph, pid) = Personas::new(&home, &id)
            .create("p", None, None, 1)
            .unwrap();
        ph.save_state(&pid, "s", b"y").unwrap();
        for (d, f) in [
            ("media", "x.jpg"),
            ("downloads", "y.pdf"),
            ("wireguard", "thr0.conf"),
        ] {
            fs::create_dir_all(dir.join(d)).unwrap();
            fs::write(dir.join(d).join(f), b"z").unwrap();
        }
        fs::write(dir.join(crate::applock::FILE), b"lock").unwrap();
        fs::write(dir.join("desktop.lock"), b"").unwrap();
        (home, id, ph.dir().to_path_buf())
    }

    #[test]
    fn erase_leaves_nothing_under_the_home() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("threnody");
        let (home, id, persona) = used_home(&dir);
        let backup = home.export_backup_with(&id, "correct horse battery", TEST_PARAMS);
        home.stage_restore(&backup.unwrap(), "correct horse battery", None)
            .unwrap();
        assert!(staging_dir(&dir).unwrap().exists());

        let erased = home.erase().unwrap();
        assert!(!dir.exists());
        assert!(!staging_dir(&dir).unwrap().exists());
        assert_eq!(erased.homes, vec![dir.clone(), persona]);
        assert!(erased.left.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn erase_follows_a_symlinked_home() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("real");
        used_home(&target);
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let erased = Home::new(&link).erase().unwrap();
        assert!(!target.exists());
        assert!(fs::symlink_metadata(&link).is_err());
        assert_eq!(erased.homes[0], link);
    }

    #[test]
    fn erase_of_a_missing_home_is_ok() {
        let t = tempfile::tempdir().unwrap();
        let erased = Home::new(t.path().join("none")).erase().unwrap();
        assert_eq!(erased, Erased::default());
    }

    #[test]
    fn erase_keeps_files_it_did_not_write() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("home");
        let (home, ..) = used_home(&dir);
        fs::write(dir.join("notes.txt"), b"mine").unwrap();
        fs::create_dir_all(dir.join("Projects")).unwrap();
        fs::write(dir.join("Projects/a.txt"), b"mine").unwrap();

        let erased = home.erase().unwrap();
        assert!(dir.exists());
        assert_eq!(fs::read(dir.join("notes.txt")).unwrap(), b"mine");
        assert_eq!(fs::read(dir.join("Projects/a.txt")).unwrap(), b"mine");
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert_eq!(
            erased.left,
            vec![dir.join("Projects"), dir.join("notes.txt")]
        );
    }

    #[test]
    fn a_node_handle_cannot_resurrect_an_erased_home() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        let (home, id, _) = used_home(&dir);
        let node = Home::new(&dir);
        home.erase().unwrap();
        assert!(matches!(
            node.save_state(&id, "late", b"1"),
            Err(Error::Wiped)
        ));
        assert!(!dir.exists());
    }

    #[test]
    fn a_write_racing_the_erase_cannot_recreate_the_home() {
        for i in 0..50 {
            let t = tempfile::tempdir().unwrap();
            let dir = t.path().join(format!("h{i}"));
            let home = Home::new(&dir);
            let id = home.create_identity_with(None, TEST_PARAMS).unwrap();
            let writer = home.reopen();
            let seed = id.seed();
            let racer = std::thread::spawn(move || {
                let id = crate::Identity::from_seed(&seed);
                for n in 0..200 {
                    if let Err(Error::Wiped) = writer.save_state(&id, &format!("s{n}"), b"x") {
                        return;
                    }
                }
            });
            home.erase().unwrap();
            racer.join().unwrap();
            assert!(!dir.exists(), "run {i}");
        }
    }

    #[test]
    fn erasing_again_removes_files_saved_after_the_first_erase() {
        // An app saving a received photo outside `Home::write` (Linux's
        // media directory) while the duress PIN erased the home: the app
        // erases again once its nodes have stopped.
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        let (home, _, _) = used_home(&dir);
        home.erase().unwrap();
        fs::create_dir_all(dir.join("media")).unwrap();
        fs::write(dir.join("media").join("photo.jpg"), b"plain").unwrap();
        let again = home.erase().unwrap();
        assert_eq!(again.homes, vec![dir.clone()]);
        assert!(!dir.exists());
    }

    #[test]
    fn clear_orphans_removes_state_left_by_an_interrupted_erase() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        let (home, ..) = used_home(&dir);
        assert!(!home.clear_orphans().unwrap(), "an identity is there");
        assert!(dir.join("a.state").exists());

        fs::remove_file(dir.join("identity.cbor")).unwrap();
        assert!(home.clear_orphans().unwrap());
        assert!(home.state_names().unwrap().is_empty());
        assert!(!dir.join("contacts.cbor").exists());
        assert!(!dir.join("personas").exists());
        assert!(!dir.join(crate::applock::FILE).exists());
        assert!(dir.join("desktop.lock").exists());
        assert!(dir.join("downloads/y.pdf").exists());
        assert!(!home.clear_orphans().unwrap());
    }

    #[test]
    fn create_identity_finishes_an_interrupted_erase() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        let (home, ..) = used_home(&dir);
        fs::remove_file(dir.join("identity.cbor")).unwrap();
        home.create_identity_with(None, TEST_PARAMS).unwrap();
        assert!(home.state_names().unwrap().is_empty());
        assert!(!dir.join("personas").exists());
    }
}
