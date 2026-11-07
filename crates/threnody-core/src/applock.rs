//! App lock (spec §16; Appendix C, *Local storage*): the settings and PIN
//! verifiers behind the apps' lock screen, the duress PIN that erases the
//! data directory, the back-off on wrong entries, and the lock timer.
//!
//! App lock is a gate on the user interface, not encryption at rest: the
//! node keeps running behind it. The lock file holds only Argon2id
//! verifiers of the secrets, never the secrets, and is encrypted under a
//! key derived from a *pepper* the platform keeps in its keystore
//! (Android Keystore, Secure Enclave, the Linux keyring), so a copy of the
//! data directory shows neither whether a duress PIN is set nor anything
//! to guess the PIN against offline.
//!
//! ```text
//! LockFile = { 0: 1, 1: peppered bool, 2: nonce bstr .size 12,
//!              3: ChaCha20-Poly1305(key, nonce, ad = "threnody lock", LockBody) }
//! key      = KDF("threnody v1 2026-11-05 app lock file key", pepper or "")
//! LockBody = { 0: 1, 1: [away_secs, idle_secs / null], 2: biometric bool,
//!              ? 3: [kind (0 digits, 1 passphrase), PinSlot], 4: PinSlot,
//!              5: duress set, 6: failures, 7: retry at ms, 8: last failure ms,
//!              ? 9: erase after failures bool, 15: zero padding }   ; 512 bytes
//! PinSlot  = { 0: salt bstr .size 16, 1: hash bstr .size 32, 2: [m_kib, t, lanes] }
//! ```
//!
//! The file exists exactly while app lock is on ([`Home::app_lock_enabled`]
//! needs no pepper, so the apps can fail closed before reading it).

use std::fs;

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::{kdf, random_bytes};
use crate::error::{Error, Result};
use crate::sealed::KdfParams;
use crate::store::{Home, secure_delete};
use crate::wipe::Erased;

/// The lock file in a data directory.
pub const FILE: &str = "lock.cbor";
pub const MIN_PIN_DIGITS: usize = 4;
pub const MAX_PIN_DIGITS: usize = 16;
pub const MIN_PASSPHRASE_CHARS: usize = 6;
/// The longest passphrase accepted (it is hashed, so this only bounds work).
pub const MAX_PASSPHRASE_CHARS: usize = 256;
/// Wrong entries before back-off starts, the first delay, and the cap.
pub const FREE_ATTEMPTS: u32 = 5;
pub const FIRST_DELAY_MS: u64 = 30_000;
pub const MAX_DELAY_MS: u64 = 3_600_000;
/// Wrong entries in a row that erase everything, when the user turned
/// that on ([`LockPolicy::erase_after_failures`]).
pub const ERASE_AFTER: u32 = 10;
/// The encrypted body's size, whatever it holds.
const PAD_TO: usize = 512;
const VERSION: u64 = 1;
const AD: &[u8] = b"threnody lock";

/// What the lock secret is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    /// 4 to 16 ASCII digits.
    Digits,
    /// At least 6 characters, none of them control characters.
    Passphrase,
}

/// When the app locks and how it unlocks besides the secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockPolicy {
    /// Unlock with the platform's own authentication (biometrics or the
    /// device credential). Alone, without a secret: "device lock".
    pub biometric: bool,
    /// Time away before it locks; 0 = on leaving.
    pub away_secs: u32,
    /// Idle time in front before it locks; None = never.
    pub idle_secs: Option<u32>,
    /// Erase everything after [`ERASE_AFTER`] wrong entries in a row.
    /// Off unless the user turns it on; needs a secret.
    pub erase_after_failures: bool,
}

impl LockPolicy {
    pub const DEFAULT: Self = Self {
        biometric: true,
        away_secs: 0,
        idle_secs: Some(300),
        erase_after_failures: false,
    };
}

/// What the settings screen may know. Never carries verifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockView {
    pub policy: LockPolicy,
    pub secret: Option<SecretKind>,
    pub duress: bool,
    /// Wall-clock ms before which entries are refused (0: none).
    pub retry_at_ms: u64,
}

/// The outcome of an entry on the lock screen.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Unlock,
    /// The duress secret: the home is already erased when this returns.
    Duress(Erased),
    /// The last of [`ERASE_AFTER`] wrong entries with
    /// [`LockPolicy::erase_after_failures`] on: the home is erased.
    Exhausted(Erased),
    Wrong {
        retry_at_ms: u64,
    },
    /// Refused unchecked: back-off in force.
    Wait {
        retry_at_ms: u64,
    },
}

/// Whether `s` is acceptable as a secret of `kind`.
pub fn secret_ok(s: &str, kind: SecretKind) -> bool {
    match kind {
        SecretKind::Digits => {
            (MIN_PIN_DIGITS..=MAX_PIN_DIGITS).contains(&s.len())
                && s.bytes().all(|b| b.is_ascii_digit())
        }
        SecretKind::Passphrase => {
            let n = s.chars().count();
            (MIN_PASSPHRASE_CHARS..=MAX_PASSPHRASE_CHARS).contains(&n)
                && !s.chars().any(char::is_control)
        }
    }
}

fn check_secret(s: &str, kind: SecretKind) -> Result<()> {
    if secret_ok(s, kind) {
        Ok(())
    } else {
        Err(Error::Malformed(match kind {
            SecretKind::Digits => "a PIN is 4 to 16 digits",
            SecretKind::Passphrase => "a passphrase needs at least 6 characters",
        }))
    }
}

