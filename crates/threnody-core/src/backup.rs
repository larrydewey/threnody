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
//! The contact book, though kept as the `contacts` state file, goes in
//! key 3 as plain CBOR (as before it was encrypted), so older versions
//! restore it too.
//!
//! Left out on purpose: one-time prekeys (ours and our contacts'), since
//! restoring used ones would let old sealed messages replay or be sealed
//! twice to the same key; relay tokens, which are spent once; personas,
//! which must stay unlinkable to the main identity; scheduled messages,
//! since a backup restored later must not suddenly send old messages, or
//! send them a second time while the old device still runs; and saved
//! tunnel peers, whose PSKs are bound to sessions with this device, so
//! they never leave it. See `docs/appendix-c-wire-format.md`, *Backups*.

use std::fs;
use std::path::{Path, PathBuf};

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::error::{Error, Result};
use crate::identity::Identity;
use crate::sealed::{self, KdfParams};
use crate::store::{CONTACTS_STATE, Contacts, Home};

/// The shortest passphrase a backup accepts. A backup file may sit on a
/// cloud drive for years, so its passphrase is all that protects it.
pub const MIN_PASSPHRASE_CHARS: usize = 12;

const MAGIC: &[u8] = b"threnody backup\n";
const PURPOSE: &str = "threnody v1 backup";
const VERSION: u64 = 1;

