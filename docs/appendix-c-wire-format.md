# Appendix C: CBOR Schemas (v1.0)

Status: implemented in `threnody-core::{wire, handshake, ratchet, message, store}` with [`const-cbor`](https://crates.io/crates/const-cbor). Written in CDDL (RFC 8610).

## General rules

- All maps use small unsigned-integer keys and definite lengths.
- Encoders emit keys in ascending order using the preferred (shortest) encoding.
- Decoders reject duplicate keys and trailing bytes, and ignore unknown keys (spec §6.3).
- Every frame is an `Envelope`. A receiver MUST reject an unknown `major` version, because every envelope type is security-critical (spec §11).

```cddl
Envelope = {
  0 => uint,          ; protocol major version (1)
  1 => uint,          ; protocol minor version (0)
  2 => MsgType,
  3 => bstr,          ; body, encoded per MsgType
  * uint => any,
}
MsgType = &(handshake-init: 1, handshake-resp: 2, handshake-finish: 3, ratchet: 4)

; --- Appendix A ---
HS1 = { 0 => [+ Suite], 1 => bstr .size 1216 }
HS2 = { 0 => Suite, 1 => bstr .size 1120, 2 => bstr }
HS3 = { 0 => bstr }
Suite = &(chacha20-poly1305: 1, aes-256-gcm: 2)
ResponderPayload = { 0 => bstr .size 32, 1 => bstr .size 64, 2 => bstr .size 1216 }
InitiatorPayload = { 0 => bstr .size 32, 1 => bstr .size 64 }

; --- Appendix B ---
RatchetMsg    = { 0 => bstr .size 12, 1 => bstr, 2 => bstr }   ; header nonce, encrypted RatchetHeader, ciphertext
RatchetHeader = { 0 => bstr .size 1216, 1 => bstr .size 1120, 2 => uint, 3 => uint }

; --- Application layer (inside the ratchet, after unpadding) ---
AppMessage = Hello / Text / File / Approval / Cover / TunnelOffer / Group / Relay / Prekeys / Mailbox / Onion / Account
           / Direct / Tracked / Ack / Delete / Edit / Identity / React / Observed / Paths / Credential / Directory
Hello    = { 0 => 0, ? 5 => uint }                         ; feature bits (1 acks, 2 delete, 4 edit, 8 identity, 16 react,
                                                           ; 32 observed, 64 paths, 128 credentials); absent = 0
Text     = { 0 => 1, 1 => uint, 2 => tstr, ? 4 => uint, ? 5 => uint }  ; sent_ms, body, disappear after (s), sender's message id
File     = { 0 => 2, 1 => uint, 2 => bstr, 3 => tstr, ? 5 => uint,    ; sent_ms, data (≤ 8 MiB), name, sender's message id,
             ? 6 => uint, ? 7 => tstr, ? 8 => uint, ? 9 => uint }      ; flags (1 sensitive, 2 voice, 4 video), caption,
                                                                       ; album id, clip duration (ms)
Approval = { 0 => 3, 2 => bool }
Cover    = { 0 => 4 }
TunnelOffer = { 0 => 5, 2 => bstr .size 32, 3 => uint }  ; WireGuard public key, UDP port (Appendix D)
Group    = { 0 => 6, 2 => bstr .cbor GroupWire }          ; Appendix F
Relay    = { 0 => 7, 2 => bstr .cbor RelayMsg }           ; Appendix G
Prekeys  = { 0 => 8, 2 => bstr .cbor PrekeyBundle }       ; Appendix H
Mailbox  = { 0 => 9, 2 => bstr .cbor MailboxMsg }         ; Appendix H
Onion    = { 0 => 10, 2 => bstr .cbor OnionMsg }          ; Appendix I
Account  = { 0 => 11, 2 => bstr .cbor AccountMsg }        ; Appendix J
Direct   = { 0 => 12, 2 => bstr .cbor DirectMsg }         ; Appendix L
Tracked  = { 0 => 13, 2 => bstr .cbor AppMessage, 5 => uint }  ; inner message (not Tracked or Ack), id
Ack      = { 0 => 14, 2 => bstr }                          ; acknowledged ids, 8 bytes each (big-endian), ≤ 512
Delete   = { 0 => 15, 2 => bstr, 3 => bstr }               ; message ids (8 bytes each, ≤ 512), conversation id
Edit     = { 0 => 16, 2 => tstr, 3 => bstr, 5 => uint }    ; new text, conversation id, message id
Identity = { 0 => 17, 2 => bstr .cbor IdentityMsg }       ; profile or revealed identity, Appendix M
React    = { 0 => 18, 2 => tstr, 3 => bstr, 5 => uint, ? 6 => uint }  ; emoji, conversation id, message id, flags (1 = remove)
Observed = { 0 => 19, 2 => bstr, 3 => uint }               ; address (4 or 16 bytes) and port we see the peer at, Appendix N
Paths    = { 0 => 20, 2 => bstr .cbor Paths }             ; recovery paths and heartbeat, Appendix N
Credential = { 0 => 21, 2 => bstr .cbor CredMsg }         ; credential issuance and presentation, Appendix O
Directory  = { 0 => 22, 2 => bstr .cbor DirMsg }          ; relay directory requests, anonymous links only, Appendix P

GroupWire = { 0 => 1..6, 1 => bstr .size 16, ? 2 => bstr, ? 3 => tstr, ? 4 => bstr .size 32, ? 5 => uint }
          ; kind (1 key-package request, 2 key package, 3 welcome, 4 MLS message, 5 forward, 6 receipt),
          ; group id, payload, name, member (forward target or receipt subject), reference
```

## Acknowledgements

Each side's first message in a session is its `Hello`, and its feature bits say what it supports. Before this, only the initiator sent `Hello`, and peers ignored a `Hello` from the responder. Bit 1 means the side acknowledges `Tracked` messages and accepts `Tracked` and `Ack`. A peer that lacks it would drop the session on either kind, so neither is ever sent to it.

When both sides set bit 1, text, files, group messages and mailbox messages travel as `Tracked { id, inner }`, with a random 64-bit id. The sender keeps each one until an `Ack` names its id. If the session ends first, even one that only looked alive, the sender sends it again at the start of the next session with that peer. The receiver acknowledges every copy, but delivers only the first: it remembers the last 4,096 ids per peer. The sender keeps at most 256 messages or 32 MiB per peer, dropping the oldest first. Tracked messages wait until the peer's first message shows whether it supports acknowledgements. If it doesn't, they go out plain and are not tracked.

An outgoing text or file in history carries a random local id (history key 7), and its tracked copies carry the same id as a tag. When any device of the recipient's account acknowledges one, the history entry is marked delivered (key 8), and the node emits `Delivered`. Apps show this as a second tick. A group message's entry records how many members it went to (key 9) and the devices that acknowledged their copy (key 10), and it is delivered once all have. Only a member's own acknowledgement counts, not a mailbox's or a forwarder's. A forwarder that delivers a copy reports the member's acknowledgement back with a group `Receipt` (Appendix F).

Both lists are kept in encrypted state (`unacked`, `delivered-ids`), so a restart neither loses nor repeats messages. The exception is messages over 64 KiB (files), which are resent only within one run.

## Deleting and editing messages

Texts and files carry the sender's message id (its history entry's local id). The receiver records it with the entry (history key 11).

