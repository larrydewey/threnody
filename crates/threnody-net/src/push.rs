//! Waking devices that sleep (`docs/appendix-r-push.md`).
//!
//! A device that turns off its always-on connection hands its mutually
//! approved contacts a UnifiedPush (Web Push) endpoint in `AppMessage::Push`.
//! A contact that then holds something for it (a sealed message in its
//! mailbox, or a message of its own waiting for a session) POSTs an empty
//! wake-up to that endpoint: a fixed-size random body, encrypted to the
//! device (RFC 8291) when it gave keys. The device wakes, connects, takes
//! what is waiting, and sleeps again.
//!
//! Wake-ups are paced per device (jitter, at most one a minute and
//! [`MAX_PER_HOUR`] an hour, bursts coalesced), and padded with cover
//! wake-ups at random times, at the rate the device asked for.

use std::collections::HashMap;
use std::time::Duration;

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map};
use threnody_core::message::FEATURE_PUSH;
use threnody_core::push::{MAX_KEEP_MS, PushReg, PushTarget, WebPushKeys};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::node::{Node, lock};

/// At least this long between two wake-ups of one device.
pub const MIN_GAP_MS: u64 = 60_000;
/// At most this many wake-ups (cover included) of one device an hour.
pub const MAX_PER_HOUR: u32 = 20;
/// A wake-up waits a random time up to this long, so its timing says less
/// about the message that caused it, and messages close together share it.
pub const MAX_JITTER_MS: u64 = 20_000;
/// The shortest such wait.
pub const MIN_JITTER_MS: u64 = 2_000;
/// After the push server says "too many" or fails, wait this long.
pub const BACKOFF_MS: u64 = 10 * 60_000;
/// Cover wake-ups a device asks for unless it says otherwise.
pub const DEFAULT_DUMMIES_PER_DAY: u8 = 4;
/// Size of the random wake-up content, before encryption.
pub const WAKE_CONTENT: usize = 32;
/// How long the push server may keep a wake-up for an unreachable device.
const TTL_S: u32 = 24 * 60 * 60;
/// The aes128gcm record size we declare.
const RECORD_SIZE: u32 = 4096;
const HOUR_MS: u64 = 60 * 60_000;
const DAY_MS: u64 = 24 * HOUR_MS;
const UPKEEP: Duration = Duration::from_secs(15);
const STATE: &str = "push";
const STATE_VERSION: u64 = 1;

/// One device's wake-up budget.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pace {
    last_ms: Option<u64>,
    window_start: u64,
    in_window: u32,
    /// A wake-up is already on its way: later reasons share it.
    scheduled: bool,
    backoff_until: u64,
}

impl Pace {
    /// When to send a wake-up asked for at `now` (with `jitter` added), or
    /// `None` if one already is scheduled or the budget is spent.
    pub(crate) fn plan(&mut self, now: u64, jitter: u64) -> Option<u64> {
        if self.scheduled || now < self.backoff_until {
            return None;
        }
        if now.saturating_sub(self.window_start) >= HOUR_MS {
            self.window_start = now;
            self.in_window = 0;
        }
        if self.in_window >= MAX_PER_HOUR {
            return None;
        }
        self.in_window += 1;
        self.scheduled = true;
        let earliest = self.last_ms.map_or(now, |l| now.max(l + MIN_GAP_MS));
        Some(earliest + jitter)
    }

    /// The wake-up planned last went (or was dropped) at `now`.
    pub(crate) fn fired(&mut self, now: u64, sent: bool) {
        self.scheduled = false;
        if sent {
            self.last_ms = Some(now);
        }
    }

    fn back_off(&mut self, now: u64) {
        self.backoff_until = now + BACKOFF_MS;
    }
}

/// A contact's endpoint, as we keep it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    target: PushTarget,
    /// When we forget it.
    until_ms: u64,
    /// When the next cover wake-up goes (0 = none).
    next_dummy_ms: u64,
}

/// Endpoints we gave out and were given.
#[derive(Default)]
pub(crate) struct PushState {
    /// Ours, handed to mutually approved contacts.
    own: Option<PushTarget>,
    /// We withdrew ours: tell contacts that may still have it.
    withdrawn: bool,
    held: HashMap<[u8; 32], Held>,
    pace: HashMap<[u8; 32], Pace>,
    upkeep: bool,
}

