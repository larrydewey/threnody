//! App lock and erasing (`threnody_core::applock`, `threnody_core::wipe`)
//! for the apps: the lock file's settings, checking entries on the lock
//! screen, the lock timer, and erasing a data directory.
//!
//! `pepper` is the platform's keystore-held secret (Appendix C, *App
//! lock*): Android's and iOS's `KeyVault` secret, the Linux keyring's
//! app-lock entry, or `None` where there is none.

use std::sync::{Arc, Mutex};

use threnody_core::applock::{LockClock, LockPolicy, SecretKind, Verdict};
use threnody_core::store::Home;

use crate::{Result, ThrenodyError, fail};

/// What an app lock secret is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LockKind {
    /// A PIN: 4 to 16 digits.
    Digits,
    /// A passphrase: at least 6 characters.
    Passphrase,
}

impl From<LockKind> for SecretKind {
    fn from(k: LockKind) -> Self {
        match k {
            LockKind::Digits => SecretKind::Digits,
            LockKind::Passphrase => SecretKind::Passphrase,
        }
    }
}

impl From<SecretKind> for LockKind {
    fn from(k: SecretKind) -> Self {
        match k {
            SecretKind::Digits => LockKind::Digits,
            SecretKind::Passphrase => LockKind::Passphrase,
        }
    }
}

/// The app lock's settings, as a settings screen shows them. Never
/// carries a secret or its verifier.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LockSettings {
    pub enabled: bool,
    /// None: the device's own lock only.
    pub kind: Option<LockKind>,
    /// Unlock with biometrics (or, without `kind`, the device lock).
    pub biometric: bool,
    /// Time away before it locks; 0 = on leaving.
    pub away_secs: u32,
    /// Idle time in front before it locks; None = never.
    pub idle_secs: Option<u32>,
    /// A duress PIN is set.
    pub duress: bool,
    /// Erase everything after 10 wrong entries in a row.
    pub erase_after_failures: bool,
    /// Wall-clock ms before which entries are refused (0: none).
    pub retry_at_ms: u64,
}

impl LockSettings {
    /// App lock off.
    pub fn off() -> Self {
        Self {
            enabled: false,
            kind: None,
            biometric: false,
            away_secs: LockPolicy::DEFAULT.away_secs,
            idle_secs: LockPolicy::DEFAULT.idle_secs,
            duress: false,
            erase_after_failures: false,
            retry_at_ms: 0,
        }
    }
}

/// The result of an entry on the lock screen.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum LockOutcome {
    Unlocked,
    /// Wrong; entries are refused until `retry_at_ms` (wall clock, 0: none).
    Wrong {
        retry_at_ms: u64,
    },
    /// Refused unchecked: too many wrong entries, wait until `retry_at_ms`.
    Wait {
        retry_at_ms: u64,
    },
    /// Everything is erased (the duress PIN, or the last of 10 wrong
    /// entries when that is on). `homes` are the identity homes removed,
    /// for keyring cleanup. Show nothing; reset and restart.
    Erased {
        homes: Vec<String>,
    },
}

fn pepper_of(p: &Option<String>) -> Option<&[u8]> {
    p.as_deref().map(str::as_bytes)
}

fn lock_error(e: threnody_core::Error) -> ThrenodyError {
    match e {
        threnody_core::Error::WrongPin => fail("wrong PIN"),
        threnody_core::Error::Decrypt => fail("app lock settings can't be read"),
        e => fail(e),
    }
}

fn policy(
    biometric: bool,
    away_secs: u32,
    idle_secs: Option<u32>,
    erase_after_failures: bool,
) -> LockPolicy {
    LockPolicy {
        biometric,
        away_secs,
        idle_secs,
        erase_after_failures,
    }
}

fn paths(homes: Vec<std::path::PathBuf>) -> Vec<String> {
    homes.into_iter().map(|h| h.display().to_string()).collect()
}

