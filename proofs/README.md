# Formal model

This directory has two Tamarin models:

- `handshake.spthy` models the v1 handshake ([Appendix A](../docs/appendix-a-handshake.md)).
- `ratchet.spthy` models the KEM double ratchet ([Appendix B](../docs/appendix-b-ratchet.md)).
- `sealed.spthy` models sealed messages and prekeys for offline delivery ([Appendix H](../docs/appendix-h-offline.md)).
- `onion.spthy` models the onion hop handshake ([Appendix I](../docs/appendix-i-onion.md)).
- `devices.spthy` models device linking and account chains ([Appendix J](../docs/appendix-j-devices.md)).

Every lemma verifies automatically, in a few minutes in total:

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

## Sealed messages

| Lemma | Property |
|---|---|
| `executable_opk`, `executable_spk_only` | Both message forms can be delivered. |
| `secrecy_with_opk` | The body stays secret unless B's identity key leaked before sending (forged prekeys), both the SPK and OPK secrets were revealed while they existed, or the KEM is broken. |
| `secrecy_spk_only` | The same with the SPK alone. |
| `sender_authentication` | B accepts a message from A only if A sent exactly that body to B, unless A's key leaked earlier. Holds even when all of B's keys are compromised (KCI). |
| `no_replay` | Every message is accepted at most once. |
| `sanity_*` | Revealing both prekeys, or A's key, really does enable an attack. |

Key lifecycles are restrictions over persistent facts:

- An SPK cannot be used after it is retired.
- An OPK is accepted at most once.
- A key can only be revealed before it is deleted.

That last restriction is what makes the secrecy lemmas forward-secrecy statements.

## Onion hop handshake

| Lemma | Property |
|---|---|
| `executable` | An anonymous client and a relay agree on layer keys. |
| `client_key_secrecy` | The client's layer keys stay secret unless the relay's identity key leaked before the handshake, or the KEM is broken. This gives forward secrecy: later leaks don't help. |
| `relay_authentication` | The hop the client finishes with is the relay it addressed, on this very exchange. |
| `key_agreement` | Client and relay derive the same keys. |
| `sanity_*` | An early identity-key leak really does allow impersonation. |

Client anonymity holds by construction: the client sends only a fresh ephemeral key and signs nothing. A formal proof would need observational equivalence, which isn't modelled.

## Devices

| Lemma | Property |
|---|---|
| `only_code_holder_is_linked` | A device is linked only if it received the out-of-band link code. Proofs are bound to the session and the device, so a code captured off the network is useless. |
| `honest_signer_authorized_and_linked` | When an honest member signs an add, it authorised exactly that add, after linking that very device. |
| `honest_devices_consent` | An honest device is never put into an account without its own signature. |
| `*_executable`, `sanity_*` | Both protocols can complete, and a leaked code or a compromised member really does let an intruder in. |

These lemmas are stated per add, so the prover never has to unroll the chain. Applied link by link from the genesis link, they give the global property: with no compromised device and no leaked code, every member of an account is an honest device that an existing member linked.

## Abstractions and limits

- Primitives are ideal. KEM breaks are modelled as an oracle that leaks a component's decapsulation key.
- Suite negotiation, identity hiding, header-encryption metadata properties and ratchet message authenticity are not modelled yet.
- These models are not computational proofs.
