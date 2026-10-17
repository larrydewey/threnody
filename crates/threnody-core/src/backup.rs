//! Encrypted backups (spec §6.4): everything needed to bring an identity
//! back on a new device after the old one is lost, sealed under a
//! passphrase the user chooses.
//!
//! ```text
//! file      = "threnody backup\n" || sealed(passphrase, "threnody v1 backup", plaintext)
//! plaintext = { 0: version (1), 1: created_ms, 2: identity seed (32),
//!               3: contacts file, 4: [[state name, state file], ...] }
//! ```
//!
//! `sealed` is the Argon2id + ChaCha20-Poly1305 format that also seals
//! identities (`sealed.rs`). State files are copied as stored: they are
//! encrypted under a key derived from the seed, which the backup carries.
//!
//! Left out on purpose: one-time prekeys (ours and our contacts'), since
//! restoring used ones would let old sealed messages replay or be sealed
//! twice to the same key; relay tokens, which are spent once; and
//! personas, which must stay unlinkable to the main identity. See
//! `docs/appendix-c-wire-format.md`, *Backups*.

use std::fs;
use std::path::{Path, PathBuf};

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::sealed::{self, KdfParams};
use crate::store::{Contacts, Home};

/// The shortest passphrase a backup accepts. A backup file may sit on a
/// cloud drive for years, so its passphrase is all that protects it.
pub const MIN_PASSPHRASE_CHARS: usize = 12;

const MAGIC: &[u8] = b"threnody backup\n";
const PURPOSE: &str = "threnody v1 backup";
const VERSION: u64 = 1;

/// State files a backup leaves out (see the module comment).
const LEFT_OUT: &[&str] = &["prekeys", "bundles", "relay-wallet", "personas"];

/// Marks a staged restore as complete (see [`Home::stage_restore`]).
const READY: &str = "restore-ready";

/// What a backup restored.
pub struct Restored {
    pub identity: Identity,
    /// When the backup was made (ms since the Unix epoch).
    pub created_ms: u64,
}

fn check_passphrase(passphrase: &str) -> Result<()> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(Error::WeakPassphrase(MIN_PASSPHRASE_CHARS));
    }
    Ok(())
}

impl Home {
    /// Seals this home's identity, contacts and state into a backup.
    /// Safe while a node runs here: every file is replaced atomically.
    pub fn export_backup(&self, identity: &Identity, passphrase: &str) -> Result<Vec<u8>> {
        self.export_backup_with(identity, passphrase, KdfParams::DEFAULT)
    }

