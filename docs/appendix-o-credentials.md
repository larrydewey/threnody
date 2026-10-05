# Appendix O: Zero-Knowledge Credentials (v1)

Status: specified here, modelled in [`proofs/credentials.spthy`](../proofs/README.md), implemented in `threnody-core::credential` and `threnody-net::cred`. Fulfils spec §4.3 ("attribute-based or zero-knowledge style credentials"). Relay tokens (Appendix P) are credentials too.

An issuer vouches for attributes of someone, such as "member of the hackspace" or "over 18". The holder can later prove any subset of them to someone else without showing the rest. Proofs are unlinkable: two presentations of one credential, or a presentation and its issuance, can't be tied together, except as described under [Pseudonyms](#pseudonyms).

## Primitives

- **BBS signatures** on BLS12-381 with SHA-256, following `draft-irtf-cfrg-bbs-signatures`. Blind issuance follows `draft-irtf-cfrg-bbs-blind-signatures`, and pseudonyms follow `draft-irtf-cfrg-bbs-per-verifier-linkability`. The implementation is the `zkryptium` crate (0.7, features `bbsplus_nym`).
- **ML-DSA-65** (FIPS 204), deterministic, with context strings. It is used beside Ed25519 wherever an issuer signs something that must stay unforgeable against a quantum adversary.

## Issuer keys

Any identity can issue credentials. Its keys are derived from its identity seed and a generation number, so nothing new is stored:

```
ikm_bbs   = KDF("… issuer bbs key",   seed, le32(generation))      → BBS KeyGen
seed_mldsa = KDF("… issuer ml-dsa key", seed, le32(generation))    → ML-DSA-65 KeyGen
m         = KDF("… issuer key", identity, le32(generation), bbs_pk, mldsa_pk)   (64 bytes)
IssuerKey = { 0: 1, 1: identity (32), 2: generation, 3: bbs_pk (96), 4: mldsa_pk (1952),
              5: Ed25519(identity, m), 6: ML-DSA(mldsa, m, ctx "threnody issuer key") }
issuer_id = KDF("… issuer id", identity, le32(generation), bbs_pk, mldsa_pk)       (32 bytes)
```

Decoding an issuer key verifies both signatures. Pinning `issuer_id` (as a directory link does, Appendix P) pins the ML-DSA key with it.

## Credentials

```
header    = { 0: 1, 1: issuer_id (32), 2: schema tstr (≤ 64), 3: expires_day }   canonical CBOR
attribute = [ key tstr, value tstr ]            one BBS message each; ≤ 16, limits as profiles (Appendix M)
committed = [ link_secret (32) ]                hidden from the issuer, with one pseudonym secret
```

`expires_day` counts days since 1970-01-01 (UTC). A credential is valid through the end of that day.

## Issuance

```
holder → issuer   commitment = Commit(link_secret, nym_secret) with proof of knowledge (176 bytes)
issuer → holder   Issued = { 0: header, 1: [* attribute], 2: signature (80), 3: nym_entropy (32),
                             4: receipt }
receipt = ML-DSA(issuer, KDF("… credential receipt", header, commitment, signature, nym_entropy,
                              attribute…), ctx "threnody credential receipt")
```

The issuer verifies the commitment's proof and signs with `BlindSign` with pseudonym (`nym_entropy` is the signer's contribution, so the holder can't choose its pseudonym secret). The holder then checks the ML-DSA receipt against the issuer's pinned key. It also checks the BBS signature, which yields the final pseudonym secret. The issuer never learns the link secret or the pseudonym secret.

The receipt makes issuance post-quantum authenticated: a holder never accepts a credential an impostor made up, even against an adversary who can forge BBS signatures. The receipt is never shown in a presentation, since it would link it to the issuance.

## Presentations

```
Presentation = { 0: issuer_id, 1: header, 2: total (attribute count), 3: [* [index, key, value]],
                 4: proof, 5: pseudonym (48) }
proof = ProofGenWithNym(pk, signature, header, ph = binding, context, disclosed indexes)
```

The verifier checks the following:

- The issuer key is the one it trusts for this purpose.
- The header names that issuer and hasn't expired.
- The disclosed indexes are strictly increasing and below `total`.
- The proof is exactly `240 + 32·(total − disclosed + 3) + 32` bytes. This check runs before any BBS code sees the bytes.
- The proof verifies for its own `context` and `binding`.

Everything else (the hidden attributes, the link secret, the signature) stays hidden.

**Binding.** Between peers, `binding = KDF("… presentation binding", session_id, nonce)`: the verifier's random nonce and the session the request came in. A verifier can't replay the proof to someone else as fresh. For relay tokens, the binding is the circuit hop's ephemeral key (Appendix P).

## Pseudonyms

Every presentation carries `pseudonym = Nym(nym_secret, context)`. It is the same each time one credential is presented in one context, and unrelated across contexts.

- **Between peers**, `context = "threnody peer nym v1" || verifier identity`. A verifier recognises a returning credential, and no two verifiers can link what they saw.
- **For relay tokens**, the context is a relay, a day and a slot (Appendix P). This is what lets a relay rate-limit a credential without identifying it.

## Peer protocol

Over a session, as `AppMessage::Credential` (kind 21), only to peers whose `Hello` has feature bit 128:

```
CredMsg = { 0: op, 1: id u64, ? 2: issuer key, ? 3: schema, ? 4: [* attribute], ? 5: expires_day,
            ? 6: commitment, ? 7: Issued, ? 8: nonce (32), ? 9: [* key tstr], ? 10: Presentation }
op: 1 offer, 2 request, 3 issued, 4 decline, 5 ask, 6 proof
```

- **Issuing.** The issuer offers (1), naming itself. An offer whose key isn't the sender's own is ignored. The holder's user accepts, and the holder sends a request with its commitment (2). The issuer issues (3) only for an offer it made to that peer.
- **Proving.** A verifier asks (5) for keys of a schema, with a nonce. The holder's user chooses a credential, and the holder shows the asked-for keys among those the user allows (6). Keys nobody asked for are never shown. The verifier refuses a proof showing more, or of another schema.
- Either side may decline (4). Every step that gives something away waits for the user. Apps show who issued a credential and when it expires.

## What this protects, and what it doesn't

- **Post-quantum.** Issuer keys, issuance receipts and the sessions credentials travel in are post-quantum (X-Wing and ML-DSA). BBS proofs are not. An adversary able to break discrete logarithms in BLS12-381 could forge a presentation. Unlinkability is perfect, so recorded presentations stay unlinkable even then. There is no standardised post-quantum anonymous credential to use instead.
- **Sharing.** A holder can't hand a presentation to someone else (it is bound to the session). It could still share its whole credential and secrets, as with any bearer credential.
- **Issuers know what they issued**, and to whom. They can't recognise its presentations.
- **Not audited.** `zkryptium` follows IETF drafts and has not been audited. Threnody checks every size before passing network bytes to it, and contains any panic it raises as a failed verification. The mutation tests (`robustness.rs`) and the `credential` fuzz target exercise these paths.
