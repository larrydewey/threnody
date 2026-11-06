# Appendix T: Adding contacts by NFC tap (v1)

## Status

Implemented. Android takes both roles; iPhones and Linux computers with a PC/SC NFC reader can tap an Android phone. The node-to-node protocol is unchanged: a tap hands over an ordinary invite link (Appendix S), and the session that follows is an ordinary handshake.

## Roles

Two people add each other by touching devices. One device shows its invite: it is the **card**. The other taps it: it is the **reader**.

- **Why card and reader.** Android has had no peer-to-peer NFC since Android 10 removed Android Beam. A phone can emulate a card (Host Card Emulation, HCE) or read one (reader mode), not both at once: reader mode turns card emulation off. So the devices keep the roles they have with a QR code. The one showing its invite is the card; the one choosing Scan is the reader.
- **What each gets.** The reader gets the card's invite link, exactly what its QR code holds. The card gets the reader's 20-byte fingerprint and nothing else.
- **Why only a fingerprint.** The card learns the fingerprint anyway the moment the reader connects. The reader's invite link would also give away its addresses and invite key to whatever device it touched, before its user confirms anything, and would bypass `prefer_onion` (dialing through approved contacts so that a new contact doesn't see your IP address). So the reader dials, as after scanning a QR code, and the card's user says in advance to take the reader's session as a contact.

## Protocol

```
AID       = F0 74 68 72 65 6E 6F 64 79                  -- 0xF0 || "threnody" (proprietary AID, 9 bytes)
VERSION   = 01
CHUNK     = 240                                          -- data bytes per APDU (short APDUs only)
MAX_LINK  = 512                                          -- bytes of invite link, UTF-8
PADDED    = 2 + MAX_LINK                                 -- u16 BE length || link || zeros

SELECT    R→C  00 A4 04 00 09 AID 00
          C→R  "TN" max_version(01) flags(00) 90 00      -- flags reserved: send 0, ignore on receipt
OFFER     R→C  80 D0 P1=i P2=(ver<<1 | last) Lc data     -- chunk i of M1 = ek (X-Wing public, 1216 B)
          C→R  90 00
FETCH     R→C  80 D2 P1=i 00 00
          C→R  more(00|01) || chunk_i (≤240) || 90 00    -- chunk i of M2 = ct (1120 B) || box_C (530 B)
GIVE      R→C  80 D4 00 00 24 box_R                       -- M3 = box_R (36 B), one APDU
          C→R  90 00
```

A tap is 1 + 6 + 7 + 1 = 15 APDUs, about 3 KB. Every command is at most 5 + 240 bytes and every response at most 243, so no extended-length APDUs are needed (support for them varies across Android HCE and iOS). The INS codes are even, avoiding the odd ones ISO 7816-3 historically reserves, and GET RESPONSE (C0) isn't used.

Status words:

| SW | Meaning |
|---|---|
| `9000` | OK |
| `6A82` | A SELECT of another AID |
| `6D00` | An unknown INS |
| `6985` | Out of order, nothing selected, or already done |
| `6A80` | Bad lengths, or a sealed box that doesn't open |
| `6A86` | A FETCH index out of range, or an OFFER in a version the card doesn't speak |

**Card rules.**

- SELECT resets any state but Done to Selected.
- Done is sticky: once an exchange completes, the card answers `6985` to everything, SELECT included. One exchange per arming.
- OFFER chunks arrive in order (P1 is the next index), every chunk but the last is exactly 240 bytes, and every chunk names the same version. M1 is exactly 1216 bytes and must be a valid X-Wing public key.
- FETCH may be repeated for any index in range.
- GIVE data is exactly 36 bytes, in one APDU, and the fingerprint it opens to differs from the card's own.
- Any violation moves the card to Failed, which only SELECT leaves.

**Reader rules.**

