# Formal model

`handshake.spthy` is a Tamarin model of the Threnody v1 handshake ([Appendix A](../docs/appendix-a-handshake.md)). Every lemma verifies automatically in about 10 seconds:

```sh
tamarin-prover --prove proofs/handshake.spthy
```

This needs Tamarin 1.12 and Maude 3.x. On Arch Linux, use `pacman -S tamarin-prover`. Elsewhere, use the release binary from <https://github.com/tamarin-prover/tamarin-prover/releases>.

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

## Abstractions and limits

- Primitives are ideal. KEM breaks are modelled as an oracle that leaks a component's decapsulation key.
- Suite negotiation, identity hiding and the ratchet are not modelled yet.
- This model is not a computational proof.
