# Appendix H: Offline Delivery (v1)

Status: specified here, modelled in [`proofs/sealed.spthy`](../proofs/README.md), implemented in `threnody-core::prekey` and `threnody-net::mailbox`.

The interactive handshake (Appendix A) needs both ends online at the same moment. Offline delivery adds two pieces so a message can reach a peer that is currently away:

- **Sealed messages**, which can be decrypted without any interaction, using prekeys the recipient handed out earlier.
- **Mailboxes**, which are approved neighbours that hold sealed messages until the recipient shows up.

## Prekeys

Each device keeps:

- **A signed prekey (SPK).** An X-Wing key pair with id `spk_id`, rotated every 7 days. The previous one is kept for another 14 days so messages still in flight can be read.
- **One-time prekeys (OPKs).** X-Wing key pairs, each with an id. Each is reserved for one contact, and its secret is deleted on first use.

```
sig_spk = Sign(id_B, L("prekey") || L(id_B) || L(u32 spk_id) || L(spk_pub) || L(u64 expiry_ms))
sig_opk = Sign(id_B, L("one-time prekey") || L(id_B) || L(u32 opk_id) || L(opk_pub))
Bundle  = { 0: id_B, 1: spk_id, 2: spk_pub, 3: expiry_ms, 4: sig_spk, 5: [ { 0: opk_id, 1: opk_pub, 2: sig_opk } * ] }
```

- **Distribution.** There is no directory. Once a session is mutually approved, each side sends the other a fresh bundle (`AppMessage::Prekeys`, kind 8) carrying the current SPK and 5 new OPKs reserved for that peer.
- **Storage on the receiving side.** The receiver verifies every signature against the identity it authenticated in the session, then stores the bundle in its encrypted state. A new bundle replaces the old one.
- **Clean-up.** A device keeps the OPK secrets it reserved for a peer for the last two bundles it sent that peer, and deletes older ones.

## Sealed message

The sender A seals to recipient B as follows. It takes an unused OPK if one is left, and otherwise uses the SPK alone.

```
(ct_s, ss_s) = Encap(spk_pub)
(ct_o, ss_o) = Encap(opk_pub)                         -- if an OPK is used; else ct_o = ss_o = ""
h      = KDF("sealed transcript", id_B, spk_id, ct_s, opk_id?, ct_o)
k      = KDF("sealed key", ss_s, ss_o, h)
sig_A  = Sign(id_A, L("sealed sender") || L(h) || L(id_A) || L(id_B) || L(BLAKE3(body)))
Sealed = { 0: spk_id, 1: ct_s, ? 2: opk_id, ? 3: ct_o, 4: AEAD(k, nonce 0, ad = h, { 0: id_A, 1: sig_A, 2: body }) }
```

`body` is a padded `AppMessage` (Appendix C), and each sealed message uses fresh encapsulations. On receipt, B:

1. **Finds the keys.** Looks up the SPK (current or previous, unexpired) and the OPK, if one was used. If anything is missing, it rejects the message.
2. **Derives and decrypts.** Decapsulates, derives `k` and decrypts.
3. **Checks the sender.** Verifies `sig_A` against the claimed `id_A`, which must be a known contact.
4. **Consumes the OPK.** Deletes the OPK secret, if one was used.
5. **Rejects replays.** Rejects the message if `h` has been seen before. Seen transcript hashes are kept for as long as their SPK can still be used.

### Properties (proved in `proofs/sealed.spthy`)

- **Confidentiality.** The body stays secret unless the recipient's SPK secret is revealed and, when an OPK was used, its OPK secret too. Breaking both KEMs also breaks it.
- **Forward secrecy.** For OPK messages it starts when the OPK is deleted. For SPK-only messages it starts when the SPK is retired.
- **Sender authentication.** B accepts a message as coming from A only if A sealed exactly that body to B, unless A's identity key was revealed. A leaked recipient key does not let anyone forge messages *to* B (resistance to key-compromise impersonation).
- **Bound to its recipient.** The signature covers `id_B` and the ciphertexts, so a sealed message can't be replayed to someone else.
- **No replay with an OPK.** A message sealed to an OPK is accepted at most once (injective agreement).

The signature means sealed messages are not deniable, the same as the interactive handshake.

## Mailboxes

Mailbox traffic is carried as `AppMessage::Mailbox` (kind 9):

```
Mailbox = { 0: op, ? 1: to (32), ? 2: sealed, ? 3: status }
op: 1 deposit, 2 deliver, 3 receipt (status 0 declined, 1 held, 2 delivered now)
```

- **Depositing.** If A has no session with B, A sends `deposit { to: id_B, sealed }` to every live, mutually approved neighbour except B.
- **Holding.** A neighbour R accepts a deposit only if R is mutually approved with both A and B. It holds the sealed message in its encrypted state, capped at 100 messages and 16 MiB per recipient, for up to 14 days.
- **Delivering.** If R has a live session with B, it delivers immediately. Otherwise it delivers the next time B connects, then deletes its copy.
- **Receipts.** Every deposit is answered with a receipt, so the sender knows whether each mailbox holds the message, delivered it immediately, or declined it.
- **Duplicates.** B ignores duplicates by `h`, so depositing with several neighbours is safe.

Mailboxes see that A sent something to B, when, and roughly how large. They never see the contents or the sender's signature. A malicious mailbox can drop or delay messages, but cannot alter or forge them.

## Not yet done

- End-to-end read receipts. Mailbox receipts only report what the mailbox did with the message.
- Onion-routed deposits, which would hide A from the mailbox.
- Prekey bundles fetched through relays from contacts that have not handed one out yet.
