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
           / Direct / Tracked / Ack
Hello    = { 0 => 0, ? 5 => uint }                         ; feature bits (1 = acknowledgements); absent = 0
Text     = { 0 => 1, 1 => uint, 2 => tstr, ? 4 => uint }    ; sent_ms, body, disappear after (s)
File     = { 0 => 2, 1 => uint, 2 => bstr, 3 => tstr }      ; sent_ms, data (≤ 8 MiB), name
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

GroupWire = { 0 => 1..5, 1 => bstr .size 16, ? 2 => bstr, ? 3 => tstr, ? 4 => bstr .size 32 }
          ; kind (1 key-package request, 2 key package, 3 welcome, 4 MLS message, 5 forward),
          ; group id, payload, name, member to forward to
```

## Acknowledgements

Each side's first message in a session is its `Hello`, and its feature bits say what it supports. Before this, only the initiator sent `Hello`, and peers ignored a `Hello` from the responder. Bit 1 means the side acknowledges `Tracked` messages and accepts `Tracked` and `Ack`. A peer that lacks it would drop the session on either kind, so neither is ever sent to it.

When both sides set bit 1, text, files, group messages and mailbox messages travel as `Tracked { id, inner }`, with a random 64-bit id. The sender keeps each one until an `Ack` names its id. If the session ends first, even one that only looked alive, the sender sends it again at the start of the next session with that peer. The receiver acknowledges every copy, but delivers only the first: it remembers the last 4,096 ids per peer. The sender keeps at most 256 messages or 32 MiB per peer, dropping the oldest first. Tracked messages wait until the peer's first message shows whether it supports acknowledgements. If it doesn't, they go out plain and are not tracked.

An outgoing text or file in history carries a random local id (history key 7), and its tracked copies carry the same id as a tag. When any device of the recipient's account acknowledges one, the history entry is marked delivered (key 8), and the node emits `Delivered`. Apps show this as a second tick. Group messages go to many members and aren't marked.

Both lists are kept in encrypted state (`unacked`, `delivered-ids`), so a restart neither loses nor repeats messages. The exception is messages over 64 KiB (files), which are resent only within one run.

## Padding (spec §9, layer 1)

Before encryption, an `AppMessage` is padded ISO/IEC 7816-4 style: a `0x80` byte, then zeros. The total length is rounded up as follows:

- to 256 bytes if smaller,
- otherwise to the next power of two, up to 64 KiB,
- beyond that, to the next multiple of 64 KiB.

## Stream framing (transport)

On stream transports, each frame is prefixed with its length as a `u32` in big-endian byte order. The maximum is 16 MiB plus 4 KiB.

## Local storage

All local state other than the two files below is kept in `<name>.state` files. Each one is encrypted with ChaCha20-Poly1305 under `KDF("state encryption key", identity_seed)`, with the file name as associated data (`Home::save_state`). This covers groups, prekeys, bundles, mailboxes, accounts, acknowledgement state and message history (`hist-p-<account or device>` and `hist-g-<group>`). Message history keeps at most 10,000 entries per conversation. A file transfer is stored as an entry with empty text and a file record (key 6: `{ 0 => name, 1 => size, ? 2 => location }`). Outgoing entries may have a local id (key 7) and a delivered flag (key 8, see *Acknowledgements*). The record keeps where the app saved the file, not the file's contents. Older readers skip the key. Disappearing messages are deleted on the first load or save after they expire, and by a sweep that runs every minute.

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
}
```