/// State files a backup leaves out (see the module comment). Scheduled
/// messages among them: restored later, they would send old messages, or
/// send them twice if the old device still runs. `tunnels`: PSKs are
/// bound to sessions with this device, so they never leave it.
const LEFT_OUT: &[&str] = &[
    "prekeys",
    "bundles",
    "relay-wallet",
    "personas",
    "scheduled",
    crate::tunnel::STATE,
];

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
        let (contacts, dropped) = self.read_contacts(identity)?;
        let contacts = Zeroizing::new(contacts.encode()?);
        let mut states = Vec::new();
        for name in self.state_names()? {
            if LEFT_OUT.contains(&name.as_str())
                || name == CONTACTS_STATE
                || crate::history::is_history_name(&name)
            {
                continue;
            }
            match fs::read(self.state_path(&name)?) {
                Ok(b) => states.push((name, b)),
                // Removed since we listed it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        states.extend(self.history_for_backup(identity, &dropped.unwrap_or_default())?);
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

        self.remove_identity_files()?;
        // The identity first, so an interrupted restore still opens, and
        // restoring again starts over.
        let identity = &opened.restored.identity;
        self.store_identity(identity, seal, KdfParams::DEFAULT)?;
        self.save_contacts(identity, &Contacts::decode(&opened.contacts)?)?;
        for (name, bytes) in &opened.states {
            if !LEFT_OUT.contains(&name.as_str()) && name != CONTACTS_STATE {
                self.write_state_file(name, bytes)?;
            }
        }
        // History comes under the names older versions read; here it
        // moves to keyed names at once.
        self.migrate_history_names(identity);
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
        let staging = self.child(staging_dir(self.dir())?);
        if staging.dir().exists() {
            fs::remove_dir_all(staging.dir())?;
        }
        let restored = staging.restore_backup(backup, passphrase, seal)?;
        staging.write(&staging.dir().join(READY), b"")?;
        Ok(restored)
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
    let staging = home.child(staging_dir(home.dir())?);
    if !staging.dir().join(READY).exists() {
        if staging.dir().exists() {
            fs::remove_dir_all(staging.dir())?;
        }
        return Ok(false);
    }
    home.remove_identity_files()?;
    // The identity first, as in `restore_backup`.
    home.write_identity_file(&fs::read(staging.identity_path())?)?;
    // Only a restore staged before contacts were encrypted has this.
    match fs::read(staging.contacts_path()) {
        Ok(b) => home.write_contacts_file(&b)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
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

/// The state files a backup holds, by name (tests only).
#[cfg(test)]
pub(crate) fn states_in(backup: &[u8], passphrase: &str) -> Vec<(String, Vec<u8>)> {
    open(backup, passphrase).unwrap().states
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
        old.save_contacts(&id, &contacts).unwrap();
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
        assert!(new.load_contacts(&loaded).unwrap().get(&peer).is_some());
        assert!(!new.dir().join("contacts.cbor").exists());
        assert_eq!(
            &new.load_state(&loaded, "hist-p-abc").unwrap().unwrap()[..],
            b"history"
        );
        assert!(new.load_state(&loaded, "prekeys").unwrap().is_none());
        assert!(new.load_state(&loaded, "groups").unwrap().is_none());
    }

    #[test]
    fn tunnel_state_is_left_out_of_backups() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        old.save_contacts(&id, &Contacts::default()).unwrap();
        old.save_state(&id, crate::tunnel::STATE, b"pairwise keys")
            .unwrap();
        old.save_state(&id, "groups", b"groups").unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();
        assert!(
            !states_in(&backup, PW)
                .iter()
                .any(|(n, _)| n == crate::tunnel::STATE)
        );
        let new = Home::new(b.path());
        new.restore_backup(&backup, PW, None).unwrap();
        assert!(new.load_state(&id, "groups").unwrap().is_some());
        assert!(new.load_state(&id, crate::tunnel::STATE).unwrap().is_none());

        // A backup that holds one anyway (made by hand) doesn't restore it.
        let contacts = Contacts::default().encode().unwrap();
        let tunnels = old
            .seal_state(&id, crate::tunnel::STATE, b"pairwise keys")
            .unwrap();
        let groups = old.seal_state(&id, "groups", b"groups").unwrap();
        let seed = id.seed();
        let plain = cbor::to_vec(1024, |e| {
            e.map_len(5)?;
            e.u8(0)?.uint(VERSION)?;
            e.u8(1)?.u64(1)?;
            e.u8(2)?.bytes(&seed[..])?;
            e.u8(3)?.bytes(&contacts)?;
            e.u8(4)?.array_len(2)?;
            e.array_len(2)?.str(crate::tunnel::STATE)?.bytes(&tunnels)?;
            e.array_len(2)?.str("groups")?.bytes(&groups)?;
            Ok(())
        })
        .unwrap();
        let mut forged = MAGIC.to_vec();
        forged.extend(sealed::seal(PW.as_bytes(), PURPOSE, &plain, FAST).unwrap());
        let c = temp();
        let third = Home::new(c.path());
        third.restore_backup(&forged, PW, None).unwrap();
        assert!(third.load_state(&id, "groups").unwrap().is_some());
        assert!(!c.path().join("tunnels.state").exists());
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
    fn a_restore_keeps_the_app_lock() {
        use crate::applock::{LockPolicy, SecretKind, Verdict};
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();

        let home = Home::new(b.path().join("threnody"));
        home.create_identity(None).unwrap();
        home.set_app_lock_with(
            Some(b"p"),
            None,
            Some(("2468", SecretKind::Digits)),
            LockPolicy::DEFAULT,
            FAST,
        )
        .unwrap();
        home.restore_backup(&backup, PW, None).unwrap();
        assert!(home.app_lock_enabled());
        home.stage_restore(&backup, PW, None).unwrap();
        assert!(apply_staged_restore(&home).unwrap());
        assert_eq!(
            home.check_lock(Some(b"p"), "2468", 1).unwrap(),
            Verdict::Unlock
        );
    }

    #[test]
    fn scheduled_state_is_not_backed_up() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        old.save_state(&id, "scheduled", b"send later").unwrap();
        old.save_state(&id, "lists", b"groceries").unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();
        let names: Vec<String> = states_in(&backup, PW).into_iter().map(|(n, _)| n).collect();
        assert!(!names.iter().any(|n| n == "scheduled"));
        assert!(names.iter().any(|n| n == "lists"));

        let new = Home::new(b.path());
        let r = new.restore_backup(&backup, PW, None).unwrap();
        assert!(new.load_state(&r.identity, "scheduled").unwrap().is_none());
        assert!(new.load_state(&r.identity, "lists").unwrap().is_some());
    }

    #[test]
    fn restoring_drops_a_pending_schedule() {
        let (a, b) = (temp(), temp());
        let old = Home::new(a.path());
        let id = old.create_identity(None).unwrap();
        let backup = old.export_backup_with(&id, PW, FAST).unwrap();

        // The same identity, with messages scheduled since the backup.
        let home = Home::new(b.path());
        home.restore_backup(&backup, PW, None).unwrap();
        home.save_state(&id, "scheduled", b"send later").unwrap();
        home.restore_backup(&backup, PW, None).unwrap();
        assert!(home.load_state(&id, "scheduled").unwrap().is_none());
        assert!(!home.dir().join("scheduled.state").exists());
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
