//! Local-first persistence (spec §6.4): the device identity and the contact
//! book, as CBOR files in a per-user directory.
//!
//! This is the software-keystore fallback of spec §3.1: secrets live in a
//! file readable only by the owning user (mode 0600, directory 0700).
//! Platform keystores (Secure Enclave, StrongBox, TPM) plug in here later.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::account::AccountId;
use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::kdf;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::identity::{Fingerprint, Identity, PublicIdentity, fingerprint_matches_prefix};
use crate::sealed::{self, KdfParams};

const FILE_VERSION: u64 = 1;
const IDENTITY_PURPOSE: &str = "threnody v1 identity seed";

enum IdentityFile {
    Plain(Zeroizing<[u8; 32]>),
    Sealed(Vec<u8>),
}

/// The per-user Threnody directory.
pub struct Home {
    dir: PathBuf,
}

impl Home {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn identity_path(&self) -> PathBuf {
        self.dir.join("identity.cbor")
    }

    fn contacts_path(&self) -> PathBuf {
        self.dir.join("contacts.cbor")
    }

    pub fn has_identity(&self) -> bool {
        self.identity_path().exists()
    }

    /// Creates and stores a fresh identity, sealed under `passphrase` when
    /// one is given.
    pub fn create_identity(&self, passphrase: Option<&[u8]>) -> Result<Identity> {
        self.create_identity_with(passphrase, KdfParams::DEFAULT)
    }

    pub fn create_identity_with(
        &self,
        passphrase: Option<&[u8]>,
        p: KdfParams,
    ) -> Result<Identity> {
        let id = Identity::generate();
        self.store_identity(&id, passphrase, p)?;
        Ok(id)
    }

    fn store_identity(&self, id: &Identity, passphrase: Option<&[u8]>, p: KdfParams) -> Result<()> {
        let seed = id.seed();
        let sealed = passphrase
            .map(|pw| sealed::seal(pw, IDENTITY_PURPOSE, &seed[..], p))
            .transpose()?;
        let bytes = Zeroizing::new(cbor::to_vec(1024, |e| {
            e.map_len(2)?;
            e.u8(0)?.uint(FILE_VERSION)?;
            match &sealed {
                Some(s) => e.u8(2)?.bytes(s)?,
                None => e.u8(1)?.bytes(&seed[..])?,
            };
            Ok(())
        })?);
        write_private(&self.dir, &self.identity_path(), &bytes)
    }

    /// True when the identity file is passphrase-sealed.
    pub fn identity_is_sealed(&self) -> Result<bool> {
        Ok(matches!(
            self.read_identity_file()?,
            IdentityFile::Sealed(_)
        ))
    }

    /// Loads the identity. Sealed files need the passphrase: `None` yields
    /// [`Error::PassphraseRequired`], a wrong one [`Error::Decrypt`].
    pub fn load_identity(&self, passphrase: Option<&[u8]>) -> Result<Identity> {
        match self.read_identity_file()? {
            IdentityFile::Plain(seed) => Ok(Identity::from_seed(&seed)),
            IdentityFile::Sealed(blob) => {
                let pw = passphrase.ok_or(Error::PassphraseRequired)?;
                let seed = sealed::open(pw, IDENTITY_PURPOSE, &blob)?;
                let seed: &[u8; 32] = seed[..]
                    .try_into()
                    .map_err(|_| Error::Malformed("identity seed"))?;
                Ok(Identity::from_seed(seed))
            }
        }
    }

    /// Adds, changes or removes (`new = None`) the identity passphrase.
    pub fn change_passphrase(&self, current: Option<&[u8]>, new: Option<&[u8]>) -> Result<()> {
        let id = self.load_identity(current)?;
        self.store_identity(&id, new, KdfParams::DEFAULT)
    }

