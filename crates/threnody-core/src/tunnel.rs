//! WireGuard-compatible full-mesh tunnel material (spec §8).
//!
//! - Each device's WireGuard static key is derived from its identity seed,
//!   so it is stable and needs no extra storage.
//! - Each mutually approved pair's WireGuard preshared key is exported from
//!   their current Threnody session. WireGuard's PSK slot is mixed into every
//!   handshake, so tunnels inherit the session's X-Wing post-quantum
//!   protection (the same idea as Rosenpass), and the PSK rotates with
//!   every Threnody session.
//! - Overlay addresses are IPv6 ULAs derived from identity keys, so no
//!   address coordination is needed.
//!
//! Details: `docs/appendix-d-tunnels.md`.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use const_cbor::Decoder;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::cbor::{self, finish, read_map, required};
use crate::crypto::kdf::{derive, label};
use crate::error::{Error, Result};
use crate::identity::{Identity, PublicIdentity};

/// Exporter context for the WireGuard preshared key.
pub const PSK_CONTEXT: &[u8] = b"wireguard preshared key";

/// Default WireGuard UDP port for Threnody tunnels.
pub const DEFAULT_PORT: u16 = 51820;

/// A device's WireGuard static key pair.
pub struct WgKeys {
    secret: Zeroizing<[u8; 32]>,
    public: [u8; 32],
}

impl WgKeys {
    pub fn derive(identity: &Identity) -> Self {
        let seed = identity.seed();
        let mut secret: Zeroizing<[u8; 32]> =
            Zeroizing::new(derive(label::WG_STATIC, &[&seed[..]]));
        // RFC 7748 clamping, so the stored key is exactly what `wg` would use.
        secret[0] &= 248;
        secret[31] &= 127;
        secret[31] |= 64;
        let public = *PublicKey::from(&StaticSecret::from(*secret)).as_bytes();
        Self { secret, public }
    }

