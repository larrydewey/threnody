//! Wake-up endpoints (`docs/appendix-r-push.md`): where a device that sleeps
//! can be woken, handed to its mutually approved contacts inside
//! `AppMessage::Push`.
//!
//! ```text
//! PushReg = { ? 1: url tstr, ? 2: p256dh bstr(65), ? 3: auth bstr(16),
//!             ? 4: dummies_per_day uint, ? 5: expires_ms uint }
//! ```
//!
//! An empty map withdraws the endpoint. The wake-up itself carries nothing:
//! a fixed-size random body, encrypted to `p256dh`/`auth` (RFC 8291) when
//! the device gave them.

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map};
use crate::error::{Error, Result};

/// Longest endpoint URL accepted.
pub const MAX_URL: usize = 1024;
/// Most cover wake-ups per day a device may ask a contact for.
pub const MAX_DUMMIES_PER_DAY: u8 = 24;
/// How long a contact keeps an endpoint at most, whatever it says.
pub const MAX_KEEP_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Web Push keys (RFC 8291) the wake-up is encrypted to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPushKeys {
    /// The device's P-256 public key, uncompressed (`0x04 ‖ x ‖ y`).
    pub p256dh: [u8; 65],
    /// The shared authentication secret.
    pub auth: [u8; 16],
}

/// Where and how to wake a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushTarget {
    /// The UnifiedPush (Web Push) endpoint: `https://`, or `http://` to a
    /// loopback address (tests, a distributor on the same machine).
    pub url: String,
    pub keys: Option<WebPushKeys>,
    /// Cover wake-ups per day the contact sends, at random times.
    pub dummies_per_day: u8,
    /// When the contact forgets it (capped at [`MAX_KEEP_MS`] from receipt).
    pub expires_ms: u64,
}

/// A registration, or its withdrawal (`None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushReg(pub Option<PushTarget>);

impl PushTarget {
    /// Checks the URL: `https://` with a host, or `http://` to loopback;
    /// no user info, no whitespace, at most [`MAX_URL`] bytes.
    pub fn validate(&self) -> Result<()> {
        valid_url(&self.url)?;
        if self.dummies_per_day > MAX_DUMMIES_PER_DAY {
            return Err(Error::Malformed("push dummies"));
        }
        if let Some(k) = &self.keys
            && k.p256dh[0] != 0x04
        {
            return Err(Error::Malformed("push key"));
        }
        Ok(())
    }
}

/// The URL's scheme, host and port check of [`PushTarget::validate`].
pub fn valid_url(url: &str) -> Result<()> {
    let bad = Error::Malformed("push endpoint");
    if url.len() > MAX_URL || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad);
    }
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(bad);
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(bad);
    }
    if !secure {
        let host = match authority.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
        };
        let loopback = host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(bad);
        }
    }
    Ok(())
}

impl PushReg {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let Some(t) = &self.0 else {
            return cbor::to_vec(8, |e| {
                e.map_len(0)?;
                Ok(())
            });
        };
        cbor::to_vec(t.url.len() + 128, |e| {
            e.map_len(3 + 2 * usize::from(t.keys.is_some()))?;
            e.u8(1)?.str(&t.url)?;
            if let Some(k) = &t.keys {
                e.u8(2)?.bytes(&k.p256dh)?;
                e.u8(3)?.bytes(&k.auth)?;
            }
            e.u8(4)?.u8(t.dummies_per_day)?;
            e.u8(5)?.u64(t.expires_ms)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut url, mut p256dh, mut auth, mut dummies, mut expires) = (None, None, None, 0, 0);
        read_map(&mut dec, |k, d| {
            match k {
                1 => url = Some(d.str()?.to_owned()),
                2 => p256dh = Some(fixed_bytes::<65>(d)?),
                3 => auth = Some(fixed_bytes::<16>(d)?),
                4 => dummies = d.u8()?,
                5 => expires = d.u64()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let keys = match (p256dh, auth) {
            (Some(p256dh), Some(auth)) => Some(WebPushKeys { p256dh, auth }),
            (None, None) => None,
            _ => return Err(Error::Malformed("push keys")),
        };
        let Some(url) = url else {
            if keys.is_some() {
                return Err(Error::Malformed("push keys without endpoint"));
            }
            return Ok(Self(None));
        };
        let t = PushTarget {
            url,
            keys,
            dummies_per_day: dummies,
            expires_ms: expires,
        };
        t.validate()?;
        Ok(Self(Some(t)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> PushTarget {
        PushTarget {
            url: "https://push.example.org/up/abc?x=1".into(),
            keys: Some(WebPushKeys {
                p256dh: {
                    let mut k = [7; 65];
                    k[0] = 4;
                    k
                },
                auth: [9; 16],
            }),
            dummies_per_day: 4,
            expires_ms: 123,
        }
    }

    #[test]
    fn registrations_round_trip() {
        let mut plain = target();
        plain.keys = None;
        for r in [PushReg(Some(target())), PushReg(Some(plain)), PushReg(None)] {
            assert_eq!(PushReg::decode(&r.encode().unwrap()).unwrap(), r);
        }
    }

    #[test]
    fn urls_are_checked() {
        for ok in [
            "https://ntfy.sh/upAbC",
            "https://[2001:db8::1]:8443/x",
            "http://127.0.0.1:9/x",
            "http://localhost/x",
            "http://[::1]:80/",
        ] {
            assert!(valid_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://ntfy.sh/up",
            "ftp://x/y",
            "https://",
            "https://user@host/x",
            "https://host/a b",
            "http://127.0.0.1.evil.example/x",
            "javascript:alert(1)",
        ] {
            assert!(valid_url(bad).is_err(), "{bad}");
        }
        assert!(valid_url(&format!("https://h/{}", "a".repeat(MAX_URL))).is_err());
        let mut t = target();
        t.dummies_per_day = MAX_DUMMIES_PER_DAY + 1;
        assert!(PushReg::decode(&PushReg(Some(t)).encode().unwrap()).is_err());
    }

    #[test]
    fn half_a_key_pair_is_malformed() {
        let b = cbor::to_vec(64, |e| {
            e.map_len(2)?.u8(1)?.str("https://h/x")?;
            e.u8(3)?.bytes(&[0; 16])?;
            Ok(())
        })
        .unwrap();
        assert!(PushReg::decode(&b).is_err());
    }
}
