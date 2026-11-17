//! Invite links: who to expect, where they were, and a secret to find them
//! by across the internet when none of those addresses answer.
//!
//! ```text
//! link  = "threnody://" fingerprint [ "@" addr *( "," addr ) ] [ "?r=" key ] [ "&m=" meet ]
//! meet  = Crockford Base32 of relay fingerprint (20) || cookie (16), "@", addr
//! addr  = host ":" port            -- IPv6 in brackets: [2001:db8::1]:7450
//! key   = Crockford Base32 of 16 random bytes (26 symbols)
//! base  = KDF("… invite", key)     -- a meeting place, as a handle's is (Appendix S)
//! ```
//!
//! The addresses are the inviter's at the time (global IPv6 first, then
//! public, then local). A phone on mobile data has none that take a dial,
//! so the key names a meeting place in the DHT: the inviter keeps its
//! current addresses there and answers knocks, exactly as for a handle.
//! The fingerprint, not a code, is what makes a record there trustworthy.
//! Links without a key, or with one address and no list, still parse.

use std::fmt;
use std::str::FromStr;

use crate::crypto::kdf::derive;
use crate::crypto::random_bytes;
use crate::error::{Error, Result};
use crate::handle::Meeting;
use crate::identity::{CROCKFORD, Fingerprint, crockford_value};

pub const SCHEME: &str = "threnody://";
/// Most addresses a link carries (and that a finder dials).
pub const MAX_ADDRS: usize = 4;
const KEY_LEN: usize = 16;
const KEY_SYMBOLS: usize = 26;
const BASE: &str = "threnody v1 2026-10-28 invite";

/// The secret an invite link carries to find its maker in the DHT.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct InviteKey([u8; KEY_LEN]);

impl InviteKey {
    pub fn generate() -> Self {
        Self(random_bytes())
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        b.try_into()
            .map(Self)
            .map_err(|_| Error::Malformed("invite key length"))
    }

    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Meeting for InviteKey {
    fn base(&self) -> [u8; 32] {
        derive(BASE, &[&self.0])
    }
}

impl fmt::Display for InviteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut acc: u32 = 0;
        let mut bits = 0;
        for &b in &self.0 {
            acc = (acc << 8) | u32::from(b);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                write!(f, "{}", CROCKFORD[((acc >> bits) & 31) as usize] as char)?;
            }
        }
        // 128 bits: three left over, padded with zeros.
        write!(
            f,
            "{}",
            CROCKFORD[((acc << (5 - bits)) & 31) as usize] as char
        )
    }
}

impl fmt::Debug for InviteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InviteKey(…)")
    }
}

impl FromStr for InviteKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::Malformed("invite key");
        let syms = s
            .trim()
            .bytes()
            .filter(|&b| b != b'-')
            .map(|b| crockford_value(b).ok_or_else(bad))
            .collect::<Result<Vec<u8>>>()?;
        if syms.len() != KEY_SYMBOLS {
            return Err(bad());
        }
        let mut out = [0u8; KEY_LEN];
        let (mut acc, mut bits, mut i) = (0u32, 0, 0);
        for v in syms {
            acc = (acc << 5) | u32::from(v);
            bits += 5;
            if bits >= 8 && i < KEY_LEN {
                bits -= 8;
                out[i] = (acc >> bits) as u8;
                i += 1;
            }
        }
        Ok(Self(out))
    }
}

/// A parsed invite link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteLink {
    pub fingerprint: Fingerprint,
    /// `host:port`, IPv6 hosts in brackets; best first.
    pub addrs: Vec<String>,
    pub key: Option<InviteKey>,
    /// A meeting point (Appendix N) the inviter listens at: how an
    /// anonymous identity, whose links carry no address, is reached.
    pub meet: Option<crate::rendezvous::MeetingPoint>,
}