    pub fn public(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    pub fn secret_base64(&self) -> Zeroizing<String> {
        Zeroizing::new(base64(&self.secret[..]))
    }

    pub fn public_base64(&self) -> String {
        base64(&self.public)
    }
}

/// The `/48` ULA prefix all Threnody overlay addresses live in.
pub fn overlay_prefix() -> [u8; 6] {
    let h: [u8; 5] = derive(label::OVERLAY_PREFIX, &[]);
    [0xfd, h[0], h[1], h[2], h[3], h[4]]
}

/// A device's overlay address: the shared `/48` prefix plus 80 bits
/// derived from its identity key.
pub fn overlay_addr(id: &PublicIdentity) -> Ipv6Addr {
    let mut a = [0u8; 16];
    a[..6].copy_from_slice(&overlay_prefix());
    let h: [u8; 10] = derive(label::OVERLAY_ADDR, &[id.as_bytes()]);
    a[6..].copy_from_slice(&h);
    Ipv6Addr::from(a)
}

/// Standard padded base64 (RFC 4648 §4), as WireGuard uses for keys.
pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(A[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// State file name for the saved tunnel peers (`Home::save_state`).
pub const STATE: &str = "tunnels";
/// A saved peer older than this (since its PSK was made) is neither
/// restored nor written back: by then a new session should have replaced it.
pub const KEEP_FOR_MS: u64 = 7 * 24 * 3600 * 1000;
/// Most peers a state file may hold; more are ignored on load.
pub const MAX_SAVED: usize = 4096;
const SAVED_VERSION: u64 = 1;
/// Upper bound on one encoded entry, so `cbor::to_vec` never has to retry
/// (a retry would drop a scratch buffer holding PSKs without zeroizing it).
const SAVED_ENTRY_MAX: usize = 160;
/// Upper bound on everything around the entries.
const SAVED_HEAD_MAX: usize = 64;

/// One tunnel peer as kept at rest (Appendix D, *Persistence*). The
/// overlay address is not stored: it is `overlay_addr(&peer)`.
#[derive(Clone)]
pub struct SavedPeer {
    pub peer: PublicIdentity,
    pub wg_public: [u8; 32],
    /// Last endpoint known to work (or the offered one).
    pub endpoint: SocketAddr,
    pub psk: Zeroizing<[u8; 32]>,
    /// When the PSK was exported from a session (ms since the epoch).
    pub made_ms: u64,
}

impl SavedPeer {
    /// Whether this may still be kept at `now_ms`. A timestamp from the
    /// future (the clock moved back) counts as fresh only up to
    /// [`KEEP_FOR_MS`] ahead.
    pub fn fresh(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.made_ms) <= KEEP_FOR_MS
            && self.made_ms <= now_ms.saturating_add(KEEP_FOR_MS)
    }
}

/// Encodes saved tunnel peers for `Home::save_state`:
///
/// ```text
/// { 0 => 1, 1 => [* { 0 => identity (32), 1 => WireGuard key (32),
///                     2 => IPv6 (16; IPv4 mapped), 3 => port,
///                     4 => PSK (32), 5 => made_ms }] }
/// ```
pub fn encode_saved(peers: &[SavedPeer]) -> Result<Zeroizing<Vec<u8>>> {
    let estimate = SAVED_HEAD_MAX + peers.len() * SAVED_ENTRY_MAX;
    let out = cbor::to_vec(estimate, |e| {
        e.map_len(2)?;
        e.u8(0)?.u64(SAVED_VERSION)?;
        e.u8(1)?.array_len(peers.len())?;
        for p in peers {
            let ip = match p.endpoint.ip() {
                IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                IpAddr::V6(v6) => v6,
            };
            e.map_len(6)?;
            e.u8(0)?.bytes(p.peer.as_bytes())?;
            e.u8(1)?.bytes(&p.wg_public)?;
            e.u8(2)?.bytes(&ip.octets())?;
            e.u8(3)?.u16(p.endpoint.port())?;
            e.u8(4)?.bytes(&p.psk[..])?;
            e.u8(5)?.u64(p.made_ms)?;
        }
        Ok(())
    })?;
    Ok(Zeroizing::new(out))
}

/// Decodes [`encode_saved`]'s output. Entries that don't make sense (an
/// invalid identity, a wrong length, a missing field) are skipped; past
/// [`MAX_SAVED`] the rest are read but dropped.
pub fn decode_saved(b: &[u8]) -> Result<Vec<SavedPeer>> {
    let mut dec = Decoder::new(b);
    let mut ver = None;
    let mut out = Vec::new();
    read_map(&mut dec, |k, d| {
        match k {
            0 => ver = Some(d.u64()?),
            1 => {
                for _ in 0..d.array_len()? {
                    if let Some(p) = decode_saved_peer(d)?
                        && out.len() < MAX_SAVED
                    {
                        out.push(p);
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    if required(ver, "tunnels version")? != SAVED_VERSION {
        return Err(Error::Malformed("tunnels version"));
    }
    Ok(out)
}

/// One entry, or `None` when it is consumed but unusable.
fn decode_saved_peer(d: &mut Decoder<'_>) -> Result<Option<SavedPeer>> {
    let (mut peer, mut wg_public, mut ip, mut port, mut psk, mut made) =
        (None, None, None, None, None, None);
    read_map(d, |k, d| {
        match k {
            0 => peer = <[u8; 32]>::try_from(d.bytes()?).ok(),
            1 => wg_public = <[u8; 32]>::try_from(d.bytes()?).ok(),
            2 => ip = <[u8; 16]>::try_from(d.bytes()?).ok(),
            3 => port = Some(d.u16()?),
            4 => psk = <[u8; 32]>::try_from(d.bytes()?).ok().map(Zeroizing::new),
            5 => made = Some(d.u64()?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let (Some(peer), Some(wg_public), Some(ip), Some(port), Some(psk), Some(made_ms)) =
        (peer, wg_public, ip, port, psk, made)
    else {
        return Ok(None);
    };
    let Ok(peer) = PublicIdentity::from_bytes(&peer) else {
        return Ok(None);
    };
    Ok(Some(SavedPeer {
        peer,
        wg_public,
        endpoint: SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)).to_canonical(), port),
        psk,
        made_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn keys_and_addresses_are_stable_and_distinct() {
        let a = Identity::from_seed(&[1; 32]);
        let b = Identity::from_seed(&[2; 32]);
        assert_eq!(WgKeys::derive(&a).public(), WgKeys::derive(&a).public());
        assert_ne!(WgKeys::derive(&a).public(), WgKeys::derive(&b).public());
        let (aa, ab) = (overlay_addr(&a.public()), overlay_addr(&b.public()));
        assert_ne!(aa, ab);
        assert_eq!(aa.octets()[..6], ab.octets()[..6]);
        assert_eq!(aa.octets()[0], 0xfd);
    }

    fn saved(seed: u8, endpoint: &str, made_ms: u64) -> SavedPeer {
        SavedPeer {
            peer: Identity::from_seed(&[seed; 32]).public(),
            wg_public: [seed.wrapping_add(1); 32],
            endpoint: endpoint.parse().unwrap(),
            psk: Zeroizing::new([seed.wrapping_add(2); 32]),
            made_ms,
        }
    }

    fn same(a: &SavedPeer, b: &SavedPeer) -> bool {
        a.peer == b.peer
            && a.wg_public == b.wg_public
            && a.endpoint == b.endpoint
            && *a.psk == *b.psk
            && a.made_ms == b.made_ms
    }

    /// One entry as `encode_saved` writes it, with `ip` as given and an
    /// extra unknown key 9.
    fn raw_entry(
        e: &mut const_cbor::Encoder<'_>,
        id: &[u8; 32],
        ip: &[u8],
    ) -> core::result::Result<(), const_cbor::Error> {
        e.map_len(7)?;
        e.u8(0)?.bytes(id)?;
        e.u8(1)?.bytes(&[7; 32])?;
        e.u8(2)?.bytes(ip)?;
        e.u8(3)?.u16(51820)?;
        e.u8(4)?.bytes(&[8; 32])?;
        e.u8(5)?.u64(1)?;
        e.u8(9)?.str("future")?;
        Ok(())
    }

    #[test]
    fn saved_peers_round_trip() {
        let peers = vec![
            saved(1, "[2001:db8::1]:51820", 1_000),
            saved(2, "192.0.2.7:4242", 2_000),
            saved(3, "0.0.0.0:51820", 3_000),
        ];
        let b = encode_saved(&peers).unwrap();
        let back = decode_saved(&b).unwrap();
        assert_eq!(back.len(), 3);
        for (a, b) in peers.iter().zip(&back) {
            assert!(same(a, b));
        }
        assert!(back[1].endpoint.is_ipv4(), "IPv4 comes back canonical");

        // Unknown keys, in an entry and at the top level, are ignored.
        let id = *Identity::from_seed(&[4; 32]).public().as_bytes();
        let raw = cbor::to_vec(256, |e| {
            e.map_len(3)?;
            e.u8(0)?.u64(SAVED_VERSION)?;
            e.u8(1)?.array_len(1)?;
            raw_entry(e, &id, &Ipv6Addr::LOCALHOST.octets())?;
            e.u8(7)?.u64(99)?;
            Ok(())
        })
        .unwrap();
        let back = decode_saved(&raw).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].endpoint, "[::1]:51820".parse().unwrap());
        assert_eq!(*back[0].psk, [8; 32]);

        // The estimate is an upper bound, so encoding never retries.
        let many: Vec<SavedPeer> = (0..MAX_SAVED)
            .map(|i| SavedPeer {
                endpoint: "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535"
                    .parse()
                    .unwrap(),
                made_ms: u64::MAX - i as u64,
                ..saved(1, "[::1]:1", 0)
            })
            .collect();
        let b = encode_saved(&many).unwrap();
        assert!(b.len() <= SAVED_HEAD_MAX + MAX_SAVED * SAVED_ENTRY_MAX);
        assert_eq!(decode_saved(&b).unwrap().len(), MAX_SAVED);
    }

    #[test]
    fn saved_peers_reject_bad_input() {
        let wrong_version = cbor::to_vec(64, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(SAVED_VERSION + 1)?;
            e.u8(1)?.array_len(0)?;
            Ok(())
        })
        .unwrap();
        assert!(decode_saved(&wrong_version).is_err());
        let mut trailing = encode_saved(&[saved(1, "[::1]:1", 1)]).unwrap().to_vec();
        trailing.push(0);
        assert!(decode_saved(&trailing).is_err());
        assert!(decode_saved(b"").is_err());

        // A 15-byte IP or an invalid identity skips that entry only.
        let good = *Identity::from_seed(&[5; 32]).public().as_bytes();
        let raw = cbor::to_vec(1024, |e| {
            e.map_len(2)?;
            e.u8(0)?.u64(SAVED_VERSION)?;
            e.u8(1)?.array_len(3)?;
            raw_entry(e, &good, &[0; 15])?;
            raw_entry(e, &[0; 32], &[0; 16])?;
            raw_entry(e, &good, &[0; 16])?;
            Ok(())
        })
        .unwrap();
        assert!(PublicIdentity::from_bytes(&[0; 32]).is_err());
        let back = decode_saved(&raw).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].peer.as_bytes(), &good);

        // Past MAX_SAVED entries, the rest are read and dropped.
        let over: Vec<SavedPeer> = (0..MAX_SAVED + 3).map(|_| saved(6, "[::1]:1", 1)).collect();
        let b = encode_saved(&over).unwrap();
        assert_eq!(decode_saved(&b).unwrap().len(), MAX_SAVED);
    }

    #[test]
    fn freshness_window() {
        let now = 10 * KEEP_FOR_MS;
        let at = |made_ms| saved(1, "[::1]:1", made_ms).fresh(now);
        assert!(at(now));
        assert!(at(now - KEEP_FOR_MS));
        assert!(!at(now - KEEP_FOR_MS - 1));
        assert!(
            at(now + 60_000),
            "a little in the future (clock moved back)"
        );
        assert!(at(now + KEEP_FOR_MS));
        assert!(!at(now + KEEP_FOR_MS + 1));
        assert!(saved(1, "[::1]:1", 0).fresh(0));
    }

    /// Cross-checks key derivation against the real `wg` tool when present.
    #[test]
    fn public_key_matches_wg_tool() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let keys = WgKeys::derive(&Identity::from_seed(&[3; 32]));
        let Ok(mut child) = Command::new("wg")
            .arg("pubkey")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            eprintln!("wg not installed; skipping cross-check");
            return;
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(keys.secret_base64().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            keys.public_base64()
        );
    }
}