`Delete` asks the receiver to delete messages by those ids. It's only sent to peers whose `Hello` has feature bit 2. Receivers honour it in two cases:

- **From a peer:** only for messages that peer's account sent ("delete for everyone"). Anything else is ignored.
- **From one of our own devices:** for any messages in the named conversation. Our devices delete together, whether for me or for everyone. A sibling's chat with *us* is mapped to our chat with it.

`Edit` replaces a message's text, under the same rules and with feature bit 4. The entry keeps its place and is marked edited (history key 12, the time of the edit). Only text can be edited, not files. Both messages are tracked like user content, so they're resent if lost. Deletion is a request: a modified client can keep a copy, and so can a screenshot. Group messages carry no ids yet, so in groups only "delete for me" exists, on that device.

### Reactions

`React` adds or removes one emoji on a message, named by the same id as `Edit` and `Delete` use: the message's local id on the side that sent it, recorded as the remote id (history key 11) on the other. Anyone in the conversation may react to any message, with several different emoji each. Each history entry keeps the set of reactions (key 13: `[* [reactor (32), emoji]]`). The reactor is the account (or, until it's known, the device), so a person's devices count as one. A person may put at most 16 emoji on a message, and a message holds at most 256. An emoji is at most 32 bytes, with no control characters. Like `Edit`, a reaction goes to the peer's devices and to our own (with the conversation id), as tracked messages, only to devices whose `Hello` has feature bit 16. In groups, reactions travel inside MLS (Appendix F).