- The SELECT response must start with `"TN"` and `max_version`; the reader speaks `min(max_version, 1)`. A SELECT answered with anything but `9000`, or a malformed response, means *not an invite*. `max_version = 0` means *unsupported*, and so does `6A86` in answer to the first OFFER chunk: a card that has dropped the version the reader offers still names its own highest in SELECT.
- After SELECT, any status word but `9000`, or a response that doesn't fit the step, is *garbled*.
- The card's link must parse as an invite link (Appendix S) and name a fingerprint other than the reader's own; otherwise the tap is *garbled*.
- The reader has the card's link once M2 opens and reports success after the GIVE's `9000`. If the tag goes away between those two points it still has the link, but the card may not have its fingerprint (a "half tap"): the apps then treat it as a scanned QR code.
- A tag that goes away before M2 opens means the devices *moved*. A reader gives up after 32 round trips.

## Key schedule

```
ss          = X-Wing Encaps(ek) on the card / Decaps on the reader
h           = KDF(TAP_TRANSCRIPT, [AID, [ver], ek, ct])               -- 32 B
k_C || k_R  = KDF(TAP_KEYS, [ss, h])                                   -- 64 B
box_C       = ChaCha20-Poly1305(k_C, nonce = 0^12, ad = h, pad(link_C)) -- each key used once
box_R       = ChaCha20-Poly1305(k_R, nonce = 0^12, ad = h, fp_R)        -- fp_R: reader's 20-byte fingerprint
pad(l)      = u16_be(len l) || l || 0^(MAX_LINK - len l)
```

`KDF` is the length-prefixed BLAKE3 derivation of section 3.2, with labels `threnody v1 2026-11-05 nfc tap transcript` and `threnody v1 2026-11-05 nfc tap keys`. The reader makes a fresh X-Wing key for every tag it sees, and the card draws fresh encapsulation randomness for every exchange, so each key seals exactly one box. Unpadding refuses a length over 512, non-zero padding and invalid UTF-8.

The key exchange is unauthenticated, like a QR code: what binds the two people is touching distance (a few centimetres) and, later, comparing safety numbers.

## Versions and negotiation

The card reports the highest version it speaks; the reader states its choice in every OFFER's P2, and that choice is bound into `h`. A future v2 card that still speaks v1 therefore works with v1 readers, a v1 card refuses a v2 OFFER with `6A86`, and a future v2 reader falls back to v1 when the card says 1. Flags are reserved and ignored. An app older than this appendix registers no service for the AID, so Android itself answers `6A82` and the reader reports that the tapped phone isn't showing an invite.

## Accepting the result

The reader's user confirms the card's link (Connect) and the reader dials it with the fingerprint pinned (`Node::reach_invite`), as after a scan.

The card's user sees the reader's fingerprint and chooses Add, which calls `Node::expect_contact(fp, name, EXPECT_FOR)`:

- For ten minutes the first session with that fingerprint, inbound or dialed, is a contact the user wants (`accepted`, named as given if it has no name yet), not a message request. Under the `ContactsOnly` policy it is admitted.
- Expectations are in memory only, at most 16 (the one expiring soonest makes room), and the first session with the peer consumes one, whichever side dialed: across networks the hole punch (Appendix S) may have the card's side dial, and the name must still apply. A duplicate session that loses the tie-break doesn't consume it.
- A contact that already sent a request is accepted at once.
- An expectation never marks a contact verified or approved, never unblocks it, and never overrides a revocation or the `ApprovedOnly` policy. Your own fingerprint and blocked contacts are refused.

The reader also calls `expect_contact` before dialing, so that a card that reached it first (a request already waiting) is accepted too. A restart drops expectations: the session then arrives as a message request, as after a QR scan today.

## Platforms

| Platform | Card | Reader |
|---|---|---|
| Android | "My invite", while the dialog shows, the app is in front and the phone is unlocked | "Add a contact → Scan" listens for a tap beside the camera |
| iOS | No: card emulation needs a restricted Apple entitlement | "Tap an Android phone" (Core NFC, ISO 7816 tag session) |
| Linux | No: a PC/SC reader can't act as a card | "Tap a phone" with a PC/SC (CCID) NFC reader and `pcscd` |