/// An Argon2id verifier of one secret.
#[derive(Clone, Debug)]
struct Slot {
    salt: [u8; 16],
    hash: [u8; 32],
    params: KdfParams,
}

impl Slot {
    fn new(secret: &str, params: KdfParams) -> Result<Self> {
        let salt = random_bytes();
        let hash = *params.derive(secret.as_bytes(), &salt)?;
        Ok(Self { salt, hash, params })
    }

    /// A slot no secret opens, indistinguishable from a real one.
    fn decoy(params: KdfParams) -> Self {
        Self {
            salt: random_bytes(),
            hash: random_bytes(),
            params,
        }
    }

    /// Hashes `entered` under this slot (always the full work) and
    /// compares in constant time.
    fn matches(&self, entered: &str) -> Result<bool> {
        let h = self.params.derive(entered.as_bytes(), &self.salt)?;
        Ok(blake3::Hash::from(*h) == blake3::Hash::from(self.hash))
    }

    fn encode(
        &self,
        e: &mut const_cbor::Encoder<'_>,
    ) -> core::result::Result<(), const_cbor::Error> {
        e.map_len(3)?;
        e.u8(0)?.bytes(&self.salt)?;
        e.u8(1)?.bytes(&self.hash)?;
        e.u8(2)?.array_len(3)?;
        e.u32(self.params.m_kib)?
            .u32(self.params.t)?
            .u32(self.params.lanes)?;
        Ok(())
    }

    fn decode(d: &mut Decoder<'_>) -> Result<Self> {
        let (mut salt, mut hash, mut params) = (None, None, None);
        read_map(d, |k, d| {
            match k {
                0 => salt = Some(fixed_bytes::<16>(d)?),
                1 => hash = Some(fixed_bytes::<32>(d)?),
                2 => {
                    if d.array_len()? != 3 {
                        return Err(Error::Malformed("lock slot parameters"));
                    }
                    params = Some(KdfParams {
                        m_kib: d.u32()?,
                        t: d.u32()?,
                        lanes: d.u32()?,
                    });
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(Self {
            salt: required(salt, "lock slot salt")?,
            hash: required(hash, "lock slot hash")?,
            params: required(params, "lock slot parameters")?,
        })
    }
}

/// The decrypted lock file.
#[derive(Clone, Debug)]
struct Body {
    policy: LockPolicy,
    secret: Option<(SecretKind, Slot)>,
    duress: Slot,
    duress_set: bool,
    failures: u32,
    retry_at_ms: u64,
    last_fail_ms: u64,
}

impl Body {
    fn view(&self) -> LockView {
        LockView {
            policy: self.policy,
            secret: self.secret.as_ref().map(|(k, _)| *k),
            duress: self.duress_set,
            retry_at_ms: self.retry_at_ms,
        }
    }

    fn encode_padded(&self, pad: usize) -> Result<Vec<u8>> {
        cbor::to_vec(PAD_TO + 64, |e| {
            e.map_len(if self.secret.is_some() { 11 } else { 10 })?;
            e.u8(0)?.uint(VERSION)?;
            e.u8(1)?.array_len(2)?.u32(self.policy.away_secs)?;
            match self.policy.idle_secs {
                Some(s) => e.u32(s)?,
                None => e.null()?,
            };
            e.u8(2)?.bool(self.policy.biometric)?;
            if let Some((kind, slot)) = &self.secret {
                e.u8(3)?.array_len(2)?.u8(match kind {
                    SecretKind::Digits => 0,
                    SecretKind::Passphrase => 1,
                })?;
                slot.encode(e)?;
            }
            e.u8(4)?;
            self.duress.encode(e)?;
            e.u8(5)?.bool(self.duress_set)?;
            e.u8(6)?.u32(self.failures)?;
            e.u8(7)?.uint(self.retry_at_ms)?;
            e.u8(8)?.uint(self.last_fail_ms)?;
            e.u8(9)?.bool(self.policy.erase_after_failures)?;
            e.u8(15)?.bytes(&vec![0u8; pad])?;
            Ok(())
        })
    }

    /// The body, padded to exactly [`PAD_TO`] bytes.
    fn encode(&self) -> Result<Zeroizing<Vec<u8>>> {
        let bare = self.encode_padded(0)?.len();
        let mut pad = PAD_TO.saturating_sub(bare);
        // The padding's length prefix grows with it: shrink until it fits.
        loop {
            let out = Zeroizing::new(self.encode_padded(pad)?);
            match out.len() {
                n if n == PAD_TO => return Ok(out),
                n if n > PAD_TO && pad > 0 => pad -= (n - PAD_TO).min(pad),
                _ => return Ok(out),
            }
        }
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let mut d = Decoder::new(b);
        let (mut ver, mut times, mut bio, mut secret, mut duress) = (None, None, None, None, None);
        let (mut set, mut fails, mut retry, mut last) = (None, None, None, None);
        let mut erase_after = false;
        read_map(&mut d, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("lock times"));
                    }
                    let away = d.u32()?;
                    let idle = if d.is_null()? { None } else { Some(d.u32()?) };
                    times = Some((away, idle));
                }
                2 => bio = Some(d.bool()?),
                3 => {
                    if d.array_len()? != 2 {
                        return Err(Error::Malformed("lock secret"));
                    }
                    let kind = match d.u64()? {
                        0 => SecretKind::Digits,
                        1 => SecretKind::Passphrase,
                        _ => return Err(Error::Malformed("lock secret kind")),
                    };
                    secret = Some((kind, Slot::decode(d)?));
                }
                4 => duress = Some(Slot::decode(d)?),
                5 => set = Some(d.bool()?),
                6 => fails = Some(d.u32()?),
                7 => retry = Some(d.u64()?),
                8 => last = Some(d.u64()?),
                9 => erase_after = d.bool()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&d)?;
        match required(ver, "lock version")? {
            VERSION => {}
            v => return Err(Error::UnsupportedVersion(v)),
        }
        let (away_secs, idle_secs) = required(times, "lock times")?;
        Ok(Self {
            policy: LockPolicy {
                biometric: required(bio, "lock biometric")?,
                away_secs,
                idle_secs,
                erase_after_failures: erase_after,
            },
            secret,
            duress: required(duress, "lock duress slot")?,
            duress_set: required(set, "lock duress flag")?,
            failures: required(fails, "lock failures")?,
            retry_at_ms: required(retry, "lock retry time")?,
            last_fail_ms: required(last, "lock last failure")?,
        })
    }
}

/// Held across every read-change-write of a lock file, so two entries
/// checked at once can't both count from the same number of failures
/// (and an erase after wrong entries come late). One for the process:
/// lock files are rarely touched, and never by two homes at once.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    FILE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn file_key(pepper: Option<&[u8]>) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(kdf::derive(
        kdf::label::APP_LOCK_KEY,
        &[pepper.unwrap_or(b"")],
    ))
}