### Photos and files

A file can carry a caption (key 7), a sensitive flag (key 6, bit 1) and an album id (key 8). Files sent together, such as several photos, are sent as separate `File` messages with the same random album id, and the caption travels with the first. Receivers show an album as one message. A sensitive file is shown covered, and is not even decoded until the user opens it. The flag is the sender's request; a receiver can ignore it.

A *voice message* or *video message* is a `File` recorded in the app, flagged with bit 2 (voice) or bit 4 (video) of key 6, never both; key 9 carries how long it plays, in milliseconds, as the sender measured it (at most 10 minutes; absent or 0 if unknown). Receivers play it in the conversation instead of offering it as a download, and keep it privately as they keep photos. A receiver that doesn't know the bits sees an ordinary file, since unknown flag bits and keys are ignored. The reference apps record voice as AAC in MPEG-4 (Android) or Opus in Ogg (Linux), and video as H.264 with AAC in MPEG-4 (Android) or VP8 with Opus in WebM (Linux); each plays all four. Video messages are limited to a minute so they fit in one file.

Before a file is sent, the node removes identifying metadata from JPEG, PNG and WebP images (`threnody_core::media`). This covers EXIF (location, camera and serial numbers, times), XMP, IPTC, comments, text chunks, and data after the image such as a motion photo's video. The pixels are not re-encoded. Decoders still get what they need: colour profiles, transparency, animation, and a JPEG's orientation, rewritten as a minimal EXIF block. A damaged image of those types is refused rather than sent with its metadata. Apps convert other image formats (HEIC, AVIF) to JPEG before sending. This is on by default; `--keep-metadata` or the app's toggle turns it off.

## Padding (spec §9, layer 1)

Before encryption, an `AppMessage` is padded ISO/IEC 7816-4 style: a `0x80` byte, then zeros. The total length is rounded up as follows:

- to 256 bytes if smaller,
- otherwise to the next power of two, up to 64 KiB,
- beyond that, to the next multiple of 64 KiB.

## Stream framing (transport)

On stream transports, each frame is prefixed with its length as a `u32` in big-endian byte order. The maximum is 16 MiB plus 4 KiB.

## Local storage

All local state other than the two files below is kept in `<name>.state` files. Each one is encrypted with ChaCha20-Poly1305 under `KDF("state encryption key", identity_seed)`, with the file name as associated data (`Home::save_state`). This covers groups, prekeys, bundles, mailboxes, accounts, acknowledgement state and message history (`hist-p-<account or device>` and `hist-g-<group>`). Message history keeps at most 10,000 entries per conversation. A conversation's timer (key 1) is absent until chosen, in which case it follows the node's default (off unless the user chooses one). 0 means *off* on purpose, and any other value is seconds. A peer that turns its timer off sends messages without `expires_in_s`, and the other side records that as 0. A file transfer is stored as an entry whose text is its caption (usually empty) and a file record (key 6: `{ 0 => name, 1 => size, ? 2 => location, ? 3 => sensitive, ? 4 => album, ? 5 => [video bool, duration_ms uint] }`, key 5 marking a voice or video message). Outgoing entries may have a local id (key 7) and a delivered flag (key 8, see *Acknowledgements*). The record keeps where the app saved the file, not the file's contents. Older readers skip the key. Disappearing messages are deleted on the first load or save after they expire, and by a sweep that runs every minute.

Both files are written with mode 0600 inside a directory with mode 0700.

```cddl
IdentityFile = { 0 => 1, 1 => bstr .size 32 }               ; Ed25519 seed
ContactsFile = { 0 => 1, 1 => [* Contact] }
Contact = {
  0 => bstr .size 32,    ; identity key
  ? 1 => tstr,           ; petname
  2 => bool,             ; local approval
  3 => bool,             ; remote approval
  4 => bool,             ; safety number verified
  ? 5 => tstr,           ; last dialable address
  6 => uint, 7 => uint,  ; first / last seen (ms)
  ? 8 => bstr .size 32,  ; LAN discovery key (Appendix E)
  ? 9 => bstr .size 32,  ; account id (Appendix J)
  10 => uint,            ; approval changed (ms), for own-device sync
  ? 11 => [* bstr .size 32], ; previous discovery keys
  ? 12 => bool,          ; accepted: we want their messages (absent = true, for contacts from before requests)
  ? 13 => bool,          ; blocked
}
```
