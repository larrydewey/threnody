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

use std::net::Ipv6Addr;

use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::crypto::kdf::{derive, label};
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