    pub fn export_backup_with(
        &self,
        identity: &Identity,
        passphrase: &str,
        p: KdfParams,
    ) -> Result<Vec<u8>> {
        check_passphrase(passphrase)?;
        let contacts = match fs::read(self.contacts_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Contacts::default().encode()?,
            Err(e) => return Err(e.into()),
        };
        let mut states = Vec::new();
        for name in self.state_names()? {
            if LEFT_OUT.contains(&name.as_str()) {
                continue;
            }
            match fs::read(self.state_path(&name)?) {
                Ok(b) => states.push((name, b)),
                // Removed since we listed it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let seed = identity.seed();
        let size = 128
            + contacts.len()
            + states
                .iter()
                .map(|(n, b)| n.len() + b.len() + 16)
                .sum::<usize>();
        let plain = Zeroizing::new(cbor::to_vec(size, |e| {
            e.map_len(5)?;
            e.u8(0)?.uint(VERSION)?;
            e.u8(1)?.u64(crate::now_ms())?;
            e.u8(2)?.bytes(&seed[..])?;
            e.u8(3)?.bytes(&contacts)?;
            e.u8(4)?.array_len(states.len())?;
            for (name, bytes) in &states {
                e.array_len(2)?.str(name)?.bytes(bytes)?;
            }
            Ok(())
        })?);
        let mut out = MAGIC.to_vec();
        out.extend(sealed::seal(passphrase.as_bytes(), PURPOSE, &plain, p)?);
        Ok(out)
    }

    /// Replaces everything in this home with a backup: the identity,
    /// contacts, history and other state, and any personas (which a
    /// backup never holds). The restored identity is stored sealed under
    /// `seal` when given, as [`Home::create_identity`] would.
    ///
    /// The backup is opened and checked before anything is removed, so a
    /// wrong passphrase or a damaged file changes nothing. No node may
    /// have this home open.
    pub fn restore_backup(
        &self,
        backup: &[u8],
        passphrase: &str,
        seal: Option<&[u8]>,
    ) -> Result<Restored> {
        if crate::persona::is_persona(self) {
            return Err(Error::Malformed(
                "a backup can't be restored into a persona",
            ));
        }
        let opened = open(backup, passphrase)?;
        for (name, _) in &opened.states {
            self.state_path(name)?;
        }

        self.wipe()?;
        // The identity first, so an interrupted restore still opens, and
        // restoring again starts over.
        let identity = &opened.restored.identity;
        self.store_identity(identity, seal, KdfParams::DEFAULT)?;
        self.write_contacts_file(&opened.contacts)?;
        for (name, bytes) in &opened.states {
            if !LEFT_OUT.contains(&name.as_str()) {
                self.write_state_file(name, bytes)?;
            }
        }
        Ok(opened.restored)
    }

    /// Prepares a restore beside this home, in [`staging_dir`], for
    /// [`apply_staged_restore`] to carry out before a node next opens
    /// this home. For apps that can't stop their node: they stage, then
    /// restart. Nothing here changes until then.
    pub fn stage_restore(
        &self,
        backup: &[u8],
        passphrase: &str,
        seal: Option<&[u8]>,
    ) -> Result<Restored> {
        let staging = Home::new(staging_dir(self.dir())?);
        if staging.dir().exists() {
            fs::remove_dir_all(staging.dir())?;
        }
        let restored = staging.restore_backup(backup, passphrase, seal)?;
        fs::write(staging.dir().join(READY), b"")?;
        Ok(restored)
    }

    /// Removes the identity (first: without it the rest is unreadable),
    /// contacts, state files and personas.
    fn wipe(&self) -> Result<()> {
        if !self.dir().exists() {
            return Ok(());
        }
        let remove = |p: &std::path::Path| match fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
        remove(&self.identity_path())?;
        remove(&self.contacts_path())?;
        for name in self.state_names()? {
            remove(&self.state_path(&name)?)?;
        }
        let personas = self.dir().join("personas");
        if personas.exists() {
            fs::remove_dir_all(personas)?;
        }
        Ok(())
    }
}

/// Where [`Home::stage_restore`] prepares a restore of `home`: beside it,
/// so the user's other files in `home` (downloads) stay where they are.
pub fn staging_dir(home: &Path) -> Result<PathBuf> {
    let (Some(parent), Some(name)) = (home.parent(), home.file_name()) else {
        return Err(Error::Malformed("data directory has no name"));
    };
    let mut name = name.to_owned();
    name.push(".restore");
    Ok(parent.join(name))
}

/// Carries out a restore staged by [`Home::stage_restore`], if there is
/// one: replaces `home`'s identity, contacts, state and personas with the
/// staged ones. Call it before opening a node on `home`, with no node
/// running there. Returns whether it restored. An interrupted run is
/// finished by the next one; an interrupted staging is discarded.
pub fn apply_staged_restore(home: &Home) -> Result<bool> {
    let staging = Home::new(staging_dir(home.dir())?);
    if !staging.dir().join(READY).exists() {
        if staging.dir().exists() {
            fs::remove_dir_all(staging.dir())?;
        }
        return Ok(false);
    }
    home.wipe()?;
    // The identity first, as in `restore_backup`.
    home.write_identity_file(&fs::read(staging.identity_path())?)?;
    home.write_contacts_file(&fs::read(staging.contacts_path())?)?;
    for name in staging.state_names()? {
        home.write_state_file(&name, &fs::read(staging.state_path(&name)?)?)?;
    }
    fs::remove_dir_all(staging.dir())?;
    Ok(true)
}

/// Opens a backup without restoring it, to show whose it is and when it
/// was made before replacing anything.
pub fn inspect_backup(backup: &[u8], passphrase: &str) -> Result<Restored> {
    Ok(open(backup, passphrase)?.restored)
}

struct Opened {
    restored: Restored,
    contacts: Vec<u8>,
    states: Vec<(String, Vec<u8>)>,
}

fn open(backup: &[u8], passphrase: &str) -> Result<Opened> {
    let sealed = backup
        .strip_prefix(MAGIC)
        .ok_or(Error::Malformed("not a Threnody backup"))?;
    let plain = sealed::open(passphrase.as_bytes(), PURPOSE, sealed)?;

    let mut dec = Decoder::new(&plain);
    let (mut ver, mut created, mut seed, mut contacts) = (None, None, None, None);
    let mut states = Vec::new();
    read_map(&mut dec, |k, d| {
        match k {
            0 => ver = Some(d.u64()?),
            1 => created = Some(d.u64()?),
            2 => seed = Some(Zeroizing::new(fixed_bytes::<32>(d)?)),
            3 => contacts = Some(d.bytes()?.to_vec()),
            4 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("backup state entry"));
                    }
                    states.push((d.str()?.to_string(), d.bytes()?.to_vec()));
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    match required(ver, "backup version")? {
        VERSION => {}
        v => return Err(Error::UnsupportedVersion(v)),
    }
    let contacts = required(contacts, "backup contacts")?;
    Contacts::decode(&contacts)?;
    Ok(Opened {
        restored: Restored {
            identity: Identity::from_seed(&*required(seed, "backup identity")?),
            created_ms: required(created, "backup time")?,
        },
        contacts,
        states,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: KdfParams = KdfParams {
        m_kib: 64,
        t: 1,
        lanes: 1,
    };
    const PW: &str = "correct horse battery";

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn restores_identity_contacts_and_state_onto_a_new_device() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        let peer = Identity::generate().public();
        let mut contacts = Contacts::default();
        contacts.observe(peer, None, 1);
        old.save_contacts(&contacts).unwrap();
        old.save_state(&id, "hist-p-abc", b"history").unwrap();
        old.save_state(&id, "prekeys", b"one-time keys").unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();
        assert_eq!(
            inspect_backup(&backup, PW).unwrap().identity.public(),
            id.public()
        );

        // The new device already made an identity of its own.
        let new = Home::new(b.path());
        let other = new.create_identity(None).unwrap();
        new.save_state(&other, "groups", b"theirs").unwrap();
        let r = new.restore_backup(&backup, PW, Some(b"local")).unwrap();

        assert_eq!(r.identity.public(), id.public());
        assert!(new.identity_is_sealed().unwrap());
        let loaded = new.load_identity(Some(b"local")).unwrap();
        assert_eq!(loaded.public(), id.public());
        assert!(new.load_contacts().unwrap().get(&peer).is_some());
        assert_eq!(
            &new.load_state(&loaded, "hist-p-abc").unwrap().unwrap()[..],
            b"history"
        );
        assert!(new.load_state(&loaded, "prekeys").unwrap().is_none());
        assert!(new.load_state(&loaded, "groups").unwrap().is_none());
    }

    #[test]
    fn a_wrong_passphrase_or_damage_changes_nothing() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        let mut backup = old.export_backup_with(&id, PW, FAST).unwrap();

        let new = Home::new(b.path());
        let mine = new.create_identity(None).unwrap();
        assert!(matches!(
            new.restore_backup(&backup, "wrong passphrase!", None),
            Err(Error::Decrypt)
        ));
        let n = backup.len();
        backup[n - 1] ^= 1;
        assert!(new.restore_backup(&backup, PW, None).is_err());
        assert!(new.restore_backup(b"something else", PW, None).is_err());
        assert_eq!(new.load_identity(None).unwrap().public(), mine.public());
    }

    #[test]
    fn a_staged_restore_applies_at_the_next_start_and_keeps_other_files() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        old.save_state(&id, "groups", b"ours").unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();

        let home = Home::new(b.path().join("threnody"));
        let mine = home.create_identity(None).unwrap();
        home.save_state(&mine, "lists", b"theirs").unwrap();
        fs::create_dir_all(home.dir().join("downloads")).unwrap();
        fs::write(home.dir().join("downloads/photo.jpg"), b"jpeg").unwrap();

        assert!(!apply_staged_restore(&home).unwrap(), "nothing staged");
        home.stage_restore(&backup, PW, None).unwrap();
        // Staging changes nothing yet.
        assert_eq!(home.load_identity(None).unwrap().public(), mine.public());

        assert!(apply_staged_restore(&home).unwrap());
        let now = home.load_identity(None).unwrap();
        assert_eq!(now.public(), id.public());
        assert_eq!(
            &home.load_state(&now, "groups").unwrap().unwrap()[..],
            b"ours"
        );
        assert!(home.load_state(&now, "lists").unwrap().is_none());
        assert!(home.dir().join("downloads/photo.jpg").exists());
        assert!(!staging_dir(home.dir()).unwrap().exists());
        assert!(!apply_staged_restore(&home).unwrap(), "applied once");
    }

    #[test]
    fn short_passphrases_are_refused() {
        let a = temp();
        let home = Home::new(a.path());
        let id = home.create_identity(None).unwrap();
        assert!(matches!(
            home.export_backup_with(&id, "too short", FAST),
            Err(Error::WeakPassphrase(MIN_PASSPHRASE_CHARS))
        ));
    }
}