/// The app lock's settings in `home` (`enabled: false` when it is off).
/// Fails when the lock file can't be read with this pepper: the app then
/// stays locked and offers only to erase everything.
#[uniffi::export]
pub fn app_lock(home: String, pepper: Option<String>) -> Result<LockSettings> {
    let view = Home::new(home)
        .app_lock(pepper_of(&pepper))
        .map_err(lock_error)?;
    Ok(match view {
        None => LockSettings::off(),
        Some(v) => LockSettings {
            enabled: true,
            kind: v.secret.map(LockKind::from),
            biometric: v.policy.biometric,
            away_secs: v.policy.away_secs,
            idle_secs: v.policy.idle_secs,
            duress: v.duress,
            erase_after_failures: v.policy.erase_after_failures,
            retry_at_ms: v.retry_at_ms,
        },
    })
}

/// Whether app lock is on in `home`: its lock file exists. Needs no
/// pepper and does no work, so apps can ask it synchronously (before
/// posting a notification, when a screen starts) and fail closed.
#[uniffi::export]
pub fn app_lock_enabled(home: String) -> bool {
    Home::new(home).app_lock_enabled()
}

/// Turns app lock on, or changes its secret: `secret` of `kind`, or
/// neither for the device's own lock (then `biometric` must be true).
/// `current` must be the existing secret when one is set. Keeps a duress
/// PIN unless the secret is removed (it goes too).
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn set_app_lock(
    home: String,
    pepper: Option<String>,
    current: Option<String>,
    secret: Option<String>,
    kind: Option<LockKind>,
    biometric: bool,
    away_secs: u32,
    idle_secs: Option<u32>,
    erase_after_failures: bool,
) -> Result<()> {
    let secret = match (&secret, kind) {
        (Some(s), Some(k)) => Some((s.as_str(), SecretKind::from(k))),
        (None, None) => None,
        _ => return Err(fail("a secret needs its kind")),
    };
    Home::new(home)
        .set_app_lock(
            pepper_of(&pepper),
            current.as_deref(),
            secret,
            policy(biometric, away_secs, idle_secs, erase_after_failures),
        )
        .map_err(lock_error)
}

/// Changes only when the app locks and whether biometrics unlock it. In
/// device-lock mode the app must run the device's own authentication
/// before loosening it: this can't check that.
#[uniffi::export]
pub fn set_lock_policy(
    home: String,
    pepper: Option<String>,
    biometric: bool,
    away_secs: u32,
    idle_secs: Option<u32>,
    erase_after_failures: bool,
) -> Result<()> {
    Home::new(home)
        .set_lock_policy(
            pepper_of(&pepper),
            policy(biometric, away_secs, idle_secs, erase_after_failures),
        )
        .map_err(lock_error)
}

/// Turns app lock off (deletes the lock file). `current` must be the
/// secret when one is set; in device-lock mode the app runs the device's
/// own authentication first.
#[uniffi::export]
pub fn clear_app_lock(home: String, pepper: Option<String>, current: Option<String>) -> Result<()> {
    Home::new(home)
        .clear_app_lock(pepper_of(&pepper), current.as_deref())
        .map_err(lock_error)
}

/// Sets (`duress` Some) or removes the duress PIN; needs the lock secret.
#[uniffi::export]
pub fn set_duress_pin(
    home: String,
    pepper: Option<String>,
    current: String,
    duress: Option<String>,
) -> Result<()> {
    Home::new(home)
        .set_duress(pepper_of(&pepper), &current, duress.as_deref())
        .map_err(lock_error)
}

/// Checks an entry on the lock screen. A duress PIN erases `home` before
/// this returns (`Erased`); the app then removes what it keeps elsewhere
/// and restarts. Fails in device-lock mode (no PIN is set).
#[uniffi::export]
pub fn unlock_app(home: String, pepper: Option<String>, entered: String) -> Result<LockOutcome> {
    let verdict = Home::new(home)
        .check_lock(pepper_of(&pepper), &entered, threnody_core::now_ms())
        .map_err(lock_error)?;
    Ok(match verdict {
        Verdict::Unlock => LockOutcome::Unlocked,
        Verdict::Wrong { retry_at_ms } => LockOutcome::Wrong { retry_at_ms },
        Verdict::Wait { retry_at_ms } => LockOutcome::Wait { retry_at_ms },
        Verdict::Duress(e) | Verdict::Exhausted(e) => LockOutcome::Erased {
            homes: paths(e.homes),
        },
    })
}