/// The back-off after `failures` wrong entries in a row (0: none).
fn delay_ms(failures: u32) -> u64 {
    if failures < FREE_ATTEMPTS {
        return 0;
    }
    let shift = (failures - FREE_ATTEMPTS).min(7);
    (FIRST_DELAY_MS << shift).min(MAX_DELAY_MS)
}

fn check_policy(policy: &LockPolicy, has_secret: bool) -> Result<()> {
    if !has_secret && !policy.biometric {
        return Err(Error::Malformed(
            "app lock needs a PIN, a passphrase or the device lock",
        ));
    }
    if !has_secret && policy.erase_after_failures {
        return Err(Error::Malformed(
            "erasing after wrong entries needs an app PIN or passphrase",
        ));
    }
    Ok(())
}

impl Home {
    fn lock_path(&self) -> std::path::PathBuf {
        self.dir().join(FILE)
    }

    /// Whether app lock is on: the lock file exists. Needs no pepper, so
    /// the platforms can ask synchronously and fail closed.
    pub fn app_lock_enabled(&self) -> bool {
        self.lock_path().exists()
    }

    fn load_lock(&self, pepper: Option<&[u8]>) -> Result<Option<Body>> {
        let raw = match fs::read(self.lock_path()) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let mut d = Decoder::new(&raw);
        let (mut ver, mut peppered, mut nonce, mut ct) = (None, None, None, None);
        read_map(&mut d, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => peppered = Some(d.bool()?),
                2 => nonce = Some(fixed_bytes::<12>(d)?),
                3 => ct = Some(d.bytes()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&d)?;
        match required(ver, "lock file version")? {
            VERSION => {}
            v => return Err(Error::UnsupportedVersion(v)),
        }
        if required(peppered, "lock pepper flag")? != pepper.is_some() {
            return Err(Error::Malformed("app lock pepper mismatch"));
        }
        let pt = Zeroizing::new(Suite::ChaCha20Poly1305.open(
            &file_key(pepper),
            &required(nonce, "lock nonce")?,
            AD,
            required(ct, "lock body")?,
        )?);
        Body::decode(&pt).map(Some)
    }

    fn save_lock(&self, pepper: Option<&[u8]>, body: &Body) -> Result<()> {
        let pt = body.encode()?;
        let nonce: [u8; 12] = random_bytes();
        let ct = Suite::ChaCha20Poly1305.seal(&file_key(pepper), &nonce, AD, &pt);
        let out = cbor::to_vec(ct.len() + 64, |e| {
            e.map_len(4)?;
            e.u8(0)?.uint(VERSION)?;
            e.u8(1)?.bool(pepper.is_some())?;
            e.u8(2)?.bytes(&nonce)?;
            e.u8(3)?.bytes(&ct)?;
            Ok(())
        })?;
        self.write(&self.lock_path(), &out)
    }

    /// The app lock's settings, or `None` when it is off.
    pub fn app_lock(&self, pepper: Option<&[u8]>) -> Result<Option<LockView>> {
        Ok(self.load_lock(pepper)?.map(|b| b.view()))
    }

    /// Turns app lock on or changes how it unlocks. `current` must be the
    /// existing secret when one is set. `secret = None` with
    /// `policy.biometric = false` is refused (a lock nothing opens). Keeps
    /// the duress secret unless `secret` changes to it (refused) or is
    /// removed (the duress secret goes too).
    pub fn set_app_lock(
        &self,
        pepper: Option<&[u8]>,
        current: Option<&str>,
        secret: Option<(&str, SecretKind)>,
        policy: LockPolicy,
    ) -> Result<()> {
        self.set_app_lock_with(pepper, current, secret, policy, KdfParams::LOCK)
    }

    pub fn set_app_lock_with(
        &self,
        pepper: Option<&[u8]>,
        current: Option<&str>,
        secret: Option<(&str, SecretKind)>,
        policy: LockPolicy,
        p: KdfParams,
    ) -> Result<()> {
        if let Some((s, kind)) = secret {
            check_secret(s, kind)?;
        }
        check_policy(&policy, secret.is_some())?;
        let _held = file_lock();
        let old = self.load_lock(pepper)?;
        if let Some((_, slot)) = old.as_ref().and_then(|b| b.secret.as_ref())
            && !slot.matches(current.unwrap_or(""))?
        {
            return Err(Error::WrongPin);
        }
        let (mut duress, mut duress_set) = (Slot::decoy(p), false);
        if let Some(old) = &old
            && old.duress_set
            && let (Some((s, kind)), Some((old_kind, _))) = (secret, &old.secret)
        {
            if kind != *old_kind {
                return Err(Error::Malformed(
                    "remove the duress PIN before changing what kind of secret locks the app",
                ));
            }
            if old.duress.matches(s)? {
                return Err(Error::Malformed("the duress PIN must differ from your PIN"));
            }
            duress = old.duress.clone();
            duress_set = true;
        }
        let secret = secret
            .map(|(s, kind)| Ok::<_, Error>((kind, Slot::new(s, p)?)))
            .transpose()?;
        self.save_lock(
            pepper,
            &Body {
                policy,
                secret,
                duress,
                duress_set,
                failures: 0,
                retry_at_ms: 0,
                last_fail_ms: 0,
            },
        )
    }

    /// Changes only the policy (times, biometrics, erasing after wrong
    /// entries). Refused if it would leave neither a secret nor biometrics.
    pub fn set_lock_policy(&self, pepper: Option<&[u8]>, policy: LockPolicy) -> Result<()> {
        let _held = file_lock();
        let mut body = self
            .load_lock(pepper)?
            .ok_or(Error::Malformed("app lock is off"))?;
        check_policy(&policy, body.secret.is_some())?;
        body.policy = policy;
        self.save_lock(pepper, &body)
    }

    /// Turns app lock off: checks `current` when a secret is set, then
    /// deletes the lock file, so the file exists exactly while app lock
    /// is on.
    pub fn clear_app_lock(&self, pepper: Option<&[u8]>, current: Option<&str>) -> Result<()> {
        let _held = file_lock();
        let Some(body) = self.load_lock(pepper)? else {
            return Ok(());
        };
        if let Some((_, slot)) = &body.secret
            && !slot.matches(current.unwrap_or(""))?
        {
            return Err(Error::WrongPin);
        }
        secure_delete(&self.lock_path())
    }

    /// Sets (Some) or removes (None) the duress secret; needs the lock
    /// secret, which it must differ from and match the kind of.
    pub fn set_duress(
        &self,
        pepper: Option<&[u8]>,
        current: &str,
        duress: Option<&str>,
    ) -> Result<()> {
        self.set_duress_with(pepper, current, duress, KdfParams::LOCK)
    }

    pub fn set_duress_with(
        &self,
        pepper: Option<&[u8]>,
        current: &str,
        duress: Option<&str>,
        p: KdfParams,
    ) -> Result<()> {
        let _held = file_lock();
        let mut body = self
            .load_lock(pepper)?
            .ok_or(Error::Malformed("app lock is off"))?;
        let Some((kind, slot)) = &body.secret else {
            return Err(Error::Malformed(
                "a duress PIN needs an app PIN or passphrase",
            ));
        };
        if !slot.matches(current)? {
            return Err(Error::WrongPin);
        }
        match duress {
            Some(d) => {
                check_secret(d, *kind)?;
                if d == current {
                    return Err(Error::Malformed("the duress PIN must differ from your PIN"));
                }
                body.duress = Slot::new(d, p)?;
                body.duress_set = true;
            }
            None => {
                body.duress = Slot::decoy(p);
                body.duress_set = false;
            }
        }
        self.save_lock(pepper, &body)
    }

    /// Checks an entry on the lock screen at `now_ms` (wall clock). Does
    /// the same work for every entry, so its timing shows nothing.
    pub fn check_lock(&self, pepper: Option<&[u8]>, entered: &str, now_ms: u64) -> Result<Verdict> {
        let _held = file_lock();
        let Some(mut body) = self.load_lock(pepper)? else {
            return Ok(Verdict::Unlock);
        };
        let Some((_, slot)) = &body.secret else {
            return Err(Error::Malformed("no app PIN is set"));
        };
        // A clock set back must not keep the user out for as long.
        if now_ms < body.last_fail_ms && body.retry_at_ms > 0 {
            body.retry_at_ms = now_ms + delay_ms(body.failures);
            body.last_fail_ms = now_ms;
            self.save_lock(pepper, &body)?;
            return Ok(Verdict::Wait {
                retry_at_ms: body.retry_at_ms,
            });
        }
        if now_ms < body.retry_at_ms {
            return Ok(Verdict::Wait {
                retry_at_ms: body.retry_at_ms,
            });
        }
        // Both, always: which one matched mustn't show in the time taken.
        let pin = slot.matches(entered)?;
        let duress = body.duress.matches(entered)?;
        if pin {
            body.failures = 0;
            body.retry_at_ms = 0;
            body.last_fail_ms = 0;
            self.save_lock(pepper, &body)?;
            return Ok(Verdict::Unlock);
        }
        if duress && body.duress_set {
            return Ok(Verdict::Duress(self.erase()?));
        }
        body.failures = body.failures.saturating_add(1);
        body.last_fail_ms = now_ms;
        if body.policy.erase_after_failures && body.failures >= ERASE_AFTER {
            return Ok(Verdict::Exhausted(self.erase()?));
        }
        let delay = delay_ms(body.failures);
        body.retry_at_ms = if delay > 0 { now_ms + delay } else { 0 };
        self.save_lock(pepper, &body)?;
        Ok(Verdict::Wrong {
            retry_at_ms: body.retry_at_ms,
        })
    }

    /// A biometric unlock succeeded: clears the failure count.
    pub fn lock_passed(&self, pepper: Option<&[u8]>) -> Result<()> {
        let _held = file_lock();
        let Some(mut body) = self.load_lock(pepper)? else {
            return Ok(());
        };
        if body.failures == 0 && body.retry_at_ms == 0 {
            return Ok(());
        }
        body.failures = 0;
        body.retry_at_ms = 0;
        body.last_fail_ms = 0;
        self.save_lock(pepper, &body)
    }
}

/// The lock timer: when the app locks after leaving it or leaving it
/// idle. Pure, so the platforms only feed it events and a clock (a
/// monotonic one that counts sleep where they have it). Every clock that
/// goes backwards locks.
#[derive(Clone, Debug)]
pub struct LockClock {
    locked: bool,
    policy: LockPolicy,
    last_ms: u64,
    away_since: Option<u64>,
    away_limit_ms: u64,
}

/// How long the app may be away in its own picker or camera before it
/// locks, at least.
pub const PICKER_GRACE_SECS: u32 = 180;

impl LockClock {
    pub fn new(locked: bool, policy: LockPolicy, now_ms: u64) -> Self {
        Self {
            locked,
            policy,
            last_ms: now_ms,
            away_since: None,
            away_limit_ms: 0,
        }
    }