/// Crockford Base32 of `bytes`, zero bits padding the last symbol.
fn to_b32(bytes: &[u8]) -> String {
    let mut out = String::new();
    let (mut acc, mut bits) = (0u32, 0);
    for &b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(CROCKFORD[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(CROCKFORD[((acc << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// `n` bytes from Crockford Base32 made by [`to_b32`].
fn from_b32(s: &str, n: usize) -> Option<Vec<u8>> {
    let syms: Vec<u8> = s.bytes().map(crockford_value).collect::<Option<_>>()?;
    if syms.len() != (n * 8).div_ceil(5) {
        return None;
    }
    let mut out = Vec::with_capacity(n);
    let (mut acc, mut bits) = (0u32, 0);
    for v in syms {
        acc = (acc << 5) | u32::from(v);
        bits += 5;
        if bits >= 8 && out.len() < n {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (out.len() == n).then_some(out)
}

fn parse_meet(v: &str) -> Result<crate::rendezvous::MeetingPoint> {
    let bad = || Error::Malformed("invite link meeting point");
    let (code, addr) = v.split_once('@').ok_or_else(bad)?;
    let b = from_b32(code, 36).ok_or_else(bad)?;
    let addr: std::net::SocketAddr = addr.parse().map_err(|_| bad())?;
    Ok(crate::rendezvous::MeetingPoint {
        relay: Fingerprint(b[..20].try_into().map_err(|_| bad())?),
        addr,
        cookie: b[20..].try_into().map_err(|_| bad())?,
    })
}

impl fmt::Display for InviteLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{SCHEME}{}", self.fingerprint.compact())?;
        if !self.addrs.is_empty() {
            write!(f, "@{}", self.addrs.join(","))?;
        }
        let mut sep = '?';
        if let Some(k) = &self.key {
            write!(f, "?r={k}")?;
            sep = '&';
        }
        if let Some(m) = &self.meet {
            let mut b = m.relay.0.to_vec();
            b.extend_from_slice(&m.cookie);
            write!(f, "{sep}m={}@{}", to_b32(&b), m.addr)?;
        }
        Ok(())
    }
}

impl FromStr for InviteLink {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = Error::Malformed("invite link (threnody://<fingerprint>@<host:port>)");
        let rest = s.trim().strip_prefix(SCHEME).ok_or(bad)?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let rest = rest.trim_end_matches('/');
        let (fp, addrs) = rest.split_once('@').unwrap_or((rest, ""));
        let (mut key, mut meet) = (None, None);
        for pair in query.split('&') {
            if let Some(k) = pair.strip_prefix("r=") {
                key = Some(k.parse()?);
            } else if let Some(m) = pair.strip_prefix("m=") {
                meet = Some(parse_meet(m)?);
            }
        }
        let addrs: Vec<String> = addrs
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .take(MAX_ADDRS)
            .map(str::to_owned)
            .collect();
        if addrs.is_empty() && key.is_none() && meet.is_none() {
            return Err(Error::Malformed("invite link has no address"));
        }
        if addrs.iter().any(|a| !is_host_port(a)) {
            return Err(Error::Malformed("invite link address (host:port)"));
        }
        Ok(Self {
            fingerprint: fp.parse()?,
            addrs,
            key,
            meet,
        })
    }
}

/// `host:port`, with an IPv6 host in brackets.
fn is_host_port(a: &str) -> bool {
    let Some((host, port)) = a.rsplit_once(':') else {
        return false;
    };
    let host_ok = match host.strip_prefix('[') {
        Some(v6) => v6
            .strip_suffix(']')
            .is_some_and(|ip| ip.parse::<std::net::Ipv6Addr>().is_ok()),
        None => !host.is_empty() && !host.contains(':'),
    };
    host_ok && port.parse::<u16>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    #[test]
    fn keys_print_and_parse() {
        let k = InviteKey::generate();
        let s = k.to_string();
        assert_eq!(s.len(), KEY_SYMBOLS);
        assert_eq!(s.parse::<InviteKey>().unwrap(), k);
        assert_eq!(s.to_lowercase().parse::<InviteKey>().unwrap(), k);
        assert!(s[1..].parse::<InviteKey>().is_err());
        assert_ne!(k.base(), InviteKey::generate().base());
        assert_eq!(format!("{k:?}"), "InviteKey(…)");
    }

    #[test]
    fn links_round_trip_with_ipv6_and_keys() {
        let fp = Identity::generate().public().fingerprint();
        let link = InviteLink {
            fingerprint: fp,
            addrs: vec!["[2001:db8::7]:7450".into(), "198.51.100.4:7450".into()],
            key: Some(InviteKey::generate()),
            meet: None,
        };
        let s = link.to_string();
        assert_eq!(s.parse::<InviteLink>().unwrap(), link);
        // Only a key: a phone on mobile data with no dialable address.
        let bare = InviteLink {
            addrs: vec![],
            ..link.clone()
        };
        assert!(!bare.to_string().contains('@'));
        assert_eq!(bare.to_string().parse::<InviteLink>().unwrap(), bare);
    }

    #[test]
    fn old_links_still_parse() {
        let fp = Identity::generate().public().fingerprint();
        let s = format!("threnody://{}@192.0.2.7:7450", fp.compact());
        let l: InviteLink = s.parse().unwrap();
        assert_eq!(l.addrs, ["192.0.2.7:7450"]);
        assert_eq!(l.key, None);
        assert_eq!(l.to_string(), s);
    }

    #[test]
    fn bad_links_are_refused() {
        let fp = Identity::generate().public().fingerprint().compact();
        for s in [
            format!("threnody://{fp}"),
            format!("threnody://{fp}@2001:db8::1:7450"),
            format!("threnody://{fp}@[nonsense]:7450"),
            format!("threnody://{fp}@host"),
            format!("threnody://{fp}?r=SHORT"),
            "threnody://NOTAFINGERPRINT@192.0.2.1:7450".to_owned(),
            format!("https://{fp}@192.0.2.1:7450"),
        ] {
            assert!(s.parse::<InviteLink>().is_err(), "{s}");
        }
    }
}