/// A biometric (or device-lock) unlock succeeded: clears the count of
/// wrong entries.
#[uniffi::export]
pub fn biometric_unlocked(home: String, pepper: Option<String>) -> Result<()> {
    Home::new(home)
        .lock_passed(pepper_of(&pepper))
        .map_err(lock_error)
}

/// Whether `secret` is acceptable as a `kind` lock secret, for checking
/// as the user types.
#[uniffi::export]
pub fn lock_secret_ok(kind: LockKind, secret: String) -> bool {
    threnody_core::applock::secret_ok(&secret, kind.into())
}

/// Erases `home`: identity, contacts, history, personas, received files
/// kept inside it, the lock file and a staged restore (`Home::erase`).
/// Safe with a node still open there: it can't write any more. Returns
/// the identity homes removed (main first), whose keyring entries the
/// app should delete too.
#[uniffi::export]
pub fn wipe_home(home: String) -> Result<Vec<String>> {
    Home::new(home)
        .erase()
        .map(|e| paths(e.homes))
        .map_err(fail)
}

/// The lock timer: feed it what the app does, ask it whether to lock.
/// Pass a monotonic clock in ms (one that counts sleep where there is
/// one); a clock going backwards locks.
#[derive(uniffi::Object)]
pub struct AppLockClock(Mutex<LockClock>);

