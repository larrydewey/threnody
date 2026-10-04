//! Keeps the identity's passphrase in the platform keyring (spec §3.1): the
//! Secret Service on Linux (GNOME Keyring, KWallet, KeePassXC), the
//! Keychain on macOS, the Credential Manager on Windows.
//!
//! As on Android, the identity file is sealed (Argon2id) with a random
//! passphrase, and only the keyring holds that passphrase. Copying the
//! data directory then gets an attacker nothing without the user's
//! unlocked keyring. One entry per data directory.

use anyhow::{Context, Result, anyhow};
use threnody_core::store::Home;

use crate::zeroize_string::Secret;

const SERVICE: &str = "threnody";

fn entry(home: &Home) -> Result<keyring::Entry> {
    let dir = std::path::absolute(home.dir()).context("resolving the data directory")?;
    keyring::Entry::new(SERVICE, &dir.display().to_string())
        .map_err(|e| anyhow!("no keyring available: {e}"))
}

/// The stored passphrase for `home`, if the keyring has one.
pub fn get(home: &Home) -> Option<Secret> {
    entry(home).ok()?.get_password().ok().map(Secret)
}

/// Makes a random passphrase for `home` and stores it, replacing any old one.
pub fn create(home: &Home) -> Result<Secret> {
    let e = entry(home)?;
    let bytes: [u8; 32] = threnody_core::crypto::random_bytes();
    let pw = Secret(bytes.iter().map(|b| format!("{b:02x}")).collect());
    e.set_password(&pw.0)
        .map_err(|e| anyhow!("storing the key in the keyring: {e}"))?;
    // Only rely on it once it reads back.
    match e.get_password() {
        Ok(back) if back == pw.0 => Ok(pw),
        _ => Err(anyhow!("the keyring did not keep the key")),
    }
}

/// Forgets the keyring entry for `home` (no error if there is none).
pub fn delete(home: &Home) {
    if let Ok(e) = entry(home) {
        let _ = e.delete_credential();
    }
}