Anonymous identities' invites and device-link codes are never served over NFC, and the reader always reveals the main identity's fingerprint only.

## Privacy

| Adversary | Learns / can do | Compared with a QR code |
|---|---|---|
| Passive RF eavesdropper (metres away with an antenna) | That a Threnody tap happened (the AID is in the clear), and when. Not the card's link nor the reader's fingerprint: both are sealed under a fresh X-Wing encapsulation, and the link is padded to 512 bytes so the number of addresses doesn't show. ML-KEM covers harvest-now-decrypt-later. | Someone who sees an invite QR code learns the whole link. |
| Active skimmer reading the card (a few cm) | The card's invite link, but only while "My invite" is on screen and the phone is unlocked, and once per arming. | Identical to photographing the QR code. |
| Skimmer probing for "has Threnody" | Nothing when no invite is showing: the HCE service is disabled, so Android answers `6A82` as on a phone without Threnody. | Same. |
| A device that makes the card expect a forged fingerprint | Can't add itself silently: the card's user sees the fingerprint and must press Add, and even then it is a contact, never approved or verified. | The scanner confirms the same way. |
| A fake card the reader taps | The reader's fingerprint only: no addresses, no invite key. Its IP stays hidden when its dial goes through contacts first (`prefer_onion`, on by default). The reader's user sees the link and must press Connect. | A QR sticker learns nothing about the scanner; here a long-term fingerprint leaks to a device the user chose to touch. |
| Relay or man in the middle between the devices | Could swap payloads: the ephemeral key exchange is unauthenticated, as a QR code is. Limited by touching distance and caught by comparing safety numbers, which stay unverified. | Same trust as a QR code. |
| Resource exhaustion | Bounded state: M1 is 1216 bytes, M3 36, chunk indices are checked, `run` stops after 32 round trips, at most 16 expectations. The card never panics (mutation test and fuzzing). | n/a |

Every protection is always on, as none costs the feature anything: encryption and padding, arming only while the invite shows and the phone is unlocked, one exchange per arming, the reader revealing only its fingerprint, confirmation on both sides, and expectations that expire, are capped and are never stored.

## Compatibility

No node wire change: no new message, field or Hello feature bit. An older peer is dialed and answers as for a scanned QR code; an older card simply doesn't arm NFC, and an older reader doesn't listen.

## Implementation

- `threnody-core::tap`: the whole protocol, sans-IO (`Card`, `Reader`, `run`), with unit tests, `robustness::tap_card_survives_mutated_apdus` and the `nfc_card` fuzz target.
- `threnody-net::handle`: `Node::expect_contact` and the expectations it keeps; `Node::spawn_session` and the accept policy consult them.
- `threnody-ffi::tap`: `NfcCard`, `NfcReader`, the foreign `NfcTransport` trait, `NfcOutcome`/`NfcFailure`, and `ThrenodyNode::{nfc_reader, expect_contact}`.
- Android: `Nfc.kt` (`NfcInviteService`, a `HostApduService` disabled until an invite shows; reader mode over `IsoDep`), `res/xml/nfc_invite.xml`, and the invite and scan screens.
- iOS: `NfcTap.swift` (`NFCTagReaderSession`) and `AddContactView`.
- Linux: `apps/linux/src/nfc.rs` (PC/SC through the `pcsc` crate, cargo feature `nfc`, on by default) and the "Add a contact" dialog.

## Not yet done

- Anonymous identities' invites over NFC.
- iOS or Linux as the card.
- Handing over the reader's full invite as well, so that the card could dial too. It would have to be an explicit choice per tap.
- Marking safety numbers verified after a tap: the key exchange can't rule out a relay, so contacts stay unverified until compared.