    pub fn set_policy(&mut self, p: LockPolicy) {
        self.policy = p;
    }

    /// User input in front: postpones the idle lock (ignored while locked).
    pub fn activity(&mut self, now_ms: u64) {
        if !self.locked && self.away_since.is_none() {
            self.last_ms = now_ms;
        }
    }

    fn leave(&mut self, now_ms: u64, limit_secs: u32) {
        if self.locked {
            return;
        }
        if limit_secs == 0 {
            self.locked = true;
            self.away_since = None;
        } else {
            self.away_since = Some(now_ms);
            self.away_limit_ms = u64::from(limit_secs) * 1000;
        }
    }

    /// The app went to the background / was hidden.
    pub fn left(&mut self, now_ms: u64) {
        self.leave(now_ms, self.policy.away_secs);
    }

    /// The app opened its own picker or camera: like [`LockClock::left`],
    /// with at least [`PICKER_GRACE_SECS`] before it locks.
    pub fn left_expecting_return(&mut self, now_ms: u64) {
        self.leave(now_ms, self.policy.away_secs.max(PICKER_GRACE_SECS));
    }

    fn away_expired(&self, now_ms: u64) -> bool {
        self.away_since
            .is_some_and(|t| now_ms < t || now_ms - t >= self.away_limit_ms)
    }