    fn read_identity_file(&self) -> Result<IdentityFile> {
        let bytes = Zeroizing::new(fs::read(self.identity_path())?);
        let mut dec = Decoder::new(&bytes);
        let (mut ver, mut seed, mut blob) = (None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => seed = Some(Zeroizing::new(fixed_bytes::<32>(d)?)),
                2 => blob = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        check_version(ver)?;
        match (seed, blob) {
            (Some(s), None) => Ok(IdentityFile::Plain(s)),
            (None, Some(b)) => Ok(IdentityFile::Sealed(b)),
            _ => Err(Error::Malformed(
                "identity file needs exactly one of seed, sealed seed",
            )),
        }
    }

    pub fn load_contacts(&self) -> Result<Contacts> {
        match fs::read(self.contacts_path()) {
            Ok(b) => Contacts::decode(&b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Contacts::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_contacts(&self, c: &Contacts) -> Result<()> {
        write_private(&self.dir, &self.contacts_path(), &c.encode()?)
    }

    fn state_path(&self, name: &str) -> Result<PathBuf> {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(Error::Malformed("state name"));
        }
        Ok(self.dir.join(format!("{name}.state")))
    }

    /// Stores `data` encrypted under a key derived from the identity seed
    /// (so it is as protected as the identity itself), bound to `name`.
    ///
    /// ```text
    /// file = nonce (12) || ChaCha20-Poly1305(KDF("state encryption key", seed), nonce, ad = name, data)
    /// ```
    pub fn save_state(&self, identity: &Identity, name: &str, data: &[u8]) -> Result<()> {
        let path = self.state_path(name)?;
        let key = state_key(identity);
        let nonce: [u8; 12] = random_bytes();
        let mut out = nonce.to_vec();
        out.extend(Suite::ChaCha20Poly1305.seal(&key, &nonce, name.as_bytes(), data));
        write_private(&self.dir, &path, &out)
    }

    /// Deletes a state file (no error if absent).
    pub fn remove_state(&self, name: &str) -> Result<()> {
        match fs::remove_file(self.state_path(name)?) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Loads state written by [`Home::save_state`]; `Ok(None)` if absent.
    pub fn load_state(
        &self,
        identity: &Identity,
        name: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let path = self.state_path(name)?;
        let raw = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if raw.len() < 12 {
            return Err(Error::Malformed("state file"));
        }
        let (nonce, ct) = raw.split_at(12);
        let nonce: [u8; 12] = nonce
            .try_into()
            .map_err(|_| Error::Malformed("state file"))?;
        let pt = Suite::ChaCha20Poly1305.open(&state_key(identity), &nonce, name.as_bytes(), ct)?;
        Ok(Some(Zeroizing::new(pt)))
    }
}

fn state_key(identity: &Identity) -> Zeroizing<[u8; 32]> {
    let seed = identity.seed();
    Zeroizing::new(kdf::derive(kdf::label::STATE_KEY, &[&seed[..]]))
}

fn check_version(v: Option<u64>) -> Result<()> {
    match v {
        Some(FILE_VERSION) => Ok(()),
        Some(other) => Err(Error::UnsupportedVersion(other)),
        None => Err(Error::Malformed("file version")),
    }
}

/// Atomically replaces `path` with `bytes`, owner-only permissions.
fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Securely deletes a file by overwriting it with random data before removal.
/// This is a best-effort implementation for spec §16 secure deletion.
/// Note: On SSDs with wear leveling, this may not guarantee physical erasure.
pub fn secure_delete(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    // Get file size
    let metadata = fs::metadata(path)?;
    let size = metadata.len() as usize;

    if size == 0 {
        fs::remove_file(path)?;
        return Ok(());
    }

    // Overwrite with random data (3 passes)
    for _ in 0..3 {
        let random_bytes: Vec<u8> = (0..size).map(|_| rand::random::<u8>()).collect();
        let mut opts = fs::OpenOptions::new();
        opts.write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(path)?;
        f.write_all(&random_bytes)?;
        f.sync_all()?;
    }

    // Final pass with zeros
    let zeros = vec![0u8; size];
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(path)?;
        f.write_all(&zeros)?;
        f.sync_all()?;
    }

    fs::remove_file(path)?;
    Ok(())
}

/// Encrypted backup containing all node state.
/// File format: nonce (12) || ChaCha20-Poly1305(key, nonce, "backup", data)
/// where data is CBOR-encoded map of filename -> encrypted file content.
#[derive(Clone, Debug)]
pub struct Backup {
    pub version: u64,
    pub created_ms: u64,
    /// Map of state file name -> encrypted content
    pub files: Vec<(String, Vec<u8>)>,
}

impl Home {
    /// Exports all node state as an encrypted backup.
    /// Includes identity, contacts, history, prekeys, groups, and all other state.
    /// The backup is encrypted with a key derived from the identity seed.
    pub fn export_backup(&self, identity: &Identity) -> Result<Vec<u8>> {
        use crate::cbor::to_vec;

        let mut files = Vec::new();

        // Collect all state files
        let state_dir = self.dir.join("state");
        if state_dir.exists() {
            for entry in fs::read_dir(&state_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".cbor") || name.ends_with(".cbor.tmp") {
                    continue; // Skip temp files
                }
                let path = entry.path();
                if let Ok(content) = fs::read(&path)
                    && !content.is_empty()
                {
                    files.push((name, content));
                }
            }
        }

        // Also include identity file
        if let Ok(content) = fs::read(self.identity_path()) {
            files.push(("identity.cbor".to_string(), content));
        }

        // Include contacts
        if let Ok(content) = fs::read(self.contacts_path()) {
            files.push(("contacts.cbor".to_string(), content));
        }

        // Include personas list
        let personas_path = self.dir.join("personas.list.cbor");
        if let Ok(content) = fs::read(&personas_path) {
            files.push(("personas.list.cbor".to_string(), content));
        }

        let backup = Backup {
            version: 1,
            created_ms: crate::now_ms(),
            files,
        };

        // Encrypt the backup
        let key = backup_key(identity);
        let nonce: [u8; 12] = crate::crypto::random_bytes();
        let plaintext = to_vec(4096, |e| {
            e.map_len(3)?;
            e.u8(0)?.u64(backup.version)?;
            e.u8(1)?.u64(backup.created_ms)?;
            e.u8(2)?.array_len(backup.files.len())?;
            for (name, content) in &backup.files {
                e.array_len(2)?.str(name)?.bytes(content)?;
            }
            Ok(())
        })?;

        let mut out = nonce.to_vec();
        out.extend(Suite::ChaCha20Poly1305.seal(&key, &nonce, b"backup", &plaintext));
        Ok(out)
    }

    /// Imports a backup, replacing all node state.
    /// The backup must have been created by the same identity.
    pub fn import_backup(&self, identity: &Identity, backup_data: &[u8]) -> Result<()> {
        if backup_data.len() < 12 {
            return Err(Error::Malformed("backup file"));
        }

        let key = backup_key(identity);
        let (nonce, ct) = backup_data.split_at(12);
        let nonce: [u8; 12] = nonce
            .try_into()
            .map_err(|_| Error::Malformed("backup file"))?;
        let plaintext = Suite::ChaCha20Poly1305.open(&key, &nonce, b"backup", ct)?;

        let mut dec = const_cbor::Decoder::new(&plaintext);
        let mut version = None;
        let mut created_ms = None;
        let mut files = Vec::new();

        crate::cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => version = Some(d.u64()?),
                1 => created_ms = Some(d.u64()?),
                2 => {
                    let n = d.array_len()?;
                    for _ in 0..n {
                        d.array_len()?;
                        let name = d.str()?.to_string();
                        let content = d.bytes()?.to_vec();
                        files.push((name, content));
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        crate::cbor::finish(&dec)?;

        if version != Some(1) {
            return Err(Error::UnsupportedVersion(version.unwrap_or(0)));
        }

        // Write all files
        for (name, content) in files {
            if name == "identity.cbor" {
                write_private(&self.dir, &self.identity_path(), &content)?;
            } else if name == "contacts.cbor" {
                write_private(&self.dir, &self.contacts_path(), &content)?;
            } else if name == "personas.list.cbor" {
                let path = self.dir.join("personas.list.cbor");
                write_private(&self.dir, &path, &content)?;
            } else {
                // State files go in state directory
                let path = self.state_path(&name)?;
                write_private(&self.dir, &path, &content)?;
            }
        }

        Ok(())
    }
}

fn backup_key(identity: &Identity) -> Zeroizing<[u8; 32]> {
    let seed = identity.seed();
    Zeroizing::new(crate::crypto::kdf::derive(
        crate::crypto::kdf::label::STATE_KEY,
        &[&seed[..], b"backup"],
    ))
}

/// A known peer. Identity is the key; everything else is local metadata.
/// Previous discovery keys kept per contact (see [`Contact::discovery_older`]).
pub const MAX_OLD_DISCOVERY_KEYS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Contact {
    pub key: PublicIdentity,
    pub petname: Option<String>,
    /// We approve this peer for mesh / tunnel participation.
    pub local_approved: bool,
    /// The peer told us (inside an authenticated session) that it approves us.
    pub remote_approved: bool,
    /// The user confirmed the safety number out of band.
    pub verified: bool,
    pub last_addr: Option<String>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// Pairwise LAN discovery key from the latest mutually approved
    /// session (see `discovery`). Cleared on revocation.
    pub discovery_key: Option<[u8; 32]>,
    /// The previous few discovery keys, still accepted when recognising
    /// beacons. Two sessions that start at once (both sides dialing) can
    /// leave each side with a different "latest" key; keeping recent ones
    /// means both still recognise each other. Cleared on revocation.
    pub discovery_older: Vec<[u8; 32]>,
    /// Account this device belongs to (Appendix J), once its chain is known.
    pub account: Option<AccountId>,
    /// When the approval or verification flags last changed (own-device
    /// sync merges these last-writer-wins).
    pub approval_changed_ms: u64,
    /// We want messages from this peer: we contacted it, wrote to it,
    /// approved it, or accepted its message request. Until then its
    /// messages are requests.
    pub accepted: bool,
    /// Refused: no sessions, no messages.
    pub blocked: bool,
    /// The attributes this peer shares with us (its profile, as it
    /// describes itself; see `persona`).
    pub profile: crate::persona::Profile,
    /// Which of our profile's attributes we share with this peer.
    pub shares: Vec<String>,
    /// The main identity this peer (a persona) proved it belongs to, with
    /// the invite it gave to reach it.
    pub revealed: Option<(PublicIdentity, Option<String>)>,
    /// The user cleared the conversation up to this time: on every one of
    /// our devices, older messages go and don't come back (0 = never).
    pub cleared_ms: u64,
}

impl Contact {
    pub fn new(key: PublicIdentity, now_ms: u64) -> Self {
        Self {
            key,
            petname: None,
            local_approved: false,
            remote_approved: false,
            verified: false,
            last_addr: None,
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            discovery_key: None,
            discovery_older: Vec::new(),
            account: None,
            approval_changed_ms: 0,
            accepted: false,
            blocked: false,
            profile: Vec::new(),
            shares: Vec::new(),
            revealed: None,
            cleared_ms: 0,
        }
    }

    /// Installs the discovery key from a new session, keeping the previous
    /// [`MAX_OLD_DISCOVERY_KEYS`] for recognition.
    pub fn set_discovery_key(&mut self, k: [u8; 32]) {
        if let Some(cur) = self.discovery_key.replace(k)
            && cur != k
        {
            self.discovery_older.retain(|o| *o != cur && *o != k);
            self.discovery_older.insert(0, cur);
            self.discovery_older.truncate(MAX_OLD_DISCOVERY_KEYS);
        }
    }

    /// Forgets every discovery key (revocation, or not ours to share).
    pub fn clear_discovery_keys(&mut self) {
        self.discovery_key = None;
        self.discovery_older.clear();
    }

    /// Keys to recognise this peer's beacons with: current first.
    pub fn recognition_keys(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.discovery_key.iter().chain(&self.discovery_older)
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.key.fingerprint()
    }

    /// Spec §5.2: mesh and tunnel participation need approval on both sides.
    pub fn mutually_approved(&self) -> bool {
        self.local_approved && self.remote_approved
    }

    pub fn label(&self) -> String {
        // A stranger's chosen name isn't shown until they're accepted: it
        // could be anything ("Mom").
        let shared = self
            .shared_name()
            .filter(|_| self.accepted && !self.blocked);
        match self.petname.as_deref().or(shared) {
            Some(n) => format!("{n} ({})", &self.fingerprint().to_string()[..9]),
            None => self.fingerprint().to_string(),
        }
    }

    /// The name this peer shares with us, if it does.
    pub fn shared_name(&self) -> Option<&str> {
        self.profile
            .iter()
            .find(|(k, _)| k == crate::persona::NAME)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Contacts {
    list: Vec<Contact>,
    /// Contacts the user deleted, and when: own-device sync deletes them
    /// on our other devices too, and doesn't bring them back.
    forgotten: Vec<(PublicIdentity, u64)>,
}

/// How long deletions are remembered for sync, and how many.
const FORGET_FOR_MS: u64 = 90 * 24 * 3600 * 1000;
const MAX_FORGOTTEN: usize = 1024;

/// Outcome of a contact lookup by user-supplied text.
pub enum Lookup<'a> {
    Found(&'a Contact),
    None,
    Ambiguous(Vec<&'a Contact>),
}

impl Contacts {
    pub fn iter(&self) -> impl Iterator<Item = &Contact> {
        self.list.iter()
    }

    pub fn get(&self, key: &PublicIdentity) -> Option<&Contact> {
        self.list.iter().find(|c| &c.key == key)
    }

    pub fn get_mut(&mut self, key: &PublicIdentity) -> Option<&mut Contact> {
        self.list.iter_mut().find(|c| &c.key == key)
    }

    /// Records a sighting, creating the contact on first use (TOFU, §5.2).
    /// Returns true when the contact is new.
    pub fn observe(&mut self, key: PublicIdentity, addr: Option<String>, now_ms: u64) -> bool {
        if let Some(c) = self.get_mut(&key) {
            c.last_seen_ms = now_ms;
            if addr.is_some() {
                c.last_addr = addr;
            }
            false
        } else {
            let mut c = Contact::new(key, now_ms);
            c.last_addr = addr;
            self.list.push(c);
            true
        }
    }

    pub fn remove(&mut self, key: &PublicIdentity) -> bool {
        let before = self.list.len();
        self.list.retain(|c| &c.key != key);
        before != self.list.len()
    }

    /// Deletes a contact the user chose to delete, remembering that so our
    /// other devices delete it too. If it reaches us again later, it comes
    /// back as a new contact.
    pub fn forget(&mut self, key: &PublicIdentity, now_ms: u64) -> bool {
        self.note_forgotten(*key, now_ms);
        self.prune_forgotten(now_ms);
        self.remove(key)
    }

    /// When `key` was last deleted, if within the remembered window.
    pub fn forgotten_at(&self, key: &PublicIdentity) -> Option<u64> {
        self.forgotten
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, t)| *t)
    }

    fn note_forgotten(&mut self, key: PublicIdentity, at: u64) {
        match self.forgotten.iter_mut().find(|(k, _)| *k == key) {
            Some(f) => f.1 = f.1.max(at),
            None => self.forgotten.push((key, at)),
        }
    }

    fn prune_forgotten(&mut self, now_ms: u64) {
        self.forgotten
            .retain(|(_, t)| now_ms.saturating_sub(*t) < FORGET_FOR_MS);
        if self.forgotten.len() > MAX_FORGOTTEN {
            self.forgotten.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
            self.forgotten.truncate(MAX_FORGOTTEN);
        }
    }

    /// Whether a contact first seen at `first_seen_ms` predates its deletion.
    fn deleted(&self, key: &PublicIdentity, first_seen_ms: u64) -> bool {
        self.forgotten_at(key).is_some_and(|t| first_seen_ms <= t)
    }

    /// Finds a contact by exact petname, else by fingerprint prefix.
    pub fn find(&self, query: &str) -> Lookup<'_> {
        if let Some(c) = self
            .list
            .iter()
            .find(|c| c.petname.as_deref() == Some(query))
        {
            return Lookup::Found(c);
        }
        let hits: Vec<_> = self
            .list
            .iter()
            .filter(|c| fingerprint_matches_prefix(&c.fingerprint(), query))
            .collect();
        match hits.len() {
            0 => Lookup::None,
            1 => Lookup::Found(hits[0]),
            _ => Lookup::Ambiguous(hits),
        }
    }

    /// A snapshot for own-device sync: per-device secrets and the peer's
    /// approval of *this* device are not meaningful elsewhere, so they are
    /// stripped.
    pub fn sync_snapshot(&self) -> Result<Vec<u8>> {
        let mut c = self.clone();
        for x in &mut c.list {
            x.clear_discovery_keys();
            x.remote_approved = false;
        }
        c.encode()
    }

    /// Merges a snapshot from one of our own devices. Approval and
    /// verification are last-writer-wins on `approval_changed_ms`; names,
    /// addresses and accounts fill gaps. Returns true if anything changed.
    pub fn merge_snapshot(&mut self, bytes: &[u8], now_ms: u64) -> Result<bool> {
        let other = Self::decode(bytes)?;
        let mut changed = false;
        // Deletions first: on our other device, the user deleted these.
        for (k, t) in other.forgotten {
            if self.forgotten_at(&k).is_none_or(|ours| ours < t) {
                self.note_forgotten(k, t);
            }
        }
        self.prune_forgotten(now_ms);
        let before = self.list.len();
        let gone: Vec<PublicIdentity> = self
            .list
            .iter()
            .filter(|c| self.deleted(&c.key, c.first_seen_ms))
            .map(|c| c.key)
            .collect();
        self.list.retain(|c| !gone.contains(&c.key));
        changed |= self.list.len() != before;
        for o in other.list {
            if self.deleted(&o.key, o.first_seen_ms) {
                continue;
            }
            match self.get_mut(&o.key) {
                None => {
                    let mut c = o;
                    c.clear_discovery_keys();
                    c.remote_approved = false;
                    c.first_seen_ms = now_ms;
                    self.list.push(c);
                    changed = true;
                }
                Some(c) => {
                    if o.approval_changed_ms > c.approval_changed_ms {
                        c.local_approved = o.local_approved;
                        c.verified = o.verified;
                        c.approval_changed_ms = o.approval_changed_ms;
                        changed = true;
                    }
                    if c.petname.is_none() && o.petname.is_some() {
                        c.petname = o.petname;
                        changed = true;
                    }
                    if c.last_addr.is_none() && o.last_addr.is_some() {
                        c.last_addr = o.last_addr;
                        changed = true;
                    }
                    if c.account.is_none() && o.account.is_some() {
                        c.account = o.account;
                        changed = true;
                    }
                    // Accepting or blocking on one device does on all.
                    if o.accepted && !c.accepted {
                        c.accepted = true;
                        changed = true;
                    }
                    if o.blocked && !c.blocked {
                        c.blocked = true;
                        changed = true;
                    }
                    // Clearing a chat on one device clears it on all.
                    if o.cleared_ms > c.cleared_ms {
                        c.cleared_ms = o.cleared_ms;
                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(
            64 + self.list.len() * 160 + self.forgotten.len() * 48,
            |e| {
                e.map_len(2 + usize::from(!self.forgotten.is_empty()))?;
                e.u8(0)?.uint(FILE_VERSION)?;
                e.u8(1)?.array_len(self.list.len())?;
                for c in &self.list {
                    let n = 6
                        + usize::from(c.petname.is_some())
                        + usize::from(c.last_addr.is_some())
                        + usize::from(c.discovery_key.is_some())
                        + usize::from(!c.discovery_older.is_empty())
                        + usize::from(c.account.is_some())
                        + usize::from(c.blocked)
                        + usize::from(!c.profile.is_empty())
                        + usize::from(!c.shares.is_empty())
                        + usize::from(c.revealed.is_some())
                        + usize::from(c.revealed.as_ref().is_some_and(|r| r.1.is_some()))
                        + usize::from(c.cleared_ms != 0)
                        + 2;
                    e.map_len(n)?;
                    e.u8(0)?.bytes(c.key.as_bytes())?;
                    if let Some(p) = &c.petname {
                        e.u8(1)?.str(p)?;
                    }
                    e.u8(2)?.bool(c.local_approved)?;
                    e.u8(3)?.bool(c.remote_approved)?;
                    e.u8(4)?.bool(c.verified)?;
                    if let Some(a) = &c.last_addr {
                        e.u8(5)?.str(a)?;
                    }
                    e.u8(6)?.u64(c.first_seen_ms)?;
                    e.u8(7)?.u64(c.last_seen_ms)?;
                    if let Some(k) = &c.discovery_key {
                        e.u8(8)?.bytes(k)?;
                    }
                    if let Some(a) = &c.account {
                        e.u8(9)?.bytes(&a.0)?;
                    }
                    e.u8(10)?.u64(c.approval_changed_ms)?;
                    if !c.discovery_older.is_empty() {
                        e.u8(11)?.array_len(c.discovery_older.len())?;
                        for k in &c.discovery_older {
                            e.bytes(k)?;
                        }
                    }
                    e.u8(12)?.bool(c.accepted)?;
                    if c.blocked {
                        e.u8(13)?.bool(true)?;
                    }
                    if !c.profile.is_empty() {
                        e.u8(14)?.array_len(c.profile.len())?;
                        for (k, v) in &c.profile {
                            e.array_len(2)?.str(k)?.str(v)?;
                        }
                    }
                    if !c.shares.is_empty() {
                        e.u8(15)?.array_len(c.shares.len())?;
                        for k in &c.shares {
                            e.str(k)?;
                        }
                    }
                    if let Some((id, invite)) = &c.revealed {
                        e.u8(16)?.bytes(id.as_bytes())?;
                        if let Some(i) = invite {
                            e.u8(17)?.str(i)?;
                        }
                    }
                    if c.cleared_ms != 0 {
                        e.u8(18)?.u64(c.cleared_ms)?;
                    }
                }
                if !self.forgotten.is_empty() {
                    e.u8(2)?.array_len(self.forgotten.len())?;
                    for (k, t) in &self.forgotten {
                        e.array_len(2)?.bytes(k.as_bytes())?.u64(*t)?;
                    }
                }
                Ok(())
            },
        )
    }

    pub(crate) fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut ver, mut list, mut forgotten) = (None, Vec::new(), Vec::new());
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    for _ in 0..d.array_len()? {
                        list.push(decode_contact(d)?);
                    }
                }
                2 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 2 {
                            return Err(Error::Malformed("forgotten contact"));
                        }
                        let k = PublicIdentity::from_bytes(&fixed_bytes::<32>(d)?)?;
                        let t = d.u64()?;
                        if forgotten.len() < MAX_FORGOTTEN {
                            forgotten.push((k, t));
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        check_version(ver)?;
        Ok(Self { list, forgotten })
    }
}

fn decode_contact(d: &mut Decoder<'_>) -> Result<Contact> {
    let mut key = None;
    let mut c = Contact {
        key: Identity::from_seed(&[0; 32]).public(),
        petname: None,
        local_approved: false,
        remote_approved: false,
        verified: false,
        last_addr: None,
        first_seen_ms: 0,
        last_seen_ms: 0,
        discovery_key: None,
        discovery_older: Vec::new(),
        account: None,
        approval_changed_ms: 0,
        // Contacts saved before message requests existed were wanted.
        accepted: true,
        blocked: false,
        profile: Vec::new(),
        shares: Vec::new(),
        revealed: None,
        cleared_ms: 0,
    };
    let (mut revealed, mut invite) = (None, None);
    read_map(d, |k, d| {
        match k {
            0 => key = Some(fixed_bytes::<32>(d)?),
            1 => c.petname = Some(d.str()?.to_owned()),
            2 => c.local_approved = d.bool()?,
            3 => c.remote_approved = d.bool()?,
            4 => c.verified = d.bool()?,
            5 => c.last_addr = Some(d.str()?.to_owned()),
            6 => c.first_seen_ms = d.u64()?,
            7 => c.last_seen_ms = d.u64()?,
            8 => c.discovery_key = Some(fixed_bytes::<32>(d)?),
            9 => c.account = Some(AccountId(fixed_bytes::<32>(d)?)),
            10 => c.approval_changed_ms = d.u64()?,
            12 => c.accepted = d.bool()?,
            13 => c.blocked = d.bool()?,
            14 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("profile attribute"));
                    }
                    let kv = (d.str()?.to_owned(), d.str()?.to_owned());
                    if c.profile.len() < crate::persona::MAX_ATTRIBUTES {
                        c.profile.push(kv);
                    }
                }
            }
            15 => {
                for _ in 0..d.array_len()? {
                    let k = d.str()?.to_owned();
                    if c.shares.len() < crate::persona::MAX_ATTRIBUTES {
                        c.shares.push(k);
                    }
                }
            }
            16 => revealed = Some(fixed_bytes::<32>(d)?),
            17 => invite = Some(d.str()?.to_owned()),
            18 => c.cleared_ms = d.u64()?,
            11 => {
                for _ in 0..d.array_len()? {
                    let k = fixed_bytes::<32>(d)?;
                    if c.discovery_older.len() < MAX_OLD_DISCOVERY_KEYS {
                        c.discovery_older.push(k);
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    c.key = PublicIdentity::from_bytes(&required(key, "contact key")?)?;
    if let Some(r) = revealed {
        c.revealed = Some((PublicIdentity::from_bytes(&r)?, invite));
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_contacts_persist() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("t"));
        assert!(!home.has_identity());
        let id = home.create_identity(None).unwrap();
        assert_eq!(home.load_identity(None).unwrap().public(), id.public());

        let peer = Identity::generate().public();
        let mut c = home.load_contacts().unwrap();
        assert!(c.observe(peer, Some("10.0.0.1:7450".into()), 5));
        assert!(!c.observe(peer, None, 6));
        let ct = c.get_mut(&peer).unwrap();
        ct.petname = Some("alice".into());
        ct.local_approved = true;
        ct.discovery_key = Some([4; 32]);
        ct.account = Some(AccountId([5; 32]));
        ct.approval_changed_ms = 77;
        ct.blocked = true;
        assert!(!ct.accepted, "new contacts start as message requests");
        home.save_contacts(&c).unwrap();
        let loaded = home.load_contacts().unwrap();
        assert_eq!(loaded, c);
        assert!(matches!(loaded.find("alice"), Lookup::Found(_)));
        let fp = peer.fingerprint().to_string();
        assert!(matches!(loaded.find(&fp[..4]), Lookup::Found(_)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(home.identity_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn snapshots_merge_last_writer_wins_without_pairwise_secrets() {
        let (bob, carol) = (Identity::generate().public(), Identity::generate().public());
        let mut mine = Contacts::default();
        mine.observe(bob, None, 1);
        let c = mine.get_mut(&bob).unwrap();
        c.local_approved = true;
        c.approval_changed_ms = 10;
        c.discovery_key = Some([1; 32]);

        let mut theirs = Contacts::default();
        theirs.observe(bob, Some("bob.example:1".into()), 1);
        theirs.observe(carol, None, 1);
        let t = theirs.get_mut(&bob).unwrap();
        t.local_approved = false; // revoked later on the other device
        t.approval_changed_ms = 20;
        t.petname = Some("bob".into());
        t.remote_approved = true;
        theirs.get_mut(&carol).unwrap().discovery_key = Some([9; 32]);

        assert!(
            mine.merge_snapshot(&theirs.sync_snapshot().unwrap(), 5)
                .unwrap()
        );
        let b = mine.get(&bob).unwrap();
        assert!(!b.local_approved, "later revocation wins");
        assert_eq!(b.petname.as_deref(), Some("bob"));
        assert_eq!(b.discovery_key, Some([1; 32]), "own pairwise key kept");
        let c = mine.get(&carol).unwrap();
        assert!(
            c.discovery_key.is_none() && !c.remote_approved,
            "pairwise state not copied"
        );
        assert!(
            !mine
                .merge_snapshot(&theirs.sync_snapshot().unwrap(), 6)
                .unwrap(),
            "idempotent"
        );
    }
}

#[cfg(test)]
mod state_tests {
    use super::*;

    #[test]
    fn state_round_trips_and_is_bound_to_identity_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let a = Identity::generate();
        assert!(home.load_state(&a, "groups").unwrap().is_none());
        home.save_state(&a, "groups", b"secret state").unwrap();
        assert_eq!(
            &home.load_state(&a, "groups").unwrap().unwrap()[..],
            b"secret state"
        );
        let raw = fs::read(dir.path().join("groups.state")).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"));
        assert!(home.load_state(&Identity::generate(), "groups").is_err());
        fs::copy(
            dir.path().join("groups.state"),
            dir.path().join("other.state"),
        )
        .unwrap();
        assert!(
            home.load_state(&a, "other").is_err(),
            "state must be bound to its name"
        );
        assert!(home.save_state(&a, "../x", b"").is_err());
    }
}

#[cfg(test)]
mod passphrase_tests {
    use super::*;
    use crate::sealed::TEST_PARAMS;

    #[test]
    fn sealed_identity_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path());
        let id = home
            .create_identity_with(Some(b"hunter2"), TEST_PARAMS)
            .unwrap();
        assert!(home.identity_is_sealed().unwrap());
        assert!(matches!(
            home.load_identity(None),
            Err(Error::PassphraseRequired)
        ));
        assert!(matches!(
            home.load_identity(Some(b"wrong")),
            Err(Error::Decrypt)
        ));
        assert_eq!(
            home.load_identity(Some(b"hunter2")).unwrap().public(),
            id.public()
        );
        let raw = fs::read(home.identity_path()).unwrap();
        assert!(
            !raw.windows(32).any(|w| w == &id.seed()[..]),
            "seed stored in clear"
        );

        home.change_passphrase(Some(b"hunter2"), None).unwrap();
        assert!(!home.identity_is_sealed().unwrap());
        assert_eq!(home.load_identity(None).unwrap().public(), id.public());
    }

    #[test]
    fn recent_discovery_keys_are_kept_and_cleared_together() {
        let mut c = Contact::new(Identity::generate().public(), 0);
        for k in 1..=4u8 {
            c.set_discovery_key([k; 32]);
        }
        c.set_discovery_key([4; 32]);
        assert_eq!(c.discovery_key, Some([4; 32]));
        assert_eq!(c.discovery_older, vec![[3; 32], [2; 32]]);
        assert_eq!(c.recognition_keys().count(), 3);
        let mut book = Contacts::default();
        book.list.push(c.clone());
        let back = Contacts::decode(&book.encode().unwrap()).unwrap();
        assert_eq!(back.list[0], c);
        c.clear_discovery_keys();
        assert_eq!(c.recognition_keys().count(), 0);
    }

    #[test]
    fn deleted_contacts_stay_deleted_across_devices() {
        let x = Identity::generate().public();
        let y = Identity::generate().public();
        let (mut phone, mut laptop) = (Contacts::default(), Contacts::default());
        for c in [&mut phone, &mut laptop] {
            c.observe(x, None, 100);
            c.observe(y, None, 100);
        }
        // Deleted on the phone; the laptop follows, and keeps nothing.
        assert!(phone.forget(&x, 200));
        assert!(phone.get(&x).is_none() && phone.forgotten_at(&x) == Some(200));
        assert!(
            laptop
                .merge_snapshot(&phone.sync_snapshot().unwrap(), 300)
                .unwrap()
        );
        assert!(laptop.get(&x).is_none() && laptop.get(&y).is_some());
        // An older snapshot from a device that still had it doesn't bring it back.
        let mut stale = Contacts::default();
        stale.observe(x, None, 100);
        assert!(
            !phone
                .merge_snapshot(&stale.sync_snapshot().unwrap(), 300)
                .unwrap()
        );
        assert!(phone.get(&x).is_none());
        // They write again later: a new contact, which syncs normally.
        assert!(phone.observe(x, None, 400));
        assert!(
            laptop
                .merge_snapshot(&phone.sync_snapshot().unwrap(), 500)
                .unwrap()
        );
        assert!(laptop.get(&x).is_some());
        // Deletions survive encoding, and are forgotten after 90 days.
        let mut c = Contacts::decode(&phone.encode().unwrap()).unwrap();
        assert_eq!(c.forgotten_at(&x), Some(200));
        c.forget(&y, 200 + FORGET_FOR_MS + 1);
        assert_eq!(c.forgotten_at(&x), None);
        assert!(c.forgotten_at(&y).is_some());
    }
}
