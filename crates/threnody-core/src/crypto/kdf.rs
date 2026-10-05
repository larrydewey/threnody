//! Domain-separated key derivation (spec §3.2: "All symmetric keys derived
//! from the handshake MUST use a domain-separated KDF").
//!
//! Every derivation is `BLAKE3-derive_key(label, len(p0) || p0 || len(p1) || p1 ...)`
//! with 64-bit little-endian length prefixes, so no two input tuples collide.

/// Context strings. Each is globally unique and versioned.
pub mod label {
    pub const TRANSCRIPT: &str = "threnody v1 2026-10-03 handshake transcript";
    pub const HANDSHAKE_KEYS: &str = "threnody v1 2026-10-03 handshake traffic keys";
    pub const ROOT: &str = "threnody v1 2026-10-03 session root";
    pub const RATCHET_ROOT: &str = "threnody v1 2026-10-03 ratchet root step";
    pub const MESSAGE_KEY: &str = "threnody v1 2026-10-03 message key expansion";
    pub const HEADER_KEYS: &str = "threnody v1 2026-10-03 initial header keys";
    pub const FINGERPRINT: &str = "threnody v1 2026-10-03 identity fingerprint";
    pub const SAFETY_NUMBER: &str = "threnody v1 2026-10-03 safety number";
    pub const SIG_RESPONDER: &str = "threnody v1 2026-10-03 responder signature";
    pub const SIG_INITIATOR: &str = "threnody v1 2026-10-03 initiator signature";
    pub const EXPORTER: &str = "threnody v1 2026-10-03 exporter secret";
    pub const EXPORT: &str = "threnody v1 2026-10-03 exported key";
    pub const WG_STATIC: &str = "threnody v1 2026-10-03 wireguard static key";
    pub const OVERLAY_PREFIX: &str = "threnody v1 2026-10-03 overlay ula prefix";
    pub const OVERLAY_ADDR: &str = "threnody v1 2026-10-03 overlay address";
    pub const STATE_KEY: &str = "threnody v1 2026-10-03 state encryption key";
    pub const SIG_PREKEY: &str = "threnody v1 2026-10-03 prekey";
    pub const SIG_ONE_TIME_PREKEY: &str = "threnody v1 2026-10-03 one-time prekey";
    pub const SIG_SEALED_SENDER: &str = "threnody v1 2026-10-03 sealed sender";
    pub const SEALED_TRANSCRIPT: &str = "threnody v1 2026-10-03 sealed transcript";
    pub const SEALED_KEY: &str = "threnody v1 2026-10-03 sealed key";
    pub const ONION_LAYER_KEYS: &str = "threnody v1 2026-10-03 onion layer keys";
    pub const SIG_ONION_CREATED: &str = "threnody v1 2026-10-03 onion created";
    pub const ACCOUNT_ID: &str = "threnody v1 2026-10-03 account id";
    pub const ACCOUNT_FINGERPRINT: &str = "threnody v1 2026-10-03 account fingerprint";
    pub const SIG_ACCOUNT_LINK: &str = "threnody v1 2026-10-03 account link";
    pub const LINK_PROOF: &str = "threnody v1 2026-10-03 link proof";
    pub const ISSUER_BBS_KEY: &str = "threnody v1 2026-10-05 issuer bbs key";
    pub const ISSUER_MLDSA_KEY: &str = "threnody v1 2026-10-05 issuer ml-dsa key";
    pub const SIG_ISSUER_KEY: &str = "threnody v1 2026-10-05 issuer key";
    pub const ISSUER_ID: &str = "threnody v1 2026-10-05 issuer id";
    pub const CREDENTIAL_RECEIPT: &str = "threnody v1 2026-10-05 credential receipt";
    pub const PRESENTATION: &str = "threnody v1 2026-10-05 presentation binding";
    pub const RELAY_TOKEN: &str = "threnody v1 2026-10-05 relay token binding";
    pub const SIG_RELAY_DESCRIPTOR: &str = "threnody v1 2026-10-05 relay descriptor";
    pub const SIG_DIRECTORY: &str = "threnody v1 2026-10-05 directory document";
}

/// Fills `out` from the KDF.
pub fn derive_into(label: &str, parts: &[&[u8]], out: &mut [u8]) {
    let mut h = blake3::Hasher::new_derive_key(label);
    for p in parts {
        h.update(&(p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize_xof().fill(out);
}

/// Derives a fixed-size output.
pub fn derive<const N: usize>(label: &str, parts: &[&[u8]]) -> [u8; N] {
    let mut out = [0u8; N];
    derive_into(label, parts, &mut out);
    out
}

/// Symmetric-chain step: `keyed_hash(ck, [tag])`.
pub fn chain(ck: &[u8; 32], tag: u8) -> [u8; 32] {
    *blake3::keyed_hash(ck, &[tag]).as_bytes()
}