impl PushState {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let own = PushReg(self.own.clone()).encode()?;
        let held: Vec<(&[u8; 32], Vec<u8>, u64)> = self
            .held
            .iter()
            .filter_map(|(k, h)| {
                PushReg(Some(h.target.clone()))
                    .encode()
                    .ok()
                    .map(|b| (k, b, h.until_ms))
            })
            .collect();
        let size = own.len() + held.iter().map(|(_, b, _)| b.len() + 64).sum::<usize>() + 64;
        cbor::to_vec(size, |e| {
            e.map_len(4)?;
            e.u8(0)?.u64(STATE_VERSION)?;
            e.u8(1)?.bytes(&own)?;
            e.u8(2)?.bool(self.withdrawn)?;
            e.u8(3)?.array_len(held.len())?;
            for (k, b, until) in &held {
                e.array_len(3)?.bytes(*k)?.bytes(b)?.u64(*until)?;
            }
            Ok(())
        })
    }

    fn decode(b: &[u8], now: u64) -> threnody_core::Result<Self> {
        let mut me = Self::default();
        let mut dec = Decoder::new(b);
        let mut ver = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => me.own = PushReg::decode(d.bytes()?)?.0,
                2 => me.withdrawn = d.bool()?,
                3 => {
                    for _ in 0..d.array_len()? {
                        if d.array_len()? != 3 {
                            return Err(threnody_core::Error::Malformed("push entry"));
                        }
                        let k = fixed_bytes::<32>(d)?;
                        let reg = PushReg::decode(d.bytes()?)?;
                        let until_ms = d.u64()?;
                        if let Some(target) = reg.0
                            && until_ms > now
                        {
                            let next_dummy_ms = next_dummy(now, target.dummies_per_day);
                            me.held.insert(
                                k,
                                Held {
                                    target,
                                    until_ms,
                                    next_dummy_ms,
                                },
                            );
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(STATE_VERSION) {
            return Err(threnody_core::Error::Malformed("push state version"));
        }
        Ok(me)
    }
}

/// A uniform random number in `[0, 1)`.
fn unit() -> f64 {
    let r = u64::from_le_bytes(threnody_core::crypto::random_bytes());
    (r >> 11) as f64 / (1u64 << 53) as f64
}

/// When the next cover wake-up goes: exponential gaps (a Poisson process)
/// averaging `per_day` a day, so their times say nothing. 0 = none.
fn next_dummy(now: u64, per_day: u8) -> u64 {
    if per_day == 0 {
        return 0;
    }
    let mean = DAY_MS as f64 / f64::from(per_day);
    let gap = -(1.0 - unit()).ln() * mean;
    now + (gap as u64).max(1)
}

fn jitter() -> u64 {
    MIN_JITTER_MS + (unit() * (MAX_JITTER_MS - MIN_JITTER_MS) as f64) as u64
}

// ----- Web Push message encryption (RFC 8291, RFC 8188 aes128gcm) -----

struct Len(usize);

impl ring::hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[&[u8]], out: &mut [u8]) -> Option<()> {
    ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt)
        .extract(ikm)
        .expand(info, Len(out.len()))
        .ok()?
        .fill(out)
        .ok()
}

/// The (content encryption key, nonce) RFC 8291 derives.
fn webpush_keys(
    ecdh: &[u8],
    auth: &[u8; 16],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8; 16],
) -> Option<([u8; 16], [u8; 12])> {
    let mut ikm = [0u8; 32];
    hkdf(
        auth,
        ecdh,
        &[b"WebPush: info\0", ua_public, as_public],
        &mut ikm,
    )?;
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    hkdf(salt, &ikm, &[b"Content-Encoding: aes128gcm\0"], &mut cek)?;
    hkdf(salt, &ikm, &[b"Content-Encoding: nonce\0"], &mut nonce)?;
    Some((cek, nonce))
}

