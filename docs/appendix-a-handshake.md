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

Labels have the form `threnody v1 2026-10-03 <purpose>`. The full list is in `crypto/kdf.rs`.

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
```

Each handshake AEAD key encrypts exactly one message, so the nonce is fixed at zero.

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
- **Identity hiding.** Both identities are encrypted. The initiator's identity stays hidden from active attackers. The responder's identity is revealed to whoever initiates, which is inherent to SIGMA-I.
- **Forward secrecy.** The ephemeral hybrid key is discarded once the handshake finishes.
- **Policy is out of band.** The handshake only reports the peer's identity. Deciding whether to trust it (TOFU, a pinned fingerprint from an invite, or approved-only) is up to the caller (`threnody-net::node`).

## Not yet done

- Tamarin models of identity hiding (needs observational equivalence) and of suite negotiation. Secrecy, forward secrecy, KCI resistance, hybrid security and mutual authentication are already proved in [`proofs/handshake.spthy`](../proofs/README.md).
