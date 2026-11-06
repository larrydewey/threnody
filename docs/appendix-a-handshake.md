# Appendix A: Hybrid Handshake (v1)

Status: implemented in `threnody-core::handshake`. Fills in spec §3.2's "exact pattern to be specified in the detailed crypto appendix".

## Primitives

| Role | Choice |
|---|---|
| KEM | X25519 + ML-KEM-768 with the X-Wing combiner (draft-connolly-cfrg-xwing-kem) |
| Signature | Ed25519, strict verification (`verify_strict`), weak keys rejected |
| AEAD | ChaCha20-Poly1305 (preferred) or AES-256-GCM, negotiated |
| Transcript hash / KDF | BLAKE3 in `derive_key` mode, length-prefixed inputs |

### Hybrid KEM

```
pk = pk_M (1184) || pk_X (32)                     1216 bytes
ct = ct_M (1088) || ct_X (32)                     1120 bytes
ss = SHA3-256(ss_M || ss_X || ct_X || pk_X || "\.//^\")
```

This is X-Wing exactly as specified in draft-connolly-cfrg-xwing-kem-11: a private key is a 32-byte seed expanded with SHAKE256, and encapsulation uses a 64-byte `eseed`. The implementation passes all three test vectors from the draft's Appendix C (`crypto/xwing-draft-11-vectors.txt`). In addition, parsing a public key runs the FIPS 203 §7.2 modulus check, and an all-zero X25519 output is rejected.

### KDF

```
KDF(label, p0, p1, ...) = BLAKE3-derive_key(label, le64(|p0|) || p0 || le64(|p1|) || p1 || ...)
```

Labels have the form `threnody v1 <date> <purpose>`, where the date is when the label was introduced (`2026-10-03` for every label in this appendix). Most are listed in `crypto/kdf.rs`; a few live beside the protocol that uses them (`rendezvous.rs`, `handle.rs`, `invite.rs`).

## Messages

```
I -> R  HS1 = { 0: [suite, ...], 1: e_I }
R -> I  HS2 = { 0: suite, 1: ct, 2: sealed_R }
I -> R  HS3 = { 0: sealed_I }
```

Each one travels in the envelope from Appendix C.

### Transcript

`T` is an incremental BLAKE3 hash keyed with the transcript label. `absorb(name, data)` appends `le64(|name|) || name || le64(|data|) || data`.

```
absorb("protocol", "threnody/1")
absorb("offer",    offered suite bytes)          -- HS1
absorb("e_i",      e_I)
absorb("suite",    [suite])                       -- HS2
absorb("kem_ct",   ct)
h2 = T
absorb("sealed_r", sealed_R)
h3 = T
absorb("sealed_i", sealed_I)                      -- HS3
session_id = T
```

### Keys

```
(ss, ct)       = Encap(e_I)                       -- responder
k_R || k_I     = KDF("handshake traffic keys", ss, h2)   (64 bytes)
root           = KDF("session root", ss, session_id)
exporter       = KDF("exporter secret", ss, session_id)
export(ctx)    = KDF("exported key", exporter, ctx)
```

Each handshake AEAD key encrypts exactly one message, so the nonce is fixed at zero.

`export(ctx)` gives both ends the same session-bound secret for a context, in the spirit of the TLS exporter (RFC 8446 §7.5). It keys LAN discovery (Appendix E), rendezvous recovery slots (Appendix N) and WireGuard preshared keys (Appendix D).

### Payloads

```
sealed_R = AEAD(k_R, nonce 0, ad = h2, { 0: id_R, 1: sig_R, 2: rk_R })
sig_R    = Sign(id_R, L("responder signature") || L(h2) || L(id_R) || L(rk_R))

sealed_I = AEAD(k_I, nonce 0, ad = h3, { 0: id_I, 1: sig_I })
sig_I    = Sign(id_I, L("initiator signature") || L(h3) || L(id_I) || L(""))
```

`L(x) = le64(|x|) || x`. `rk_R` is the responder's first hybrid ratchet public key (Appendix B).

## Properties and rationale

- **Post-quantum confidentiality.** Every session key depends on the hybrid shared secret, so an attacker has to break both X25519 and ML-KEM-768 to read traffic. Authentication is classical (Ed25519), as spec §3.2 allows. A later version can add ML-DSA through the major version.
- **Mutual authentication (SIGMA-I).** Each side signs a transcript hash that covers both ephemeral contributions, the negotiated suite and the full offer. A signature replayed from another session fails because `h2` and `h3` are fresh, and a suite downgrade fails because the offer is part of the signed transcript.
- **Key confirmation.** Each side's identity payload can only be decrypted with keys derived from `ss`.
- **Identity hiding.** Both identities are encrypted. The initiator's identity stays hidden from active attackers. The responder's identity is revealed to whoever initiates, which is inherent to SIGMA-I. This holds only if the initiator checks the responder before sending HS3: when it dialed for a particular fingerprint (an invite, a stored contact, a relayed or onion route, a hole punch) and a different identity answers, it closes the connection without sending HS3, so an impostor at the dialed address never learns who was calling.
- **Forward secrecy.** The ephemeral hybrid key is discarded once the handshake finishes.
- **Policy is out of band.** The handshake only reports the peer's identity. Deciding whether to trust it (TOFU, a pinned fingerprint from an invite, or approved-only) is up to the caller (`threnody-net::node`).

## Limits

Anyone who can reach a listener can start a handshake, and each one holds a socket until it finishes or times out (15 s). A node runs at most 256 inbound TCP handshakes at once, 16 from one source; an IPv6 source counts by its /64. Radio links (Bluetooth, Wi-Fi Direct) have no address to tell callers apart, so they share one allowance of 16, kept apart from TCP's so that load from the internet can't shut out the local fallback. Over the limit a connection is closed at once. If accepting fails (usually because the process is out of file descriptors), the listener pauses for 100 ms instead of trying again at once. QUIC has its own limits (Appendix N, *Security*). These limits end with the handshake: a session it starts is not counted here.

Sessions with strangers are limited on their own, since under the `Anyone` accept policy a peer could otherwise make a fresh identity for each connection and hold one session, and one socket, per identity for good. A stranger is a peer we haven't accepted, that isn't one of our devices and isn't mutually approved. A node holds at most 64 stranger sessions at once, at most 4 from one network (an IPv4 address or an IPv6 /64, as above; radio links count as one network). The check comes after the handshake, over TCP, QUIC and radio links alike: past the limit the session is refused and closed straight away. A stranger's session also ends after 5 minutes without application traffic in either direction (cover traffic and heartbeats don't count). Accepting the stranger, or approving each other, lifts both at once. Nothing changes on the wire, so 0.17 peers see a refusal as a session that closed. A sender whose messages are left unacknowledged that way, by a peer that isn't mutually approved, dials again after 30 s, then twice as long each time up to 30 minutes, 20 times at most (about 8 hours). So a message request gets through once a place is free. Sessions with accepted contacts, own devices and mutually approved peers are never counted or expired. Relay and onion circuits (Appendices G and I) aren't counted either: they ride on a neighbour's session, hold no socket of their own and are capped per neighbour. A stranger at the end of one still idles out. Mailbox deposits come from mutual neighbours or over onion circuits, and anonymous links (Appendix P) have their own limits, so neither uses these places.

## Not yet done

- Tamarin models of identity hiding (needs observational equivalence) and of suite negotiation. Secrecy, forward secrecy, KCI resistance, hybrid security and mutual authentication are already proved in [`proofs/handshake.spthy`](../proofs/README.md).
