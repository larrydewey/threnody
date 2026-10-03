# Formal model

This directory has two Tamarin models:

- `handshake.spthy` models the v1 handshake ([Appendix A](../docs/appendix-a-handshake.md)).
- `ratchet.spthy` models the KEM double ratchet ([Appendix B](../docs/appendix-b-ratchet.md)).

Every lemma verifies automatically in under 20 seconds:

```sh
proofs/check.sh   # fails unless every lemma in every model verifies
```

This needs Tamarin 1.12 and Maude 3.x. On Arch Linux, use `pacman -S tamarin-prover`. Elsewhere, use the release binary from <https://github.com/tamarin-prover/tamarin-prover/releases>. Arch's Maude package does not export `MAUDE_LIB`, so `check.sh` sets it to `/usr/share/maude` when it is unset.

## Handshake

| Lemma | Property |
|---|---|
| `executable` | Honest initiator and responder can complete a session. |
| `secrecy_initiator` | The session key stays secret unless the responder's long-term key leaked before the session, or *both* ML-KEM and X25519 are broken. |
| `secrecy_responder` | The same guarantee from the responder's side. |
| `responder_authentication` | The initiator agrees with the responder on the transcript unless the responder's key leaked earlier. |
| `initiator_authentication_injective` | Injective agreement on the session id unless the initiator's key leaked earlier. |
| `sanity_*` | Attacks exist when the exceptions apply, so the security lemmas are not vacuous. |

Taken together, the secrecy lemmas show the following:

- **Forward secrecy.** A long-term key that leaks *after* the session does not expose the session key.
- **KCI resistance.** A party's own leaked key does not let an attacker impersonate peers to it.
- **Hybrid security.** Breaking either KEM alone is not enough.

## Ratchet

| Lemma | Property |
|---|---|
| `executable` | Three alternating ratchet steps complete. |
| `step_secret_secrecy` | The secret an honest step mixes into the root stays secret unless the recipient's ratchet key was revealed while held, or both KEMs are broken. Every root key may be known. |
| `message_secrecy` | The same guarantee for every message. |
| `sanity_*` | Key reveals and double breaks really do leak messages, and messages to fresh keys after a root leak are possible. |

Together these give:

- **Post-compromise security.** A full state compromise heals as soon as a message is sent to a ratchet key generated after it.
- **Forward secrecy.** Keys can only be revealed while held, because of the `reveal_only_while_held` restriction. Once a key is deleted, leaking anything else does not expose messages sent to it.
- **Hybrid security.** Breaking either KEM alone is not enough.

Abstractions specific to this model:

- The ratchet starts from the handshake's root key and the responder's first ratchet key.
- Each chain carries one message, because the symmetric chain is a one-way hash chain.
- Header encryption is not modelled.
- Reveal rules read persistent copies of each state, so Tamarin does not have to unroll the ratchet's history.

## Abstractions and limits

- Primitives are ideal. KEM breaks are modelled as an oracle that leaks a component's decapsulation key.
- Suite negotiation, identity hiding, header-encryption metadata properties and ratchet message authenticity are not modelled yet.
- These models are not computational proofs.
