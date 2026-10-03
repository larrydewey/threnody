# Appendix B: Hybrid Double Ratchet with Header Encryption (v1)

Status: implemented in `threnody-core::ratchet`. Fulfils spec §3.3 and §6.1 ("double ratchet with hybrid PQ updates").

## Construction

This is the header-encryption variant of the Signal double ratchet (DR-HE), with the Diffie-Hellman ratchet replaced by a **KEM ratchet** over the X-Wing KEM from Appendix A.

```
RootStep(rk, ss)  : rk' || ck || nhk = KDF("ratchet root step", rk, ss)     (96 bytes)
ChainStep(ck)     : mk = BLAKE3-keyed(ck, 0x01); ck' = BLAKE3-keyed(ck, 0x02)
MessageKey(mk)    : key || nonce = KDF("message key expansion", mk)          (32 + 12 bytes)
HK_I || HK_R      = KDF("initial header keys", root)                         (64 bytes)
```

Each party holds a header key for its current sending chain (`HKs`) and one for its current receiving chain (`HKr`). It also holds the next header key for each direction (`NHKs`, `NHKr`). Each `RootStep` produces the next header key for the direction it opens.

### Initialisation

- **Responder.** Holds `sk_R0` (its public half was sent as `rk_R` in HS2) and `rk = root`. It sets `NHKs = HK_R` and `NHKr = HK_I`, and has no sending chain yet.
- **Initiator.** Sets `NHKs = HK_I` and `NHKr = HK_R`. It generates `sk_A1`, runs `(ct, ss) = Encap(rk_R)` and `rk, CKs, nhk = RootStep(root, ss)`, then sets `HKs = NHKs` and `NHKs = nhk`. Its header is `(pk_A1, ct)`.

The initiator sends a `Hello` message straight away so the responder can open its own sending chain.

### Receiving a message from a new chain

The receiver first tries the header keys it stored for skipped messages, then `HKr`. If neither opens the header and `NHKr` does, the message starts a new chain:

1. Store the skipped keys of the current receiving chain up to `header.pn`.
2. `ss = Decap(sk_own, header.ct)`, then `rk, CKr, nhk = RootStep(rk, ss)`. Set `HKr = NHKr` and `NHKr = nhk`.
3. Generate a fresh `sk_own'`, run `(ct', ss') = Encap(header.pk)`, then `rk, CKs, nhk' = RootStep(rk, ss')`. Set `HKs = NHKs` and `NHKs = nhk'`. Discard the old `sk_own`.
4. Every message in the new sending chain carries `(pk_own', ct')` in its encrypted header.

Steps strictly alternate: a party opens a new sending chain only after receiving the peer's newest key. The peer's next step is therefore always encapsulated to our newest key, so only one decapsulation key has to be kept at any time.

### Messages

```
Header     = { 0: pk (1216), 1: ct (1120), 2: pn uint, 3: n uint }
enc_header = AEAD(HKs, nonce = hn, ad = session_id, Header)
RatchetMsg = { 0: hn (12, random), 1: enc_header, 2: ciphertext }
ad         = session_id (32) || hn || enc_header
```

The plaintext is a padded `AppMessage` (Appendix C).

### Limits and failure handling

- `MAX_SKIP` is 1000 per chain. At most 2000 skipped keys are kept in total, and the oldest are evicted first.
- Decryption runs on a copy of the state and commits only if authentication succeeds. A forged or corrupted frame therefore cannot desynchronise the session.
- Each message key is used once. A replayed message fails with `Replay`.
- A header key is reused for every message in its chain, so header nonces are random. That is safe for far more than `MAX_SKIP` messages per chain.

## Properties

- **Forward secrecy.** Message keys are deleted after use, chain keys advance one way, and each decapsulation key is deleted after its step.
- **Post-compromise security (hybrid).** After a compromise, the next round trip mixes in a secret encapsulated to a key the attacker never saw. Recovery holds as long as either X25519 or ML-KEM-768 stays secure.
- **Cost.** Every header carries a full public key and ciphertext (about 2.3 KB). That is fine on IP links. On Bluetooth LE a future minor version should send the KEM material only in the first messages of a chain, or move to a sparse post-quantum ratchet.
- **Opaque headers.** Path observers and relays see only random-looking bytes of a fixed size per chain. Ratchet public keys, KEM ciphertexts and message counters stay hidden, so frames cannot be linked to chains or ordered by counter.

## Not yet done

- Persisting ratchet state across reconnects. Today every connection runs a new handshake, which is safe but costs an extra round trip.
