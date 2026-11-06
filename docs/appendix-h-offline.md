# Appendix H: Offline Delivery (v1)

Status: specified here, modelled in [`proofs/sealed.spthy`](../proofs/README.md), implemented in `threnody-core::prekey` and `threnody-net::mailbox`.

The interactive handshake (Appendix A) needs both ends online at the same moment. Offline delivery adds two pieces so a message can reach a peer that is currently away:

- **Sealed messages**, which can be decrypted without any interaction, using prekeys the recipient handed out earlier.
- **Mailboxes**, which are approved neighbours that hold sealed messages until the recipient shows up.

## Prekeys

Each device keeps:

- **A signed prekey (SPK).** An X-Wing key pair with id `spk_id`, rotated every 7 days. Bundles advertise it as valid (`expiry_ms`) for 21 days from its creation, and a retired SPK is kept until that expiry, so messages still in flight can be read. Up to two retired SPKs can be live at once.
- **One-time prekeys (OPKs).** X-Wing key pairs, each with an id. Each is reserved for one contact, and its secret is deleted on first use.

```
sig_spk = Sign(id_B, L("prekey") || L(id_B) || L(u32 spk_id) || L(spk_pub) || L(u64 expiry_ms))
sig_opk = Sign(id_B, L("one-time prekey") || L(id_B) || L(u32 opk_id) || L(opk_pub))
Bundle  = { 0: id_B, 1: spk_id, 2: spk_pub, 3: expiry_ms, 4: sig_spk, 5: [ { 0: opk_id, 1: opk_pub, 2: sig_opk } * ] }
```

- **Distribution.** There is no directory. Once a session is mutually approved, each side sends the other a fresh bundle (`AppMessage::Prekeys`, kind 8) carrying the current SPK and 5 new OPKs reserved for that peer.
- **Storage on the receiving side.** The receiver verifies every signature against the identity it authenticated in the session, then stores the bundle in its encrypted state. A new bundle replaces the old one.
- **Clean-up.** A device keeps the OPK secrets of a bundle until the bundle's `expiry_ms`, even after newer bundles replace it at the peer: a message the peer sealed to the old bundle may still be waiting at a mailbox. To bound storage, it keeps the OPK secrets of at most the last eight bundles it sent a peer and deletes older ones early. A message sealed to a deleted OPK can't be opened, so a peer whose sessions with us flap more than seven times while a message waits can still lose it.

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