    /// The app came back; returns whether it is locked.
    pub fn returned(&mut self, now_ms: u64) -> bool {
        if self.away_expired(now_ms) {
            self.locked = true;
        }
        self.away_since = None;
        if !self.locked {
            self.last_ms = now_ms;
        }
        self.locked
    }

    /// Periodic check (every ~15 s); returns whether it is locked.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        if self.locked {
            return true;
        }
        if self.away_since.is_some() {
            if self.away_expired(now_ms) {
                self.locked = true;
                self.away_since = None;
            }
        } else if let Some(s) = self.policy.idle_secs
            && (now_ms < self.last_ms || now_ms - self.last_ms >= u64::from(s) * 1000)
        {
            self.locked = true;
        }
        self.locked
    }

    pub fn unlocked(&mut self, now_ms: u64) {
        self.locked = false;
        self.away_since = None;
        self.last_ms = now_ms;
    }

    pub fn lock_now(&mut self) {
        self.locked = true;
        self.away_since = None;
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sealed::TEST_PARAMS as P;

    const PEPPER: &[u8] = b"pepper from the keystore";
    const PIN: &str = "2468";
    const DURESS: &str = "1357";
    const T: u64 = 1_800_000_000_000;

    fn home() -> (tempfile::TempDir, Home) {
        let t = tempfile::tempdir().unwrap();
        let h = Home::new(t.path().join("h"));
        h.create_identity_with(None, P).unwrap();
        (t, h)
    }

    fn pin_lock(h: &Home) {
        h.set_app_lock_with(
            Some(PEPPER),
            None,
            Some((PIN, SecretKind::Digits)),
            LockPolicy::DEFAULT,
            P,
        )
        .unwrap();
    }

    fn with_duress(h: &Home) {
        pin_lock(h);
        h.set_duress_with(Some(PEPPER), PIN, Some(DURESS), P)
            .unwrap();
    }

    fn check(h: &Home, s: &str, now: u64) -> Verdict {
        h.check_lock(Some(PEPPER), s, now).unwrap()
    }

    fn failures(h: &Home) -> u32 {
        h.load_lock(Some(PEPPER)).unwrap().unwrap().failures
    }

    #[test]
    fn no_lock_file_means_unlocked_and_no_settings() {
        let (_t, h) = home();
        assert!(!h.app_lock_enabled());
        assert_eq!(h.app_lock(Some(PEPPER)).unwrap(), None);
        assert_eq!(check(&h, "anything", T), Verdict::Unlock);
    }

    #[test]
    fn device_lock_mode_refuses_pin_checks() {
        let (_t, h) = home();
        h.set_app_lock_with(Some(PEPPER), None, None, LockPolicy::DEFAULT, P)
            .unwrap();
        assert!(h.app_lock_enabled());
        for s in ["", "0000", "anything"] {
            assert!(h.check_lock(Some(PEPPER), s, T).is_err());
        }
        let v = h.app_lock(Some(PEPPER)).unwrap().unwrap();
        assert_eq!(v.secret, None);
        assert!(v.policy.biometric);
    }

    #[test]
    fn clearing_the_lock_removes_the_file() {
        let (_t, h) = home();
        pin_lock(&h);
        assert!(h.app_lock_enabled());
        assert!(h.dir().join(FILE).exists());
        assert!(matches!(
            h.clear_app_lock(Some(PEPPER), Some("0000")),
            Err(Error::WrongPin)
        ));
        h.clear_app_lock(Some(PEPPER), Some(PIN)).unwrap();
        assert!(!h.app_lock_enabled());
        assert!(!h.dir().join(FILE).exists());
        // Device lock needs no secret to clear.
        h.set_app_lock_with(Some(PEPPER), None, None, LockPolicy::DEFAULT, P)
            .unwrap();
        h.clear_app_lock(Some(PEPPER), None).unwrap();
        assert!(!h.app_lock_enabled());
    }

    #[test]
    fn pin_unlocks_and_wrong_pin_counts() {
        let (_t, h) = home();
        pin_lock(&h);
        assert_eq!(check(&h, "0000", T), Verdict::Wrong { retry_at_ms: 0 });
        assert_eq!(check(&h, PIN, T), Verdict::Unlock);
        assert_eq!(failures(&h), 0);
        for _ in 0..4 {
            assert_eq!(check(&h, "0000", T), Verdict::Wrong { retry_at_ms: 0 });
        }
        assert_eq!(
            check(&h, "0000", T),
            Verdict::Wrong {
                retry_at_ms: T + 30_000
            }
        );
        assert_eq!(
            check(&h, PIN, T + 29_999),
            Verdict::Wait {
                retry_at_ms: T + 30_000
            }
        );
        assert_eq!(
            check(&h, "0000", T + 30_000),
            Verdict::Wrong {
                retry_at_ms: T + 30_000 + 60_000
            }
        );
        assert_eq!(check(&h, PIN, T + 90_000), Verdict::Unlock);
        assert_eq!(failures(&h), 0);
    }

    #[test]
    fn clock_set_back_reanchors_the_backoff() {
        let (_t, h) = home();
        pin_lock(&h);
        for _ in 0..5 {
            check(&h, "0000", T);
        }
        let back = T - 86_400_000;
        assert_eq!(
            check(&h, PIN, back),
            Verdict::Wait {
                retry_at_ms: back + 30_000
            }
        );
        // And then it runs from there, not again from each entry.
        assert_eq!(
            check(&h, PIN, back + 10),
            Verdict::Wait {
                retry_at_ms: back + 30_000
            }
        );
        assert_eq!(check(&h, PIN, back + 30_000), Verdict::Unlock);
    }

    #[test]
    fn entries_during_backoff_are_refused_unchecked() {
        let (_t, h) = home();
        with_duress(&h);
        for _ in 0..5 {
            check(&h, "0000", T);
        }
        let wait = Verdict::Wait {
            retry_at_ms: T + 30_000,
        };
        for s in [PIN, "0000", DURESS] {
            assert_eq!(check(&h, s, T + 1), wait);
        }
        assert!(h.dir().exists());
        assert_eq!(failures(&h), 5);
    }

    #[test]
    fn backoff_survives_reopening() {
        let (_t, h) = home();
        pin_lock(&h);
        for _ in 0..5 {
            check(&h, "0000", T);
        }
        let again = Home::new(h.dir());
        assert_eq!(
            again.app_lock(Some(PEPPER)).unwrap().unwrap().retry_at_ms,
            T + 30_000
        );
        assert_eq!(
            again.check_lock(Some(PEPPER), PIN, T + 5).unwrap(),
            Verdict::Wait {
                retry_at_ms: T + 30_000
            }
        );
    }

    #[test]
    fn backoff_is_capped_at_an_hour() {
        let (_t, h) = home();
        pin_lock(&h);
        let mut now = T;
        let mut last = 0;
        for _ in 0..20 {
            if let Verdict::Wrong { retry_at_ms } = check(&h, "0000", now) {
                last = retry_at_ms.saturating_sub(now);
                now = now.max(retry_at_ms);
            }
        }
        assert_eq!(last, MAX_DELAY_MS);
        assert_eq!(delay_ms(u32::MAX), MAX_DELAY_MS);
    }

    #[test]
    fn entries_checked_at_once_all_count() {
        let (_t, h) = home();
        pin_lock(&h);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| check(&h, "0000", T));
            }
        });
        assert_eq!(failures(&h), 4);
    }

    #[test]
    fn duress_pin_erases_the_home() {
        let (_t, h) = home();
        let id = h.load_identity(None).unwrap();
        crate::persona::Personas::new(&h, &id)
            .create("p", None, None, 1)
            .unwrap();
        with_duress(&h);
        let Verdict::Duress(erased) = check(&h, DURESS, T) else {
            panic!("not erased");
        };
        assert!(!h.dir().exists());
        assert_eq!(erased.homes.len(), 2);
        assert_eq!(erased.homes[0], h.dir());
    }

    #[test]
    fn erase_after_ten_wrong_entries_when_turned_on() {
        let (_t, h) = home();
        pin_lock(&h);
        // Off by default: twenty wrong entries erase nothing.
        let mut now = T;
        for _ in 0..20 {
            if let Verdict::Wrong { retry_at_ms } = check(&h, "0000", now) {
                now = now.max(retry_at_ms);
            }
        }
        assert!(h.dir().exists());
        check(&h, PIN, now);

        let policy = LockPolicy {
            erase_after_failures: true,
            ..LockPolicy::DEFAULT
        };
        h.set_lock_policy(Some(PEPPER), policy).unwrap();
        for i in 1..ERASE_AFTER {
            let v = check(&h, "0000", now);
            let Verdict::Wrong { retry_at_ms } = v else {
                panic!("entry {i}: {v:?}");
            };
            now = now.max(retry_at_ms);
        }
        assert!(h.dir().exists());
        assert!(matches!(check(&h, "0000", now), Verdict::Exhausted(_)));
        assert!(!h.dir().exists());
    }

    #[test]
    fn erase_after_failures_needs_a_secret() {
        let (_t, h) = home();
        let policy = LockPolicy {
            erase_after_failures: true,
            ..LockPolicy::DEFAULT
        };
        assert!(
            h.set_app_lock_with(Some(PEPPER), None, None, policy, P)
                .is_err()
        );
        h.set_app_lock_with(Some(PEPPER), None, None, LockPolicy::DEFAULT, P)
            .unwrap();
        assert!(h.set_lock_policy(Some(PEPPER), policy).is_err());
    }

    #[test]
    fn duress_must_differ_and_match_the_kind() {
        let (_t, h) = home();
        h.set_app_lock_with(Some(PEPPER), None, None, LockPolicy::DEFAULT, P)
            .unwrap();
        assert!(
            h.set_duress_with(Some(PEPPER), "", Some(DURESS), P)
                .is_err()
        );
        pin_lock(&h);
        assert!(h.set_duress_with(Some(PEPPER), PIN, Some(PIN), P).is_err());
        assert!(
            h.set_duress_with(Some(PEPPER), PIN, Some("a passphrase"), P)
                .is_err()
        );
        assert!(matches!(
            h.set_duress_with(Some(PEPPER), "0000", Some(DURESS), P),
            Err(Error::WrongPin)
        ));
        h.set_duress_with(Some(PEPPER), PIN, Some(DURESS), P)
            .unwrap();
        assert!(h.app_lock(Some(PEPPER)).unwrap().unwrap().duress);
        h.set_duress_with(Some(PEPPER), PIN, None, P).unwrap();
        assert!(!h.app_lock(Some(PEPPER)).unwrap().unwrap().duress);
        // Removed: the old duress PIN is just a wrong one now.
        assert_eq!(check(&h, DURESS, T), Verdict::Wrong { retry_at_ms: 0 });
        assert!(h.dir().exists());
    }

    #[test]
    fn changing_or_removing_the_lock_needs_the_current_secret() {
        let (_t, h) = home();
        with_duress(&h);
        let set = |current, secret| {
            h.set_app_lock_with(Some(PEPPER), current, secret, LockPolicy::DEFAULT, P)
        };
        assert!(matches!(
            set(Some("0000"), Some(("9999", SecretKind::Digits))),
            Err(Error::WrongPin)
        ));
        assert!(matches!(set(None, None), Err(Error::WrongPin)));
        // Changing to the duress PIN is refused.
        assert!(set(Some(PIN), Some((DURESS, SecretKind::Digits))).is_err());
        // Changing the kind with a duress PIN set is refused.
        assert!(set(Some(PIN), Some(("long passphrase", SecretKind::Passphrase))).is_err());
        // A change keeps the duress PIN.
        set(Some(PIN), Some(("9999", SecretKind::Digits))).unwrap();
        assert!(h.app_lock(Some(PEPPER)).unwrap().unwrap().duress);
        assert_eq!(check(&h, "9999", T), Verdict::Unlock);
        // Removing the secret removes the duress PIN.
        set(Some("9999"), None).unwrap();
        let v = h.app_lock(Some(PEPPER)).unwrap().unwrap();
        assert!(!v.duress && v.secret.is_none());
    }

    #[test]
    fn device_lock_needs_biometrics() {
        let (_t, h) = home();
        let off = LockPolicy {
            biometric: false,
            ..LockPolicy::DEFAULT
        };
        assert!(
            h.set_app_lock_with(Some(PEPPER), None, None, off, P)
                .is_err()
        );
        h.set_app_lock_with(Some(PEPPER), None, None, LockPolicy::DEFAULT, P)
            .unwrap();
        assert!(h.set_lock_policy(Some(PEPPER), off).is_err());
        // With a PIN, biometrics can go.
        h.set_app_lock_with(Some(PEPPER), None, Some((PIN, SecretKind::Digits)), off, P)
            .unwrap();
        assert!(!h.app_lock(Some(PEPPER)).unwrap().unwrap().policy.biometric);
    }

    #[test]
    fn the_file_reveals_nothing_without_the_pepper() {
        let (_t, h) = home();
        pin_lock(&h);
        let plain = fs::read(h.dir().join(FILE)).unwrap();
        h.set_duress_with(Some(PEPPER), PIN, Some(DURESS), P)
            .unwrap();
        let with = fs::read(h.dir().join(FILE)).unwrap();
        assert_eq!(plain.len(), with.len());
        for f in [&plain, &with] {
            assert!(!f.windows(PIN.len()).any(|w| w == PIN.as_bytes()));
            assert!(!f.windows(DURESS.len()).any(|w| w == DURESS.as_bytes()));
        }
        assert!(matches!(h.app_lock(Some(b"other")), Err(Error::Decrypt)));
        assert!(matches!(
            h.app_lock(None),
            Err(Error::Malformed("app lock pepper mismatch"))
        ));
        // Without a pepper it works too, and says so.
        let (_t2, h2) = home();
        h2.set_app_lock_with(
            None,
            None,
            Some((PIN, SecretKind::Digits)),
            LockPolicy::DEFAULT,
            P,
        )
        .unwrap();
        assert_eq!(h2.check_lock(None, PIN, T).unwrap(), Verdict::Unlock);
        assert!(h2.app_lock(Some(PEPPER)).is_err());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let (_t, h) = home();
        pin_lock(&h);
        let body = h.load_lock(Some(PEPPER)).unwrap().unwrap();
        let pt = body.encode().unwrap();
        // The same body with an extra key 10 (as a later version might).
        let mut extra = pt[..].to_vec();
        assert_eq!(extra[0], 0xa0 | 11);
        extra[0] = 0xa0 | 12;
        extra.extend_from_slice(&[0x0a, 0xf5]);
        let b = Body::decode(&extra).unwrap();
        assert_eq!(b.view(), body.view());
    }

    #[test]
    fn secret_validation() {
        use SecretKind::*;
        for (s, k) in [
            ("123", Digits),
            ("12345678901234567", Digits),
            ("12a4", Digits),
            ("12345", Passphrase),
            ("pass\nword", Passphrase),
            ("", Digits),
        ] {
            assert!(!secret_ok(s, k), "{s:?}");
        }
        for (s, k) in [
            ("1234", Digits),
            ("1234567890123456", Digits),
            ("sixchr", Passphrase),
            ("ünïcode pass", Passphrase),
        ] {
            assert!(secret_ok(s, k), "{s:?}");
        }
    }

    // The lock timer.

    fn policy(away: u32, idle: Option<u32>) -> LockPolicy {
        LockPolicy {
            biometric: true,
            away_secs: away,
            idle_secs: idle,
            erase_after_failures: false,
        }
    }

    #[test]
    fn locks_on_leaving_when_away_is_zero() {
        let mut c = LockClock::new(false, policy(0, None), 0);
        assert!(!c.is_locked());
        c.left(10);
        assert!(c.is_locked());
        assert!(c.returned(11));
    }

    #[test]
    fn unlock_while_away_is_not_undone_by_returning() {
        let mut c = LockClock::new(true, policy(0, Some(300)), 0);
        c.left(1_000);
        c.unlocked(1_001);
        assert!(!c.returned(1_002));
    }

    #[test]
    fn returning_resets_the_idle_timer() {
        let mut c = LockClock::new(false, policy(600, Some(300)), 0);
        c.activity(0);
        c.left(1_000);
        assert!(!c.returned(400_000));
        assert!(!c.tick(400_001));
    }

    #[test]
    fn leaving_for_a_picker_uses_the_grace() {
        let mut c = LockClock::new(false, policy(0, None), 0);
        c.left_expecting_return(0);
        assert!(!c.is_locked());
        assert!(!c.returned(120_000));
        c.left_expecting_return(120_000);
        assert!(c.returned(320_000));
    }

    #[test]
    fn grace_period_before_locking_after_leaving() {
        let mut c = LockClock::new(false, policy(60, None), 0);
        c.left(0);
        assert!(!c.returned(59_000));
        c.left(100_000);
        assert!(!c.tick(159_999));
        assert!(c.returned(160_000));
    }

    #[test]
    fn idle_in_front_locks_after_the_timeout() {
        let mut c = LockClock::new(false, policy(0, Some(60)), 0);
        assert!(!c.tick(30_000));
        c.activity(50_000);
        assert!(!c.tick(100_000));
        assert!(c.tick(110_000));
    }

    #[test]
    fn idle_none_never_locks_in_front() {
        let mut c = LockClock::new(false, policy(0, None), 0);
        assert!(!c.tick(u64::MAX / 2));
    }

    #[test]
    fn clock_going_backwards_locks() {
        let mut c = LockClock::new(false, policy(600, Some(300)), 10_000);
        assert!(c.tick(5_000));
        let mut c = LockClock::new(false, policy(600, Some(300)), 10_000);
        c.left(10_000);
        assert!(c.returned(9_000));
    }

    #[test]
    fn activity_while_locked_does_not_unlock_or_extend() {
        let mut c = LockClock::new(true, policy(0, Some(60)), 0);
        c.activity(1_000);
        assert!(c.is_locked());
        c.unlocked(2_000);
        c.lock_now();
        c.activity(100_000);
        c.unlocked(100_000);
        assert!(!c.tick(159_999));
        assert!(c.tick(160_000));
    }

    #[test]
    fn unlocked_resets_idle() {
        let mut c = LockClock::new(false, policy(0, Some(60)), 0);
        assert!(c.tick(60_000));
        c.unlocked(100_000);
        assert!(!c.tick(150_000));
        assert!(c.tick(160_000));
    }
}