/// `content` encrypted to `keys` as one aes128gcm record: salt, record
/// size, our ephemeral public key, then the ciphertext.
pub(crate) fn webpush_encrypt(keys: &WebPushKeys, content: &[u8]) -> Option<Vec<u8>> {
    use ring::agreement::{ECDH_P256, EphemeralPrivateKey, UnparsedPublicKey, agree_ephemeral};
    let rng = ring::rand::SystemRandom::new();
    let ours = EphemeralPrivateKey::generate(&ECDH_P256, &rng).ok()?;
    let as_public = ours.compute_public_key().ok()?.as_ref().to_vec();
    let ecdh = agree_ephemeral(
        ours,
        &UnparsedPublicKey::new(&ECDH_P256, &keys.p256dh),
        |s| s.to_vec(),
    )
    .ok()?;
    let salt: [u8; 16] = threnody_core::crypto::random_bytes();
    let (cek, nonce) = webpush_keys(&ecdh, &keys.auth, &keys.p256dh, &as_public, &salt)?;
    let mut record = content.to_vec();
    record.push(2); // last-record delimiter, no padding
    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).ok()?,
    );
    key.seal_in_place_append_tag(
        ring::aead::Nonce::assume_unique_for_key(nonce),
        ring::aead::Aad::empty(),
        &mut record,
    )
    .ok()?;
    // One record of at most RECORD_SIZE bytes (what Web Push receivers
    // accept by default).
    if record.len() > RECORD_SIZE as usize {
        return None;
    }
    let rs = RECORD_SIZE;
    let mut out = Vec::with_capacity(21 + as_public.len() + record.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&rs.to_be_bytes());
    out.push(u8::try_from(as_public.len()).ok()?);
    out.extend_from_slice(&as_public);
    out.extend_from_slice(&record);
    Some(out)
}

/// The body and headers of one wake-up to `t`: always the same size and
/// headers, whatever caused it.
fn wake_request(t: &PushTarget) -> Option<(Vec<u8>, bool)> {
    let content: [u8; WAKE_CONTENT] = threnody_core::crypto::random_bytes();
    match &t.keys {
        Some(k) => webpush_encrypt(k, &content).map(|b| (b, true)),
        None => Some((content.to_vec(), false)),
    }
}

/// What the push server said.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Sent,
    /// The endpoint is gone (unregistered): forget it.
    Gone,
    /// Rate-limited or failing: back off.
    Later,
}

async fn post(client: &reqwest::Client, t: &PushTarget) -> Outcome {
    let Some((body, encrypted)) = wake_request(t) else {
        return Outcome::Later;
    };
    let mut req = client
        .post(&t.url)
        .header("TTL", TTL_S.to_string())
        .header("Urgency", "high")
        .header("Content-Type", "application/octet-stream")
        .body(body);
    if encrypted {
        req = req.header("Content-Encoding", "aes128gcm");
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => Outcome::Sent,
        Ok(r) if matches!(r.status().as_u16(), 404 | 410) => Outcome::Gone,
        // A request the server will never take (too large, malformed):
        // as good as gone.
        Ok(r) if matches!(r.status().as_u16(), 400 | 401 | 403 | 413) => Outcome::Gone,
        _ => Outcome::Later,
    }
}

fn client() -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()
        })
        .clone()
}

/// What a wake-up sync found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeReport {
    /// Mutually approved contacts.
    pub contacts: usize,
    /// Of them, connected when it ended.
    pub connected: usize,
}