impl AppLockClock {
    fn with<T>(&self, f: impl FnOnce(&mut LockClock) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[uniffi::export]
impl AppLockClock {
    #[uniffi::constructor]
    pub fn new(
        locked: bool,
        biometric: bool,
        away_secs: u32,
        idle_secs: Option<u32>,
        now_ms: u64,
    ) -> Arc<Self> {
        Arc::new(Self(Mutex::new(LockClock::new(
            locked,
            policy(biometric, away_secs, idle_secs, false),
            now_ms,
        ))))
    }

    pub fn set_policy(&self, biometric: bool, away_secs: u32, idle_secs: Option<u32>) {
        self.with(|c| c.set_policy(policy(biometric, away_secs, idle_secs, false)));
    }

    /// User input in front.
    pub fn activity(&self, now_ms: u64) {
        self.with(|c| c.activity(now_ms));
    }

    /// The app went to the background or was hidden.
    pub fn left(&self, now_ms: u64) {
        self.with(|c| c.left(now_ms));
    }

    /// The app opened its own picker or camera: a few minutes' grace.
    pub fn left_expecting_return(&self, now_ms: u64) {
        self.with(|c| c.left_expecting_return(now_ms));
    }

    /// The app came back; whether it is locked.
    pub fn returned(&self, now_ms: u64) -> bool {
        self.with(|c| c.returned(now_ms))
    }

    /// Periodic check; whether it is locked.
    pub fn tick(&self, now_ms: u64) -> bool {
        self.with(|c| c.tick(now_ms))
    }

    pub fn unlocked(&self, now_ms: u64) {
        self.with(|c| c.unlocked(now_ms));
    }

    pub fn lock_now(&self) {
        self.with(|c| c.lock_now());
    }

    pub fn is_locked(&self) -> bool {
        self.with(|c| c.is_locked())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ThrenodyNode;

    fn s(p: &std::path::Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn wipe_home_while_a_node_runs_leaves_nothing() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("node");
        let node = ThrenodyNode::open(s(&dir), None, None).unwrap();
        let before = node.device_fingerprint();
        node.create_persona("p".into(), None, None).unwrap();
        node.write_note("hello".into(), None, None).unwrap();

        let homes = wipe_home(s(&dir)).unwrap();
        assert_eq!(homes.len(), 2);
        assert_eq!(homes[0], s(&dir));
        assert!(!dir.exists());
        let _ = node.write_note("again".into(), None, None);
        assert!(node.create_persona("q".into(), None, None).is_err());
        assert!(!dir.exists());
        node.shutdown();

        let fresh = ThrenodyNode::open(s(&dir), None, None).unwrap();
        assert_ne!(fresh.device_fingerprint(), before);
        assert!(fresh.contacts().is_empty());
        fresh.shutdown();
    }

    #[test]
    fn open_after_an_interrupted_erase_starts_fresh() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("node");
        let node = ThrenodyNode::open(s(&dir), None, None).unwrap();
        node.write_note("hello".into(), None, None).unwrap();
        node.shutdown();
        drop(node);
        std::fs::remove_file(dir.join("identity.cbor")).unwrap();
        let again = ThrenodyNode::open(s(&dir), None, None).unwrap();
        assert!(again.note_history(10).unwrap().is_empty());
        again.shutdown();
    }

    #[test]
    fn unlock_app_reports_erased_for_the_duress_pin() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        Home::new(&dir).create_identity(None).unwrap();
        let (home, pepper) = (s(&dir), Some("pepper".to_string()));
        set_app_lock(
            home.clone(),
            pepper.clone(),
            None,
            Some("2468".into()),
            Some(LockKind::Digits),
            false,
            0,
            Some(300),
            false,
        )
        .unwrap();
        set_duress_pin(
            home.clone(),
            pepper.clone(),
            "2468".into(),
            Some("1357".into()),
        )
        .unwrap();
        assert_eq!(
            unlock_app(home.clone(), pepper.clone(), "2468".into()).unwrap(),
            LockOutcome::Unlocked
        );
        assert_eq!(
            unlock_app(home.clone(), pepper.clone(), "0000".into()).unwrap(),
            LockOutcome::Wrong { retry_at_ms: 0 }
        );
        let wrong = set_duress_pin(home.clone(), pepper.clone(), "0000".into(), None);
        assert_eq!(wrong.unwrap_err().to_string(), "wrong PIN");
        assert_eq!(
            unlock_app(home.clone(), pepper.clone(), "1357".into()).unwrap(),
            LockOutcome::Erased {
                homes: vec![home.clone()]
            }
        );
        assert!(!dir.exists());
        assert!(!app_lock_enabled(home));
    }

    #[test]
    fn app_lock_settings_round_trip_and_never_expose_secrets() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("h");
        Home::new(&dir).create_identity(None).unwrap();
        let (home, pepper) = (s(&dir), Some("pepper".to_string()));
        assert_eq!(
            app_lock(home.clone(), pepper.clone()).unwrap(),
            LockSettings::off()
        );
        assert!(!app_lock_enabled(home.clone()));

        set_app_lock(
            home.clone(),
            pepper.clone(),
            None,
            None,
            None,
            true,
            0,
            Some(300),
            false,
        )
        .unwrap();
        assert!(app_lock_enabled(home.clone()));
        let device = app_lock(home.clone(), pepper.clone()).unwrap();
        assert_eq!(device.kind, None);
        assert!(device.biometric && device.enabled);

        set_app_lock(
            home.clone(),
            pepper.clone(),
            None,
            Some("long secret".into()),
            Some(LockKind::Passphrase),
            true,
            0,
            Some(300),
            false,
        )
        .unwrap();
        set_lock_policy(home.clone(), pepper.clone(), false, 60, None, true).unwrap();
        set_duress_pin(
            home.clone(),
            pepper.clone(),
            "long secret".into(),
            Some("other secret".into()),
        )
        .unwrap();
        assert_eq!(
            app_lock(home.clone(), pepper.clone()).unwrap(),
            LockSettings {
                enabled: true,
                kind: Some(LockKind::Passphrase),
                biometric: false,
                away_secs: 60,
                idle_secs: None,
                duress: true,
                erase_after_failures: true,
                retry_at_ms: 0,
            }
        );
        assert!(app_lock(home.clone(), Some("other".into())).is_err());
        assert!(lock_secret_ok(LockKind::Digits, "1234".into()));
        assert!(!lock_secret_ok(LockKind::Digits, "12a4".into()));

        clear_app_lock(home.clone(), pepper.clone(), Some("long secret".into())).unwrap();
        assert!(!app_lock_enabled(home));
    }

    #[test]
    fn app_lock_clock_object_follows_the_policy() {
        let c = AppLockClock::new(false, true, 0, Some(60), 0);
        assert!(!c.tick(59_999));
        c.activity(30_000);
        assert!(!c.tick(60_000));
        c.left(70_000);
        assert!(c.is_locked());
        c.unlocked(80_000);
        c.set_policy(true, 60, None);
        c.left_expecting_return(90_000);
        assert!(!c.returned(200_000));
        c.lock_now();
        assert!(c.returned(200_001));
    }
}
