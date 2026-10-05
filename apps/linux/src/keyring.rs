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
    let e = entry(home)?;
    let bytes: [u8; 32] = threnody_core::crypto::random_bytes();
    let pw = Secret(bytes.iter().map(|b| format!("{b:02x}")).collect());
    e.set_password(&pw.0)
        .map_err(|e| format!("storing the key in the keyring: {e}"))?;
    // Only rely on it once it reads back.
    match e.get_password() {
        Ok(back) if back == pw.0 => Ok(pw),
        _ => Err("the keyring did not keep the key".into()),
    }
}

/// Forgets the keyring entry for `home` (no error if there is none).
pub fn delete(home: &Path) {
    if let Ok(e) = entry(home) {
        let _ = e.delete_credential();
    }
}