1. **Finds the keys.** Looks up the SPK (current or retired, unexpired) and the OPK, if one was used. If anything is missing, it rejects the message.
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
Mailbox = { 0: op, ? 1: to (32), ? 2: sealed, ? 3: status, ? 4: ref (uint) }
op: 1 deposit, 2 deliver, 3 receipt (status 0 declined, 1 held, 2 delivered now)
```

A direct deposit carries a `ref`, a random 64-bit number, and the receipt for it repeats it, so no receipt is taken for another deposit's. Being random, refs say nothing about how many other messages the sender placed in between. Mailboxes that predate `ref` ignore it and answer without one; their receipts are matched to that sender's deposits with them, for the same recipient, in order. A receipt that names a `ref` the sender no longer waits for is ignored.

- **Depositing.** If A has no session with B, A leaves the sealed message with every live, mutually approved neighbour except B. By default A deposits anonymously, as described below, and skips mailboxes that no circuit can reach. If no circuit can reach any mailbox, A sends `deposit { to: id_B, sealed }` directly.
- **Holding.** A neighbour R accepts a direct deposit only if R is mutually approved with both A and B. It accepts an anonymous deposit only if R is mutually approved with B.
- **Storage limits.** R holds the sealed message in its encrypted state, capped at 100 messages and 16 MiB per recipient, for up to 14 days. A sweep every minute drops messages held longer, whether or not B ever returns, and everything held for B goes as soon as R deletes or blocks B or B's account removes it.
- **Delivering.** If R has a live session with B, it delivers immediately. Otherwise it delivers the next time B connects, if B is still mutually approved, then deletes its copy. A copy too large for the acknowledgement state (over 64 KiB, Appendix C) is deleted only once B acknowledges it, so a restart of R in between doesn't lose it; R sends it again after a restart. If B gave R a wake-up endpoint, R wakes B when it starts holding the message (Appendix R).
- **Deposit receipts.** Every deposit is answered with a deposit receipt, so the sender knows whether each mailbox holds the message, delivered it immediately, or declined it. These report only what the mailbox did; whether the recipient got or read the message comes in end-to-end receipts (*Receipts* below).
- **When every mailbox declines.** For a text or group message, A keeps the sealed message's plain copy until it hears back. If every mailbox it asked declines, or a direct deposit has no receipt after 2 minutes, A sends the message over a session with B if one has come up meanwhile, and otherwise keeps it for the next one, as if it had never been sealed (Appendix C, *Acknowledgements*). A mailbox that holds or delivers it ends the wait. A deposit that timed out is taken out of A's queue to that mailbox, so it isn't sent again when the mailbox returns. The messages still waiting are kept in A's encrypted state. If A restarts before hearing back, each one waits for B the same way, and its queued deposits are withdrawn. A mailbox may still have taken a copy whose receipt was lost, so B can get a text twice, once from A and once sealed. B keeps the first copy and drops any later text from the same sender device with the same message id.
- **Duplicates.** B ignores duplicates by `h`, so depositing with several neighbours is safe.

### Anonymous deposits

A mailbox that takes a direct deposit learns that A wrote to B. To hide A, the deposit travels over an onion circuit (Appendix I) whose last hop is the mailbox M:

1. A picks a path `R1, R2, M`. R1 is a live, mutually approved neighbour, R2 is another mutually approved contact, and neither is M or B.
2. A builds the circuit and sends `DEPOSIT` to M: `to (32) ‖ total_len (u32 BE) ‖ first bytes`. The rest of the sealed message follows in `DATA` cells.
3. Once `total_len` bytes have arrived, M holds or delivers the message exactly as it would a direct deposit. It answers `DEPOSITED` with the status byte, then tears the circuit down. The status is never "delivered now": M doesn't know who is asking, and that would tell them B is online. A message passed straight on is reported as held.

M only sees R2, so it learns that someone sent something to B, but not who. M declines a `DEPOSIT` if the circuit already carries a stream or another deposit, if `to` isn't M's mutually approved contact, if `total_len` exceeds 16 MiB, or if the neighbour carrying the circuit has already passed 30 anonymous deposits in the current minute. That neighbour is the only party M can hold responsible. Over an anonymous link (Appendix P), where a new identity costs nothing, the neighbour's network (a /24 or a /56) counts instead. M holds deposits still arriving in memory, and counts the bytes that have actually arrived, not the `total_len` a head claims. Deposits over circuits from M's mutually approved contacts, and deposits over all other circuits, each have room for 64 MiB, and within each, one carrier (a contact, or a stranger's network) has room for 16 MiB. A deposit whose next bytes don't fit is declined and dropped, and so is one that isn't complete 120 s after the circuit's `CREATE` (Appendix I). Strangers who fill their room with bytes they really send can still make M decline other strangers' deposits for a while, and those senders then fall back to direct deposit; deposits from contacts' circuits are not affected.

Anonymous deposits are on by default. With onion routing turned off (`--no-onion` or the app's toggle), deposits are direct. If every circuit to a mailbox fails, A deposits with that mailbox directly rather than lose the message. With only one mailbox and no other contacts there is no path, so the deposit is direct. Either way, with onion routing on, the app is told (`Event::Fallback`, Appendix I *Fallback*) and shows a quiet notice in the chat.

Mailboxes see that B received something, when, and roughly how large. A mailbox taking a direct deposit also sees that A sent it. They never see the contents or the sender's signature. A malicious mailbox can drop or delay messages, but cannot alter or forge them.

### Receipts

A text that arrives sealed came while its sender couldn't reach us, and the sender is likely still away when we open it. So delivery and read receipts (`Receipt`, Appendix C, *Typing and read receipts*) can travel the same way:

- **What.** When B opens a sealed text from a contact it accepted, it owes the sending device a *delivered* receipt for the text's id. When B's app displays a message (however it came) and read receipts are on, B owes the sending device a *read* receipt. Read implies delivered. Nothing is ever owed for a contact B hasn't accepted or has blocked.
- **Where.** A receipt goes to the device that sent the message. If B has a session with it, the receipt goes at once, tracked. Otherwise it waits in B's encrypted state (`receipts-out`, Appendix C, *Local storage*), per device, and goes at the start of the next session with that device. If none comes first, it is sealed for that device and deposited with B's mailboxes like any sealed message, anonymously by default. Everything waiting for one device goes as one sealed message, with at most 512 ids per list (more go as several).
- **When.** A sealed receipt waits a random time from 30 seconds to 5 minutes after the first receipt for that device is queued, and receipts queued meanwhile join it. At most one sealed receipt goes to a device every 15 minutes. Receipts not sent within 14 days, the longest a mailbox holds anything, are dropped.
- **To whom.** A `Receipt` is never sent to a device whose latest `Hello` lacks bit 131072, live or sealed. B decides whether to seal one by the bits the device sent in its latest session, which B keeps with the contact. One left tracked from an earlier session is dropped, not sent, when the device's new `Hello` lacks the bit. A device with bit 512 but not 131072 gets the reads that waited as `Read` at its next session, and the delivered ones are dropped. No sealed receipt is attempted while no mailbox is reachable, so B doesn't spend one of the sender's one-time prekeys for nothing.
- **Once.** B records on the incoming entry that it reported the message read (history key 14), so a restart or a second display doesn't report it again. Sealed copies from several mailboxes are opened once (by `h`), so one text earns one delivered receipt. Marking delivered and read at the sender is idempotent, so a receipt that comes twice, or from two of B's devices, changes nothing the second time.
- **The switch.** With read receipts off, nothing displayed is reported, and turning them off drops every read still waiting, so nothing leaves after the user says no, even through a mailbox hours later. The switch is kept with the waiting receipts, so it holds from the moment the node starts. Delivered receipts can't be turned off, just as acknowledgements can't.
- **Older messages.** Reads of messages that arrived before this version first ran are reported live only, never queued or sealed, so upgrading doesn't seal receipts for the whole backlog.
- **Revocation.** A receipt waiting for a device whose contact is then blocked or removed, or which its account removes, is dropped.

From a sealed receipt itself a mailbox learns no more than from a sealed message: it is an `AppMessage` sealed and padded exactly as a text is, and it spends a one-time prekey when one is left, as a text does, so neither its form nor its size class tells it apart. A mailbox does see one more thing: deposits for A that follow its deliveries to B. A mailbox that serves both could link them as correspondents by timing. The random delay and batching blur this, and anonymous deposits hide who deposited, but a mailbox that sees many such pairs can still infer the link statistically. This is the same link a real reply would leak, with less precise timing. Each sealed receipt also takes a place in the mailbox's 100 held messages for A and may wake A's device (Appendix R); the 15-minute gap bounds both.

## Not yet done

- Prekey bundles fetched through relays from contacts that have not handed one out yet.
