//! Keeps identities' passphrases in the Secret Service (GNOME Keyring,
//! KWallet, KeePassXC), exactly as the CLI does: one random passphrase per
//! data directory, under service "threnody". A home the CLI sealed opens
//! here without a prompt, and the other way round.

use std::path::Path;

const SERVICE: &str = "threnody";

/// A passphrase that is wiped from memory on drop.
pub struct Secret(pub String);

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

fn entry(home: &Path) -> Result<keyring::Entry, String> {
    let dir =
        std::path::absolute(home).map_err(|e| format!("resolving {}: {e}", home.display()))?;
    keyring::Entry::new(SERVICE, &dir.display().to_string())
        .map_err(|e| format!("no keyring available: {e}"))
}

/// The stored passphrase for `home`, if the keyring has one.
pub fn get(home: &Path) -> Option<Secret> {
    entry(home).ok()?.get_password().ok().map(Secret)
}

/// Makes a random passphrase for `home` and stores it, replacing any old one.
pub fn create(home: &Path) -> Result<Secret, String> {
    let pw = random();
    store(home, &pw)?;
    Ok(pw)
}

/// A random passphrase, for [`store`].
pub fn random() -> Secret {
    let bytes: [u8; 32] = threnody_core::crypto::random_bytes();
    Secret(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Stores `pw` as `home`'s passphrase, replacing any old one.
pub fn store(home: &Path, pw: &Secret) -> Result<(), String> {
    let e = entry(home)?;
    e.set_password(&pw.0)
        .map_err(|e| format!("storing the key in the keyring: {e}"))?;
    // Only rely on it once it reads back.
    match e.get_password() {
        Ok(back) if back == pw.0 => Ok(()),
        _ => Err("the keyring did not keep the key".into()),
    }
}

/// The keyring account of `home`'s app-lock pepper: its own entry, so
/// re-keying the identity (`threnody keyring`) can't orphan the lock file.
fn lock_account(abs: &Path) -> String {
    format!("{} app-lock", abs.display())
}

fn lock_entry(home: &Path) -> Result<keyring::Entry, String> {
    let dir =
        std::path::absolute(home).map_err(|e| format!("resolving {}: {e}", home.display()))?;
    keyring::Entry::new(SERVICE, &lock_account(&dir))
        .map_err(|e| format!("no keyring available: {e}"))
}

/// The pepper that keys `home`'s app-lock file (Appendix C, *App lock*):
/// read, or with `create` made when there is none.
pub fn lock_pepper(home: &Path, create: bool) -> Option<Secret> {
    let e = lock_entry(home).ok()?;
    if let Ok(p) = e.get_password() {
        return Some(Secret(p));
    }
    if !create {
        return None;
    }
    let pw = random();
    e.set_password(&pw.0).ok()?;
    // Only rely on it once it reads back.
    (e.get_password().ok()? == pw.0).then_some(pw)
}

/// Forgets `home`'s app-lock pepper (no error if there is none).
pub fn delete_lock_pepper(home: &Path) {
    if let Ok(e) = lock_entry(home) {
        let _ = e.delete_credential();
    }
}

/// Forgets the keyring entry for `home` (no error if there is none).
pub fn delete(home: &Path) {
    if let Ok(e) = entry(home) {
        let _ = e.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_pepper_account_differs_from_the_identity_account() {
        let abs = Path::new("/home/u/.local/share/threnody");
        assert_eq!(lock_account(abs), "/home/u/.local/share/threnody app-lock");
        assert_ne!(lock_account(abs), abs.display().to_string());
    }
}
