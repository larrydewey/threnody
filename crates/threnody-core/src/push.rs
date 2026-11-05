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
    /// The UnifiedPush (Web Push) endpoint: `https://` to a public host.
    /// The contact, not the device, sends to it, so nothing local to the
    /// device (loopback, private networks) makes sense, and it would point
    /// the contact at its own network.
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
    /// Checks the URL: `https://` to a public host (see [`valid_url`]);
    /// no user info, no whitespace, at most [`MAX_URL`] bytes.
    pub fn validate(&self) -> Result<()> {
        self.validate_with(false)
    }

    /// [`PushTarget::validate`], also taking loopback endpoints (plain
    /// `http://` too) with `loopback`: for tests on one machine only.
    pub fn validate_with(&self, loopback: bool) -> Result<()> {
        valid_url_with(&self.url, loopback)?;
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

/// The URL's scheme and host check of [`PushTarget::validate`]: `https://`
/// to a public host. Not an address on loopback, a private network,
/// link-local or otherwise not routed on the internet, nor `localhost`, a
/// single-label name (which resolve on the local network) or a name with a
/// trailing dot. Hosts are
/// read as an HTTP client reads them, so `https://2130706433/` is
/// 127.0.0.1. A name that resolves to such an address is the sender's to
/// refuse when it connects.
pub fn valid_url(url: &str) -> Result<()> {
    valid_url_with(url, false)
}

/// [`valid_url`], also taking loopback hosts, over plain `http://` too,
/// with `loopback`: for tests on one machine only.
pub fn valid_url_with(url: &str, loopback: bool) -> Result<()> {
    let bad = Error::Malformed("push endpoint");
    if url.len() > MAX_URL || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(bad);
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or(Error::Malformed("push endpoint"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(bad);
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| Error::Malformed("push endpoint"))?;
    let host = parsed.host_str().unwrap_or("");
    let ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .parse::<std::net::IpAddr>()
        .ok();
    let local = match ip {
        Some(ip) => ip.is_loopback(),
        None => host == "localhost" || host.ends_with(".localhost"),
    };
    if loopback && local {
        return Ok(());
    }
    let public = match ip {
        Some(ip) => public_ip(&ip),
        None => !local && host.contains('.') && !host.ends_with('.'),
    };
    if parsed.scheme() != "https" || !public {
        return Err(bad);
    }
    Ok(())
}

/// Whether `ip` is routed on the public internet: not loopback, private,
/// shared (CGNAT), link-local, unspecified, broadcast, multicast,
/// documentation, benchmarking or reserved space, including IPv4 addresses
/// embedded in IPv6 ones.
pub fn public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && b & 0xc0 == 64)
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && b & 0xfe == 18))
        }
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            let embedded = |o: [u16; 2]| {
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                    (o[0] >> 8) as u8,
                    o[0] as u8,
                    (o[1] >> 8) as u8,
                    o[1] as u8,
                ))
            };
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_ip(&std::net::IpAddr::V4(v4));
            }
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) carry IPv4 ones.
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return public_ip(&embedded([seg[6], seg[7]]));
            }
            if seg[0] == 0x2002 {
                return public_ip(&embedded([seg[1], seg[2]]));
            }
            // Global unicast (2000::/3), less documentation (2001:db8::/32).
            seg[0] & 0xe000 == 0x2000 && !(seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
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
        Self::decode_with(b, false)
    }

    /// [`PushReg::decode`], also taking loopback endpoints with
    /// `loopback` (see [`PushTarget::validate_with`]).
    pub fn decode_with(b: &[u8], loopback: bool) -> Result<Self> {
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
        t.validate_with(loopback)?;
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
            "https://[2606:4700::1111]:8443/x",
            "https://1.1.1.1/x?y=1",
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
        // Nothing on the waker's own machine or network: a contact's URL
        // is fetched by us.
        for local in [
            "http://127.0.0.1:2375/containers/web/kill",
            "http://localhost/x",
            "http://[::1]:80/",
            "https://127.0.0.1/x",
            "https://localhost:8443/x",
            "https://api.localhost/x",
            "https://router/x",
            "https://localhost./x",
            "https://10.0.0.1/x",
            "https://192.168.1.1/admin",
            "https://172.16.5.4/x",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:127.0.0.1]/x",
            "https://[64:ff9b::a00:1]/x",
            "https://[2002:c0a8:101::1]/x",
            "https://[2001:db8::1]/x",
            "https://2130706433/x",
            "https://0x7f.1/x",
            "https://017700000001/x",
        ] {
            assert!(valid_url(local).is_err(), "{local}");
        }
        // Only tests on one machine take loopback.
        for test in [
            "http://127.0.0.1:9/x",
            "http://localhost/x",
            "https://[::1]/",
        ] {
            assert!(valid_url_with(test, true).is_ok(), "{test}");
        }
        assert!(valid_url_with("https://192.168.1.1/x", true).is_err());
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
