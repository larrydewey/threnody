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
AppMessage = Hello / Text / File / Approval / Cover / TunnelOffer / Group / Relay / Prekeys / Mailbox
Hello    = { 0 => 0 }
Text     = { 0 => 1, 1 => uint, 2 => tstr }                 ; sent_ms, body
File     = { 0 => 2, 1 => uint, 2 => bstr, 3 => tstr }      ; sent_ms, data (≤ 8 MiB), name
Approval = { 0 => 3, 2 => bool }
Cover    = { 0 => 4 }
TunnelOffer = { 0 => 5, 2 => bstr .size 32, 3 => uint }  ; WireGuard public key, UDP port (Appendix D)
Group    = { 0 => 6, 2 => bstr .cbor GroupWire }          ; Appendix F
Relay    = { 0 => 7, 2 => bstr .cbor RelayMsg }           ; Appendix G
Prekeys  = { 0 => 8, 2 => bstr .cbor PrekeyBundle }       ; Appendix H
Mailbox  = { 0 => 9, 2 => bstr .cbor MailboxMsg }         ; Appendix H

GroupWire = { 0 => 1..4, 1 => bstr .size 16, ? 2 => bstr, ? 3 => tstr }
          ; kind (1 key-package request, 2 key package, 3 welcome, 4 MLS message), group id, payload, name
```

## Padding (spec §9, layer 1)

Before encryption, an `AppMessage` is padded ISO/IEC 7816-4 style: a `0x80` byte, then zeros. The total length is rounded up as follows:

- to 256 bytes if smaller,
- otherwise to the next power of two, up to 64 KiB,
- beyond that, to the next multiple of 64 KiB.

## Stream framing (transport)

On stream transports, each frame is prefixed with its length as a `u32` in big-endian byte order. The maximum is 16 MiB plus 4 KiB.

## Local storage

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
}
```