impl Node {
    fn push(&self) -> std::sync::MutexGuard<'_, PushState> {
        lock(&self.shared.push)
    }

    pub(crate) fn load_push(&self) {
        let state = self
            .shared
            .home
            .load_state(self.identity_ref(), STATE)
            .ok()
            .flatten()
            .and_then(|b| PushState::decode(&b, now_ms()).ok());
        if let Some(s) = state {
            *self.push() = s;
        }
        self.start_push_upkeep();
    }

    fn persist_push(&self, s: &PushState) {
        self.shared.save_state(STATE, s.encode());
    }

    /// Sets where contacts wake this device (`None`: they no longer may),
    /// and tells the mutually approved ones connected now; the rest learn
    /// it in their next session. Not for anonymous identities: one push
    /// server seeing both would tie them together.
    pub fn set_push_target(&self, target: Option<PushTarget>) -> Result<()> {
        if self.is_persona() && target.is_some() {
            return Err(NetError::NotAllowed(
                "anonymous identities are never woken by push".into(),
            ));
        }
        if let Some(t) = &target {
            t.validate()?;
        }
        {
            let mut s = self.push();
            if s.own == target {
                return Ok(());
            }
            s.withdrawn = target.is_none() && (s.own.is_some() || s.withdrawn);
            s.own = target;
            self.persist_push(&s);
        }
        if let Some(m) = self.push_message() {
            for s in self.sessions() {
                if self.shared.mutual(&s.peer) && self.supports(&s.peer, FEATURE_PUSH) {
                    let _ = self.send(&s.peer, m.clone());
                }
            }
        }
        Ok(())
    }

    /// Our endpoint, if contacts may wake us.
    pub fn push_target(&self) -> Option<PushTarget> {
        self.push().own.clone()
    }

    /// How many contacts gave us an endpoint to wake them at.
    pub fn push_targets(&self) -> usize {
        self.push().held.len()
    }

    /// Our registration (or its withdrawal), for a session's setup; `None`
    /// when there is nothing to say.
    pub(crate) fn push_message(&self) -> Option<AppMessage> {
        let s = self.push();
        let reg = match &s.own {
            Some(t) => {
                let mut t = t.clone();
                t.expires_ms = now_ms() + MAX_KEEP_MS;
                PushReg(Some(t))
            }
            None if s.withdrawn => PushReg(None),
            None => return None,
        };
        reg.encode().ok().map(AppMessage::Push)
    }

    /// A contact's registration or withdrawal.
    pub(crate) fn on_push(&self, from: PublicIdentity, payload: &[u8]) {
        // Anonymous identities don't wake anyone: the request would show
        // the push server this device's address.
        if self.is_persona() || !self.shared.mutual(&from) {
            return;
        }
        let Ok(PushReg(reg)) = PushReg::decode(payload) else {
            return;
        };
        let now = now_ms();
        let mut s = self.push();
        match reg {
            Some(target) => {
                let until_ms = target.expires_ms.min(now + MAX_KEEP_MS);
                let next_dummy_ms = match s.held.get(from.as_bytes()) {
                    Some(h) if h.target.dummies_per_day == target.dummies_per_day => {
                        h.next_dummy_ms
                    }
                    _ => next_dummy(now, target.dummies_per_day),
                };
                s.held.insert(
                    *from.as_bytes(),
                    Held {
                        target,
                        until_ms,
                        next_dummy_ms,
                    },
                );
            }
            None => {
                if s.held.remove(from.as_bytes()).is_none() {
                    return;
                }
            }
        }
        self.persist_push(&s);
    }

    /// `peer` is no longer mutually approved: forget its endpoint.
    pub(crate) fn forget_push(&self, peer: &PublicIdentity) {
        let mut s = self.push();
        if s.held.remove(peer.as_bytes()).is_some() {
            self.persist_push(&s);
        }
    }

    /// We hold something for `dest` (a sealed message, or a message of our
    /// own waiting for a session): wake it, if it gave us an endpoint and
    /// its budget allows. Returns whether a wake-up is now on its way.
    pub(crate) fn push_wake(&self, dest: &PublicIdentity) -> bool {
        self.plan_wake(dest, jitter())
    }

    fn plan_wake(&self, dest: &PublicIdentity, jitter: u64) -> bool {
        if self.is_persona() {
            return false;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let now = now_ms();
        let at = {
            let mut s = self.push();
            match s.held.get(dest.as_bytes()) {
                Some(h) if h.until_ms > now => {}
                _ => return false,
            }
            match s
                .pace
                .entry(*dest.as_bytes())
                .or_default()
                .plan(now, jitter)
            {
                Some(at) => at,
                None => return false,
            }
        };
        let (node, dest) = (self.clone(), *dest);
        handle.spawn(async move {
            tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()))).await;
            node.fire_wake(&dest).await;
        });
        true
    }

    async fn fire_wake(&self, dest: &PublicIdentity) {
        let target = {
            let s = self.push();
            s.held
                .get(dest.as_bytes())
                .filter(|h| h.until_ms > now_ms())
                .map(|h| h.target.clone())
        };
        // Connected again by now, or no longer a contact: nothing to wake.
        let pointless = self.is_closed()
            || !self.shared.mutual(dest)
            || self.sessions().iter().any(|s| s.peer == *dest);
        let outcome = match (target, pointless, client()) {
            (Some(t), false, Some(c)) => Some(post(&c, &t).await),
            _ => None,
        };
        let now = now_ms();
        let mut s = self.push();
        let pace = s.pace.entry(*dest.as_bytes()).or_default();
        pace.fired(now, outcome == Some(Outcome::Sent));
        match outcome {
            Some(Outcome::Later) => pace.back_off(now),
            Some(Outcome::Gone) => {
                s.held.remove(dest.as_bytes());
                self.persist_push(&s);
            }
            _ => {}
        }
    }

    /// Sends the cover wake-ups that are due, and forgets expired endpoints.
    fn push_upkeep(&self) {
        let now = now_ms();
        let due: Vec<PublicIdentity> = {
            let mut s = self.push();
            let before = s.held.len();
            s.held.retain(|_, h| h.until_ms > now);
            if s.held.len() != before {
                self.persist_push(&s);
            }
            let mut due = Vec::new();
            for (k, h) in &mut s.held {
                if h.next_dummy_ms != 0 && h.next_dummy_ms <= now {
                    h.next_dummy_ms = next_dummy(now, h.target.dummies_per_day);
                    due.extend(PublicIdentity::from_bytes(k).ok());
                }
            }
            due
        };
        for d in due {
            // A cover wake-up goes exactly like a real one: same budget,
            // same jitter, same request.
            self.push_wake(&d);
        }
    }

    fn start_push_upkeep(&self) {
        {
            let mut s = self.push();
            if s.upkeep {
                return;
            }
            s.upkeep = true;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.push().upkeep = false;
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            let mut tick = tokio::time::interval(UPKEEP);
            loop {
                tokio::select! {
                    _ = tick.tick() => node.push_upkeep(),
                    () = &mut closed => break,
                }
            }
        });
    }

    /// Woken by a push: dials every mutually approved contact and waits
    /// until what they held for us has had time to arrive. Ends when every
    /// contact is connected and things have settled, when no new contact
    /// has connected for a while, or at `timeout`.
    pub async fn wake_sync(&self, timeout: Duration) -> WakeReport {
        let start = tokio::time::Instant::now();
        let deadline = start + timeout;
        let mutual: Vec<PublicIdentity> = self
            .contacts()
            .iter()
            .filter(|c| c.mutually_approved())
            .map(|c| c.key)
            .collect();
        for peer in &mutual {
            let (node, peer) = (self.clone(), *peer);
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(30), node.reach_peer(&peer)).await;
            });
        }
        // Held messages go one per cover tick: leave room for a few.
        let settle = self.constant_rate().map_or(Duration::from_secs(3), |r| {
            (r * 3).max(Duration::from_secs(3))
        });
        let quiet = settle.max(Duration::from_secs(15));
        let (mut connected, mut changed) = (0, start);
        loop {
            let live = self.sessions();
            let now_connected = mutual
                .iter()
                .filter(|p| live.iter().any(|s| s.peer == **p))
                .count();
            let now = tokio::time::Instant::now();
            if now_connected != connected {
                connected = now_connected;
                changed = now;
            }
            let idle = now - changed;
            let done = if connected == mutual.len() {
                idle >= settle
            } else {
                connected > 0 && idle >= quiet
            };
            if done || now >= deadline || self.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        WakeReport {
            contacts: mutual.len(),
            connected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pace_coalesces_spaces_and_caps() {
        let mut p = Pace::default();
        assert_eq!(p.plan(1000, 5), Some(1005));
        assert_eq!(p.plan(1001, 5), None, "coalesced into the scheduled one");
        p.fired(1005, true);
        // The next one waits for the minimum gap.
        assert_eq!(p.plan(2000, 7), Some(1005 + MIN_GAP_MS + 7));
        p.fired(1005 + MIN_GAP_MS + 7, true);
        let mut t = 1005 + MIN_GAP_MS + 7;
        for _ in 2..MAX_PER_HOUR {
            t += MIN_GAP_MS;
            assert!(p.plan(t, 0).is_some());
            p.fired(t, true);
        }
        assert_eq!(p.plan(t + MIN_GAP_MS, 0), None, "hourly cap");
        assert!(p.plan(1000 + HOUR_MS + 1, 0).is_some(), "next hour");
        p.fired(1000 + HOUR_MS + 1, false);
        p.back_off(1000 + HOUR_MS + 2);
        assert_eq!(p.plan(1000 + HOUR_MS + 3, 0), None, "backing off");
        assert!(p.plan(1000 + HOUR_MS + 2 + BACKOFF_MS, 0).is_some());
    }

    #[test]
    fn cover_wakeups_average_their_rate() {
        assert_eq!(next_dummy(5, 0), 0);
        let n = 4000;
        let total: u64 = (0..n).map(|_| next_dummy(0, 4)).sum();
        let mean = total as f64 / f64::from(n);
        let want = DAY_MS as f64 / 4.0;
        assert!((mean - want).abs() < want * 0.1, "mean gap {mean}");
        for _ in 0..1000 {
            let j = jitter();
            assert!((MIN_JITTER_MS..=MAX_JITTER_MS).contains(&j));
        }
    }

    /// Decrypts as the device (the user agent) would, RFC 8291 §3.
    fn decrypt(
        ua: ring::agreement::EphemeralPrivateKey,
        ua_public: &[u8],
        auth: &[u8; 16],
        body: &[u8],
    ) -> Vec<u8> {
        use ring::agreement::{ECDH_P256, UnparsedPublicKey, agree_ephemeral};
        let salt: [u8; 16] = body[..16].try_into().unwrap();
        let idlen = body[20] as usize;
        let as_public = &body[21..21 + idlen];
        let ecdh = agree_ephemeral(ua, &UnparsedPublicKey::new(&ECDH_P256, as_public), |s| {
            s.to_vec()
        })
        .unwrap();
        let (cek, nonce) = webpush_keys(&ecdh, auth, ua_public, as_public, &salt).unwrap();
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &cek).unwrap(),
        );
        let mut ct = body[21 + idlen..].to_vec();
        let pt = key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce),
                ring::aead::Aad::empty(),
                &mut ct,
            )
            .unwrap();
        assert_eq!(pt.last(), Some(&2), "last-record delimiter");
        pt[..pt.len() - 1].to_vec()
    }

    #[test]
    fn webpush_wakeups_decrypt_and_all_look_alike() {
        let rng = ring::rand::SystemRandom::new();
        let ua = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
            .unwrap();
        let ua_public: [u8; 65] = ua
            .compute_public_key()
            .unwrap()
            .as_ref()
            .try_into()
            .unwrap();
        let keys = WebPushKeys {
            p256dh: ua_public,
            auth: [3; 16],
        };
        let body = webpush_encrypt(&keys, b"hello").unwrap();
        assert_eq!(decrypt(ua, &ua_public, &keys.auth, &body), b"hello");
        assert_eq!(
            u32::from_be_bytes(body[16..20].try_into().unwrap()),
            RECORD_SIZE
        );

        let t = PushTarget {
            url: "https://push.example/x".into(),
            keys: Some(keys),
            dummies_per_day: 0,
            expires_ms: 0,
        };
        let (a, enc) = wake_request(&t).unwrap();
        let (b, _) = wake_request(&t).unwrap();
        assert!(enc);
        assert_eq!(a.len(), b.len());
        assert_ne!(a, b);
        assert_eq!(a.len(), 16 + 4 + 1 + 65 + WAKE_CONTENT + 1 + 16);
        let plain = PushTarget { keys: None, ..t };
        let (c, enc) = wake_request(&plain).unwrap();
        assert!(!enc);
        assert_eq!(c.len(), WAKE_CONTENT);
    }

    #[test]
    fn state_round_trips_and_drops_expired() {
        let t = |u: &str| PushTarget {
            url: u.into(),
            keys: None,
            dummies_per_day: 2,
            expires_ms: 0,
        };
        let mut s = PushState {
            own: Some(t("https://a/x")),
            withdrawn: false,
            ..PushState::default()
        };
        for (k, until) in [([1; 32], 100), ([2; 32], 5000)] {
            s.held.insert(
                k,
                Held {
                    target: t("https://b/y"),
                    until_ms: until,
                    next_dummy_ms: 0,
                },
            );
        }
        let back = PushState::decode(&s.encode().unwrap(), 1000).unwrap();
        assert_eq!(back.own, s.own);
        assert_eq!(back.held.len(), 1);
        assert_eq!(back.held[&[2; 32]].target, t("https://b/y"));
        assert!(back.held[&[2; 32]].next_dummy_ms > 1000);
    }
}
