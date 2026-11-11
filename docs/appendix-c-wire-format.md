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
           / Typing / Read / Call / Media / Pin / List / Tunnel / Push / Receipt
Hello    = { 0 => 0, ? 5 => uint }                         ; feature bits (1 acks, 2 delete, 4 edit, 8 identity, 16 react,
                                                           ; 32 observed, 64 paths, 128 credentials, 256 typing,
                                                           ; 512 read, 1024 calls, 2048 pin, 4096 lists,
                                                           ; 8192 tunnel, 16384 push, 32768 timed probes, 65536 timer off,
                                                           ; 131072 receipts, 262144 device sync); absent = 0
Text     = { 0 => 1, 1 => uint, 2 => tstr, ? 4 => uint, ? 5 => uint,   ; sent_ms, body, disappear after (s), sender's message id,
             ? 6 => uint,                                              ; flags (8 forwarded, 16 timer off)
             ? 10 => uint, ? 11 => bstr .size 32, ? 12 => tstr,    ; quoted message id, its sender's device, its text (≤ 200 chars)
             ? 13 => bstr .cbor LinkPreview }                      ; a card for a link in the body (*Link previews*)
LinkPreview = { 0 => tstr, ? 1 => tstr, ? 2 => tstr,                ; URL, title (≤ 200 chars), description (≤ 300),
                ? 3 => tstr, ? 4 => bstr }                         ; site name (≤ 100), JPEG thumbnail (≤ 24 KiB)
File     = { 0 => 2, 1 => uint, 2 => bstr, 3 => tstr, ? 5 => uint,    ; sent_ms, data (≤ 8 MiB), name, sender's message id,
             ? 6 => uint, ? 7 => tstr, ? 8 => uint, ? 9 => uint }      ; flags (1 sensitive, 2 voice, 4 video, 8 forwarded), caption,
                                                                       ; album id, clip duration (ms)
Approval = { 0 => 3, 2 => bool }
Cover    = { 0 => 4 }
TunnelOffer = { 0 => 5, 2 => bstr .size 32, 3 => uint }  ; WireGuard public key, UDP port (Appendix D); may repeat within a session
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
Typing     = { 0 => 23, 2 => bool }                        ; typing now or not; never tracked, sealed or stored
Read       = { 0 => 24, 2 => bstr, 3 => bstr }             ; ids displayed (8 bytes each, ≤ 512), conversation id
Call       = { 0 => 25, 2 => bstr .cbor CallMsg }          ; call signalling, Appendix Q
Media      = { 0 => 26, 2 => bstr }                        ; one sealed call media packet on links without datagrams, Appendix Q
Pin        = { 0 => 27, 3 => bstr, 5 => uint, ? 6 => uint }  ; conversation id, message id, flags (1 = unpin)
List       = { 0 => 28, 2 => bstr }                        ; a shared list's state, see *Lists*
Tunnel     = { 0 => 29, 2 => bstr }                        ; one WireGuard packet, Appendix D
Push       = { 0 => 30, 2 => bstr .cbor PushReg }         ; wake-up endpoint or its withdrawal, mutual contacts only, Appendix R
Receipt    = { 0 => 31, 2 => bstr .cbor ReceiptMsg }      ; delivery and read receipts, live or sealed, Appendix H

GroupWire = { 0 => 1..10, 1 => bstr .size 16, ? 2 => bstr, ? 3 => tstr, ? 4 => bstr .size 32, ? 5 => uint,
              ? 6 => bstr .size 32, ? 7 => [* bstr], ? 8 => [uint, uint, bstr .size 8, bstr .size 8],
              ? 9 => bstr .size 64 }
          ; kind (1 key-package request, 2 key package, 3 welcome, 4 MLS message, 5 forward, 6 receipt,
          ; 7 leave, 8 add device, 9 pass welcome, 10 resync),
          ; group id, payload, name, member (forward target, receipt subject, device, joiner or stale device),
          ; reference, owner (add device), the proposals a commit references, sync hint
          ; (epoch, term, epoch authenticator and coordinator, 8 bytes each),
          ; the leaver's signed leave statement (leave; absent from 0.19.0 and older, whose readers skip it); see Appendix F

PushReg = { ? 1 => tstr, ? 2 => bstr .size 65, ? 3 => bstr .size 16, ? 4 => uint, ? 5 => uint }
          ; endpoint URL, Web Push p256dh and auth (both or neither), cover wake-ups per day (≤ 24),
          ; expires_ms; empty = withdrawn (Appendix R)

ReceiptMsg = { ? 0 => bstr, ? 1 => bstr }
          ; delivered ids, read ids: the sender's message ids, 8 bytes each (big-endian),
          ; ≤ 512 per list, ≥ 1 in all (Appendix H, *Receipts*)
```

`Tunnel` (`{ 0 => 29, 2 => bstr }`) carries one WireGuard packet for the tunnel with the peer (Appendix D, *Over the session*), only to devices whose `Hello` has bit 8192. It is sent at once even under cover traffic, like call media. On QUIC sessions the same packets go as QUIC datagrams when they fit, recognised by WireGuard's header (a message type from 1 to 4, then three zero bytes) rather than a call id.

`Push` (`{ 0 => 30, 2 => bstr .cbor PushReg }`) hands a mutually approved contact the UnifiedPush endpoint at which it may wake this device, or withdraws it (an empty `PushReg`). It is sent once per session, only to devices whose `Hello` has bit 16384, and ignored from anyone not mutually approved and by anonymous identities. See Appendix R.

## Acknowledgements

Each side's first message in a session is its `Hello`, and its feature bits say what it supports. A side may leave out bits for messages it understands but can't act on: a build without call media clears bit 1024, so callers don't ring a device that can't take the call (Appendix Q sends calls only to devices whose `Hello` has it). Anonymous links (Appendix P) always send the full set, so they don't tell builds apart. Before this, only the initiator sent `Hello`, and peers ignored a `Hello` from the responder. Bit 1 means the side acknowledges `Tracked` messages and accepts `Tracked` and `Ack`. A peer that lacks it would drop the session on either kind, so neither is ever sent to it.

When both sides set bit 1, text, files, group messages and mailbox messages travel as `Tracked { id, inner }`, with a random 64-bit id. The sender keeps each one until an `Ack` names its id. If the session ends first, even one that only looked alive, the sender sends it again at the start of the next session with that peer. The receiver acknowledges every copy, but delivers only the first: it remembers the last 4,096 ids per peer, and 65,536 in all, forgetting first the peers it heard from longest ago, strangers before the contacts it accepted. A text it can't store (a full disk, no file descriptors left) it neither acknowledges nor delivers, so the sender keeps it and sends it again in the next session. The sender keeps at most 256 messages or 32 MiB per peer, dropping the oldest first. Changes to conversations (`Delete`, `Edit`, `React`, `Pin` and `List`) count separately, with 256 places of their own, so a run of them can't push out texts held for the same peer. Tracked messages wait until the peer's first message shows whether it supports acknowledgements. If it doesn't, they go out plain and are not tracked.

An outgoing text or file in history carries a random local id (history key 7), and its tracked copies carry the same id as a tag. When any device of the recipient's account acknowledges one, the history entry is marked delivered (key 8), and the node emits `Delivered`. Apps show this as a second tick. A group message's entry records how many members it went to (key 9) and the devices that acknowledged their copy (key 10), and it is delivered once all have. Only a member's own acknowledgement counts, not a mailbox's or a forwarder's. A forwarder that delivers a copy reports the member's acknowledgement back with a group `Receipt` (Appendix F).

Both lists are kept in encrypted state (`unacked`, `delivered-ids`), so a restart neither loses nor repeats messages. Deleting, blocking or removing a peer drops both lists' entries for it. The receiver saves an id as delivered only after it has handled the message (a text is in history by then), so a crash in between takes the resent copy rather than dropping it as a duplicate. The exception is messages over 64 KiB (files), which are resent only within one run. A mailbox keeps its own copy of a delivery that large until it is acknowledged (Appendix H).

### Typing and read receipts

`Typing` (feature bit 256) tells the devices of the peer's account whether we are typing in our chat with them. It is a transient hint: never tracked, never sealed for a mailbox, never stored.

`Read` (feature bit 512) tells the peer's devices that the messages with these ids, the ids it sent them with, were displayed. It goes only over live sessions, never tracked or sealed, and only to devices whose `Hello` lacks bit 131072. The sender marks those outgoing entries read (history key 14, the time it learned) and emits `Read`; apps show an eye. A `Read` from one of our own devices is ignored.

`Receipt` (`{ 0 => 31, 2 => bstr .cbor ReceiptMsg }`, feature bit 131072) replaces `Read` with devices that have that bit. It names the sender's ids that arrived and those that were displayed (read implies delivered). Live, it travels as `Tracked`, so a session that dies resends it. A reader that can't reach the sending device keeps it and sends it at the start of the next session, or seals it for mailboxes (Appendix H, *Receipts*). The sender marks the named outgoing entries delivered (key 8, emitting `Delivered` for each) and the read ones read (key 14, emitting `Read`). A `Receipt` is never sent to a device whose latest `Hello` lacks the bit, which an older build would answer by ending the session. One waiting from an earlier session for a device whose `Hello` now lacks it is not sent: its read ids go as a `Read` if the device has bit 512, and the rest are dropped. Every `Read` and `Receipt` names at most 512 ids per list; a longer report goes as several. Read receipts can be turned off (they are on by default). Nothing displayed is then reported, and reads still waiting to be sent are dropped. A `Receipt` already sent but not yet acknowledged, which would go again in the next session, keeps its read ids only as delivered ones. Delivery receipts can't be turned off, as acknowledgements can't. Neither is sent for messages from someone we haven't accepted or have blocked.

## Deleting and editing messages

Texts and files carry the sender's message id (its history entry's local id). The receiver records it with the entry (history key 11).

`Delete` asks the receiver to delete messages by those ids. It's only sent to peers whose `Hello` has feature bit 2. Receivers honour it in two cases:

- **From a peer:** only for messages that peer's account sent ("delete for everyone"). Anything else is ignored.
- **From one of our own devices:** for any messages in the named conversation. Our devices delete together, whether for me or for everyone. A sibling's chat with *us* is mapped to our chat with it.

`Edit` replaces a message's text, under the same rules and with feature bit 4. The entry keeps its place and is marked edited (history key 12, the time of the edit). Only text can be edited, not files. Both messages are tracked like user content, so they're resent if lost. They go to every device concerned, connected or not: one that isn't connected (a contact's device or one of ours) gets the change when its next session with us starts, ahead of anything else, and also sealed for its mailboxes (Appendix H) when we hold a bundle for it, so it arrives even if we don't meet first. Taking a change twice does nothing more. These changes carry no version, so a mailbox copy opened after the session copy (and perhaps after newer changes) could undo them: a receiver remembers a digest of each `Delete`, `Edit`, `React` and `Pin` it takes, from a session or a mailbox, among the delivered ids for that peer, and drops a mailbox copy whose digest it has seen. A change repeated exactly (pinned, unpinned and pinned again) is then dropped from the mailbox too and arrives with the session copy; lists carry their own stamps and need none of this. A change held for a device we haven't heard from since starting is dropped, not sent, if that device's `Hello` then lacks the feature bit. `React`, `Pin` and `List` travel the same way, and a receiver opening any of these from a mailbox applies it as if it had come over a session. Deletion is a request: a modified client can keep a copy, and so can a screenshot. Group messages carry no ids yet, so in groups only "delete for me" exists, on that device.

### Reactions

`React` adds or removes one emoji on a message, named by the same id as `Edit` and `Delete` use: the message's local id on the side that sent it, recorded as the remote id (history key 11) on the other. Anyone in the conversation may react to any message, with several different emoji each. Each history entry keeps the set of reactions (key 13: `[* [reactor (32), emoji]]`). The reactor is the account (or, until it's known, the device), so a person's devices count as one. A person may put at most 16 emoji on a message, and a message holds at most 256. An emoji is at most 32 bytes, with no control characters. Like `Edit`, a reaction goes to the peer's devices and to our own (with the conversation id), as tracked messages, only to devices whose `Hello` has feature bit 16. In groups, reactions travel inside MLS (Appendix F).

### Replies

A reply quotes an earlier message, as `Text` keys 10 to 12: the quoted message's id (as `Delete` names it), the device that sent it, and at most 200 characters of its text (a file's name, if it has no text). The snippet travels with the reply, so it reads even where the original doesn't exist: deleted, expired, or sent before a group member joined. A message from before ids existed is quoted with id 0, and matched by sender and text instead. A quote without a sender is ignored. Older receivers ignore the keys and see the reply alone, so no feature bit is needed.

Receivers store the quote with the entry (history key 15: `[id, device (32), text]`). A quote must not outlive what it quotes. When a message is deleted (for me or for everyone) or disappears, the text of every quote of it is blanked, and apps show "Original message not found". In groups, replies travel inside MLS (Appendix F).

### Link previews

A sender may attach a card for a link in a text's body (key 13): the page's title, description and site name, and a small JPEG, as the sender's device found them. Receivers never fetch anything, so a link sent to them tells its site nothing about them. The sender fetches only when its user asks (apps offer it for the first link being written), from that device directly, so the site sees the sender's address. Its app shrinks the page's picture to a JPEG of at most 24 KiB (at most 400 pixels on its longer side), and the sender removes its metadata before sending.

A card stands only for a link the message has: its URL, normalized as links in text are (an `http` or `https` URL with a host, lowercase scheme and host, international names in punycode), must equal one of the links in the body. Texts are cut to their limits, put on one line, and lose control characters and bidirectional overrides; a picture that isn't a JPEG within 24 KiB is dropped, and a card left with no title, description or picture is no card. Receivers apply the same rules, and drop a card that fails them or doesn't decode, keeping the message. Opening a card's link asks first, as opening any link does, and names the host it goes to. The card is stored with the entry (history key 21, the encoded `LinkPreview`) and travels in transcripts to our other devices with it. Older receivers ignore key 13 and show the text alone, so no feature bit is needed. In groups the card travels inside MLS (Appendix F, `Content` key 14).

### Forwarding and drafts

A forwarded text or file carries flag 8 in key 6 (`FORWARDED`; for files alongside the other file flags). Receivers record it (history key 18 for a text, file key 6 for a file) and show the message as forwarded. Older receivers ignore the flag. Nothing says where it came from.

A conversation may keep a draft, what the user started writing and hasn't sent (history key 5). Drafts stay on the device, encrypted with the rest of the history; they're never sent.

### Scheduled messages

A message scheduled to be sent later ("send later") needs nothing on the wire. When it is due, the node sends it as an ordinary `Text` (in a group, an ordinary MLS text, Appendix F), with `sent_ms` the time it actually goes and the conversation's disappearing timer as it is then, not when the message was scheduled. Recipients, relays and mailboxes can't tell it was scheduled.

- **Device-local.** The schedule stays on the device it was made on (the `scheduled` state file below). It is never synced to our other devices, which see the message only once it is sent, like any message we send, and it is never backed up (see *Backups*).
- **Jitter.** When an item is added or moved, the node draws a delay of up to 30 seconds and at most a tenth of the time left (`fire = due + uniform(0 ..= min(30 s, lead / 10))`). It never fires early, and sends don't land on exact minute marks. "Send now" has no jitter. Apps show the time the user chose.
- **Late.** An item whose time passed while the node wasn't running goes out as soon as it runs again, unless that is more than 24 hours late. Then it is not sent: it is marked missed, and the user decides.
- **Sent once.** The item's id is the sent message's local id (history key 7), and with it the id the message carries. Before sending, the node looks for an outgoing history entry with that id, so a crash between sending and saving can't send it twice. An item being sent is never saved as such.
- **Settings first.** A node sends nothing scheduled until the app says it has applied the user's settings (`start_scheduled_sending`), so an overdue message sent at startup still gets the user's default disappearing timer and cover traffic. A node opened by the CLI never sends scheduled messages.
- **Routing.** An item to a contact keeps the device it was scheduled to and the conversation that device was in. If that device has since been removed from its account, another live device of the same conversation (a connected one first) gets it, and the message goes to every device of the account as usual. If the contact was blocked, or removed from our contacts here or on another of our devices, the item fails instead, and the contact is not added back. Blocking a contact, deleting its conversation or its request, or leaving a group drops that chat's items. An item already being sent then is checked again right before it goes and isn't sent; one that went first is deleted with the chat. Being removed from a group fails them when due.
- **This device removed** from our account fails every pending item: contacts no longer take it as us.

Limits: 256 items, 64 KiB of text each and 1 MiB in all, at most 366 days ahead.

### Pinned messages

`Pin` (`{ 0 => 27, 3 => conversation, 5 => message id, ? 6 => 1 }`, key 6 present to unpin) pins a message in a conversation for everyone in it, named by the same id as `React`. It travels like `React`: to the peer's devices and our own, tracked, only to devices whose `Hello` has feature bit 2048. Either side of a 1:1 chat may pin. A conversation holds at most three pins (history key 3, oldest first); pinning a fourth unpins the oldest. A deleted or expired message loses its pin. In groups, pins travel inside MLS, and only the owner's and admins' count (Appendix F).

### Lists

A list is a checklist that merges rather than being replaced, since everyone who has it may change it at once: `{ 0: id (16), 1: title, 2: [ms, author (32)], 3: [* item] }`, an item being `[id, text, flags (1 done, 2 heading, 4 deleted), indent (0-3), position (an IEEE 754 double, as its 64 bits), ms, author (32)]`. Each item, and the title, is a register: the larger `[ms, author]` wins. A receiver leaves out of a copy whatever is stamped more than a day ahead of its own clock (such items, and the title's stamp, which then loses to any other), so a forged stamp can't hold a register for good; and a device that changes a register stamped ahead of its clock stamps its change one millisecond after it. A deleted item stays as a tombstone so its deletion spreads. Items sort by position, then id; an item inserted between two takes the mean of their positions. The author is the account (or a device without one), so a person's devices count as one. At most 1000 items (tombstones included), 500 characters each. A device adds at most 900 live items itself, so a full copy keeps some tombstones; to make room for a new item, live or merged in, it forgets its oldest tombstone (the smallest `[ms, author]`, then id, so every copy picks the same one). A full copy takes no tombstone of an item it never had, and no new item once every item it holds is live. A forgotten deletion no longer spreads, so a copy that never heard of it can bring that item back.

A list shared with a contact goes to their devices as `AppMessage::List` (`{ 0 => 28, 2 => list }`, tracked, to devices whose `Hello` has bit 4096); one shared with a group goes inside MLS (Appendix F). Receivers merge it if it's shared with that conversation; a list new to them is kept as shared with it, and a history entry there records it (history key 19: the list id). A device that merges something new passes the list on to the other conversations it's shared with, so everyone converges; a merge that changes nothing stops there. Our own devices exchange lists with whom each is shared (Appendix J).

A device keeps at most 200 lists, and what others send only within limits, so that no contact or group can crowd out the rest or make the stored lists too large to save: lists started by others fill at most 150 of the 200, and at most 50 come from any one conversation (counting those shared with it); lists take at most 12 MiB stored after a change from others, and those shared with one conversation at most 3 MiB. A new list or a change past these is ignored. A change of our own that would make the stored lists too large to save is refused, as is starting a 201st list. Blocking a contact stops sharing every list with them, on all our devices, and no list goes to a blocked contact's devices.

### Photos and files

A file can carry a caption (key 7), a sensitive flag (key 6, bit 1) and an album id (key 8). Files sent together, such as several photos, are sent as separate `File` messages with the same random album id, and the caption travels with the first. Receivers show an album as one message. A sensitive file is shown covered, and is not even decoded until the user opens it. The flag is the sender's request; a receiver can ignore it.

A *voice message* or *video message* is a `File` recorded in the app, flagged with bit 2 (voice) or bit 4 (video) of key 6, never both; key 9 carries how long it plays, in milliseconds, as the sender measured it (at most 10 minutes; absent or 0 if unknown). Receivers play it in the conversation instead of offering it as a download, and keep it privately as they keep photos. A receiver that doesn't know the bits sees an ordinary file, since unknown flag bits and keys are ignored. The reference apps record voice as AAC in MPEG-4 (Android) or Opus in Ogg (Linux), and video as H.264 with AAC in MPEG-4 (Android) or VP8 with Opus in WebM (Linux); each plays all four. Video messages are limited to a minute so they fit in one file.

Before a file is sent, the node removes identifying metadata from JPEG, PNG and WebP images (`threnody_core::media`). This covers EXIF (location, camera and serial numbers, times), XMP, IPTC, comments, text chunks, and data after the image such as a motion photo's video. The pixels are not re-encoded. Decoders still get what they need: colour profiles, transparency, animation, and a JPEG's orientation, rewritten as a minimal EXIF block. A damaged image of those types is refused rather than sent with its metadata. Photo formats whose metadata it can't remove, HEIC/HEIF, AVIF and TIFF (which camera raw files such as DNG are built on), are recognised by their first bytes whatever the file is called, and refused too (`media::needs_converting`). Apps convert them to JPEG before sending, deciding by content as well as by name; the CLI, which can't, asks for a JPEG. This is on by default; `--keep-metadata` or the app's toggle turns it off.

## Padding (spec §9, layer 1)

Before encryption, an `AppMessage` is padded ISO/IEC 7816-4 style: a `0x80` byte, then zeros. The total length is rounded up as follows:

- to 256 bytes if smaller,
- otherwise to the next power of two, up to 64 KiB,
- beyond that, to the next multiple of 64 KiB.

On a constant-rate link (cover traffic on, spec §9 layer 1), every message the link sends on its ticks pads to at least 4096 bytes, the bucket an onion cell (`Onion`, a 2048-byte cell and its circuit header) already fills. So do the session's setup messages (`Hello`, `Approval`, `Paths`, `TunnelOffer`, sent at once in its first seconds), its `Paths` updates, and its revocation on the way out. This holds on contact sessions and on anonymous links (Appendix P). What pads to exactly 4096 bytes, and so goes out at one size (6515 bytes with the length prefix), is: cover, onion cells, and any other message that would pad to 4096 bytes or less, such as a text of up to about 4 KB or a relayed frame (Appendix G) whose inner frame carries cover or a message that pads to 1 KiB or less. An observer then can't tell ticks that carry those from idle ones. Before this, cover went in 256-byte buckets (a 2675-byte frame) and cells in 4096-byte ones, and a link showed when it carried a circuit.

Anything larger keeps its own bucket from the list above (8 KiB, 16 KiB, and so on), so a long text, a file, or a relayed frame carrying a larger inner one still shows its rough size. Urgent messages (call signalling and media, `Tunnel` packets) skip the ticks and keep the buckets above: their timing shows anyway. Sessions without cover traffic, and sessions inside relay or onion circuits, also keep the buckets above; the links that carry a circuit pad its cells.

The receiver strips padding of any length, so this needs nothing from the peer: older nodes read these frames, and theirs are read as before. But an older node still pads what it sends to the buckets above: its cover is 2675 bytes on the wire and its cells 6515, so its direction of the link still shows when it carries a circuit. Each direction of a link is uniform only when its sender pads this way, and the whole link only when both ends do.

## Stream framing (transport)

On stream transports, each frame is prefixed with its length as a `u32` in big-endian byte order. The maximum is 16 MiB plus 4 KiB. Until the handshake has authenticated the peer, the maximum is 16 KiB: handshake messages and the anonymous-link marker are far smaller, and a receiver refuses a longer length prefix without reading the frame.

Authentication does not make a peer trusted: anyone can make an identity, and nodes accept strangers by default. Frames from a peer that is not mutually approved, and every frame on an anonymous link, are read a piece at a time, so memory follows the bytes that have arrived rather than the length prefix. All such frames read but not yet handled share one budget per node, four maximal frames. A frame that would exceed it is refused and its session ends; the sender can try again on a later session. Such a frame must also keep arriving: one that stalls for 120 seconds ends the session. Mutually approved contacts are not limited by either rule. These are receiver rules only; nothing changes on the wire.

## Local storage

All local state other than the identity file is kept in `<name>.state` files. Each one is encrypted with ChaCha20-Poly1305 under `KDF("state encryption key", identity_seed)`, with the file name as associated data (`Home::save_state`). This covers the contact book (`contacts.state`, a `ContactsFile` below), groups, prekeys, bundles, mailboxes, accounts, acknowledgement state and message history (one `hist-<name>` file per conversation, named as below). Message history keeps at most 10,000 entries per conversation. A conversation's timer (key 1) is absent until chosen, in which case it follows the node's default (off unless the user chooses one). 0 means *off* on purpose, and any other value is seconds. A peer that turns its timer off on purpose sends messages without `expires_in_s` and with flag 16 in `Text` key 6 (`TIMER_OFF`), and the other side records that as 0. A message with neither, from a peer whose `Hello` has bit 65536, means it never chose a timer: the receiver keeps its own and applies it to that message too, and the peer adopts it from the receiver's next message. The bits of each contact's latest `Hello` are saved with the contact (key 20), so this holds for messages that arrive before any session, such as mailbox deliveries after a restart. Without that bit (an older peer, or one never heard from directly) it is taken as off, as older peers intend. Older receivers ignore the flag and read no timer, as before it. A file transfer is stored as an entry whose text is its caption (usually empty) and a file record (key 6: `{ 0 => name, 1 => size, ? 2 => location, ? 3 => sensitive, ? 4 => album, ? 5 => [video bool, duration_ms uint] }`, key 5 marking a voice or video message). Outgoing entries may have a local id (key 7) and a delivered flag (key 8, see *Acknowledgements*). The record keeps where the app saved the file, not the file's contents. Older readers skip the key. Disappearing messages are deleted on the first load or save after they expire, and by a sweep that runs every minute. The same sweep finds disappearing texts that expired here while still waiting for their recipient's acknowledgement (`unacked`), and replaces each with a copy sealed to the recipient (Appendix H), which goes out as a mailbox `deliver` from the sender itself. The recipient still gets it, and its timer still starts there, but the sender keeps nothing it could read back. Without prekeys for the recipient the waiting copy is dropped.

History key 14 on an outgoing entry is when the recipient displayed it (see *Typing and read receipts*). On an incoming entry it is 1 once we have reported it read, so each message is reported once. It is a flag, not a time, so the history doesn't show when the user read each message. Receipts that still have to reach their sender's device are kept in `receipts-out`, with the read-receipt switch and when that state was first created (Appendix H, *Receipts*):

```
receipts-out = 1 || flags (1: read receipts off) || created_ms (u64 BE)
               || n (u16 BE) || n × (device (32) || last sealed receipt ms (u64 BE))
               || *(device (32) || batch since ms (u64 BE) || due ms (u64 BE)
                    || nd (u16 BE) || nr (u16 BE) || nd × delivered id (u64 BE) || nr × read id (u64 BE))
```

A damaged tail is skipped; an unreadable file is started afresh, with read receipts on.

History key 20 on an outgoing entry is when this device last learned something new about its ticks (`ticked_ms`; absent = never). On an incoming file entry it is when this device received that file from the contact itself, which marks the records it passes on to its siblings (absent = merged from a sibling, or recorded before this). It is local: transcripts carry none, and a receiver drops any it is sent. A device keeps how far each sibling has its ticks in `history-sync-ticks`, and how far it has the records of files contacts sent this device in `history-sync-files`, in the same layout as `history-sync` (`1 || *(device (32) || until ms (u64 BE))`). Its queue of file contents to fetch from its siblings is `file-fetches` (`1 || cbor [* { 0: conversation, 1: bstr .cbor FileRef, ? 2: true (automatic) }]`, at most 64; references only, never contents). See Appendix J, *History* (*Received files*), *Files* and *Ticks*. Older readers skip key 20 and ignore these files.

A history file's name is keyed, so a listing of the data directory shows neither whom the user talks to nor which groups they are in:

```
name_key     = KDF("history file name key", identity_seed)
name         = "hist-" || hex(KDF("history file name", name_key, conversation))   ; 64 lowercase hex digits
conversation = 0 || account id (32)      ; a contact's account, or its device key while no account is known
             / 1 || group id (16)
```

The name doesn't say which conversation a file holds, so the file does: top-level key 7 of the stored history is `conversation` as a byte string. Listing the conversations reads each `hist-` file. Older readers skip the key. Each identity has its own `name_key`, so the same conversation has different names in the main home and in a persona's.

Versions before 0.18.0 named history files after the conversation: `hist-p-<hex account or device>` and `hist-g-<hex group>`. The account id maps one to one to the account fingerprint, so those names showed whom the user talks to. When a home is first opened (its contact book loaded), and whenever a conversation still under its old name is read, each such file is decrypted, encrypted again under its keyed name (the name is the associated data) and written atomically, and only then is the old file deleted. An interrupted move leaves both files, and the next run finishes it. If a file under the keyed name is already there, the two are merged first: entries it doesn't have are added (matched by message id, or by sender and text for entries too old to have one, as own-device sync matches them), and the timer, draft and notification setting are taken from the file under the old name where it has them set (a timer, including 0 for *off*; a draft; anything other than *all*), since only an older version run after the upgrade writes that name and it is the newer. Otherwise the keyed file's stay.

Versions before the contact book was encrypted kept it unencrypted in `contacts.cbor`. A node that finds that file and no `contacts.state` reads it, stores it encrypted and overwrites and deletes the old file. If both are there, an older version ran after the upgrade and wrote the plain one, and `contacts.state` wasn't written meanwhile, so the plain one is the newer. It is merged into the encrypted one (`Contacts::merge_older`), the conversations it says to delete are deleted, then the merged book is stored encrypted and the plain file deleted. Contacts only the plain file has are added as they are. For a contact in both:

- **A block** in the plain file wins, whatever its `approval changed` time (key 10), since 0.17.3 doesn't date blocks. It covers every device of the contact's account and is dated when merged, and the conversation's history is deleted, as when 0.18.0 blocks.
- **An approval or revocation** dated later than the encrypted book's wins, with acceptance. Approving again keeps the encrypted book's verification (the key is the same); revoking clears it. A device the older version first met through its contact's account is dated within a second of that, unapproved and unaccepted: that is no revocation, and the encrypted book's approval stays. A verification made there, which 0.17.3 doesn't date either, is kept.
- **A name** given there wins, except a `my <device>` name, which the older version gives our own devices by itself, where the encrypted book has one.
- **The later sighting** (key 7) brings its address, the peer's approval and its feature bits. Anything else missing from the encrypted book is filled in from the plain one.

Deletions recorded in the plain file (key 2) are kept, and the deleted contacts' conversations are deleted too (0.17.3 deleted only the files under the old names). Deleting a message request leaves no record there, so a contact from before the downgrade whose returning request was deleted then stays, with its conversation.

**Going back to a version before 0.18.0** after upgrading is not supported, but loses nothing. The older version can't read `contacts.state` or the keyed history names, so while it runs it shows no contacts and no history. Contacts that reach it come back as new, unnamed and unapproved, and it writes what it learns to a fresh `contacts.cbor` and to history under the old names. It never touches the files 0.18.0 wrote. When 0.18.0 next opens the home, both are merged back as described above. Contacts added while downgraded keep their names and approvals. Blocks, deletions, approvals, revocations, verifications and names given then are kept, and verifications and approvals from before are not undone by a contact only being seen again. Messages sent and received then join their conversations, and timers, drafts and notification settings chosen then are kept.

Every file is written with mode 0600 inside a directory with mode 0700.

```cddl
IdentityFile = { 0 => 1, 1 => bstr .size 32 }               ; Ed25519 seed
             / { 0 => 1, 2 => bstr }                         ; or the seed sealed (Argon2id + ChaCha20-Poly1305) under a passphrase, or a random secret the platform keystore keeps
ContactsFile = { 0 => 1, 1 => [* Contact], ? 2 => [* [bstr .size 32, uint]] }  ; contacts, forgotten keys and when (ms)
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
  ? 14 => [* [tstr, tstr]], ; their profile as they shared it (Appendix M)
  ? 15 => [* tstr],      ; our profile attributes shared with them
  ? 16 => bstr .size 32, ; the identity a persona revealed to us (Appendix M)
  ? 17 => tstr,          ; the invite link that came with the reveal
  ? 18 => uint,          ; conversation cleared at (ms)
  ? 19 => bool,          ; their latest Hello had bit 32768 (timed probes, Appendix N); absent = false
  ? 20 => uint,          ; feature bits of their latest Hello (absent = 0)
}
```

Scheduled messages (*Scheduled messages* above) are kept in `scheduled.state`, which is deleted while nothing is scheduled. Readers skip unknown keys and items they can't read; a file that can't be read at all is an empty schedule.

```cddl
ScheduledFile = { 0 => 1, 1 => [* ScheduledItem] }      ; version, items
ScheduledItem = {
  0 => uint,                   ; id (also the sent message's local id, history key 7)
  1 => bstr,                   ; target: 0 || device key (32) || conversation id (32) for a contact, 1 || group id (16)
  2 => uint,                   ; due (ms since the epoch), as the user chose
  3 => uint,                   ; fire (ms): due + jitter
  4 => uint,                   ; created (ms)
  5 => tstr,                   ; text
  ? 6 => [uint, bstr .size 32, tstr],      ; quote: id, author device, snippet (as history key 15)
  ? 7 => [* [uint, uint, bstr .size 32]],  ; mentions: start, len, who (as history key 16)
  ? 8 => uint,                 ; failed: 1 missed, 2 contact, 3 group, 4 error, 5 this device removed (absent = waiting; unknown values read as 4)
}
```

### App lock

The apps can lock their user interface behind an app PIN or passphrase, the device's own authentication, or both (`threnody-core::applock`). App lock is a gate on the screen, not encryption at rest: the node keeps running behind it so messages still arrive, and the identity stays protected as it was (sealed under a keystore-held secret). Its settings live in `lock.cbor` in the data directory, which exists exactly while app lock is on, so the apps can check that synchronously and fail closed before reading it (for example when the background service posts a notification at boot).

The file holds Argon2id verifiers of the secrets, never the secrets, and is encrypted under a key derived from a *pepper* the platform passes in: the Keystore-protected secret on Android, the Secure Enclave-protected one on iOS, and a separate keyring entry (account `"<absolute home> app-lock"`) for keyring-sealed homes on Linux. A copy of the data directory then shows neither whether a duress secret is set nor anything to guess the PIN against offline. Linux homes sealed with a passphrase, or not sealed, have no pepper (`peppered` false, key from the empty string): the file is then protected only by the Argon2id cost, and a short PIN can be guessed offline.

```
key = KDF("app lock file key", pepper or "")
ct  = ChaCha20-Poly1305(key, nonce, ad = "threnody lock", LockBody)
```

```cddl
LockFile = { 0 => 1,                 ; version
             1 => bool,              ; peppered
             2 => bstr .size 12,     ; nonce
             3 => bstr }             ; ct
LockBody = { 0 => 1,
             1 => [uint, uint / null],  ; lock after leaving (s, 0 = at once), lock when idle (s, null = never)
             2 => bool,              ; unlock with biometrics / the device lock
             ? 3 => [uint, PinSlot], ; the lock secret: 0 digits (4 to 16), 1 passphrase (6 or more characters)
             4 => PinSlot,           ; the duress secret, or a random decoy
             5 => bool,              ; duress secret set
             6 => uint,              ; wrong entries in a row
             7 => uint,              ; entries refused before (wall ms), 0 = none
             8 => uint,              ; last wrong entry (wall ms), 0 = none
             ? 9 => bool,            ; erase after 10 wrong entries in a row (absent = false)
             15 => bstr }            ; zero padding: the body is always 512 bytes
PinSlot  = { 0 => bstr .size 16,     ; salt
             1 => bstr .size 32,     ; Argon2id(secret, salt)
             2 => [uint, uint, uint] }  ; m KiB, t, lanes (19 MiB, 2, 1)
```

Without a key 3 the lock is the device's own (biometrics with the device credential as fallback), and key 2 must be true. Every entry on the lock screen is hashed under both slots and compared in constant time, so its timing doesn't show whether the duress secret was entered or whether one is set. The lock secret unlocks and resets the count. The duress secret, when set, erases the data directory (*Erasing*) and the app opens as newly installed. After 5 wrong entries in a row, entries are refused unchecked, duress secret included, for 30 s, doubling with each further wrong entry up to an hour; the refusal is kept in the file, so restarting doesn't end it. A clock set back before the last wrong entry restarts the current delay from the new time instead of keeping the user out for as long as the clock moved. Setting the clock forward skips a running delay; the Argon2id cost still bounds guessing. With key 9 set, the 10th wrong entry in a row erases everything, as the duress secret does. Changing or removing the lock secret, setting the duress secret and turning app lock off need the current secret. Readers skip unknown keys. Older versions ignore the file, so after a downgrade the lock doesn't apply.

Calls ring while the app is locked. While app lock is on, call notifications (ringing, in progress, missed) say only that there is a call, never who; Telecom is only ever given "Threnody call". On Android the call screen opens over the lock without unlocking and, while locked, shows no caller or participant names; answering doesn't unlock, and the app is still locked when the call ends. On Linux the call is answered after unlocking.

### Erasing

The apps' "Erase all Threnody data", the duress secret and `threnody wipe` erase a data directory (`Home::erase`). Nothing is sent: contacts, linked devices and the network aren't told.

1. Every `Home` handle opened on the directory before the erase (a node still shutting down, its groups node, its personas) is revoked. Its writes fail from then on and can't create the directory again. A handle opened later writes normally.
2. The identity files go first, overwritten and then deleted: the main identity, then each persona's. Every other file is encrypted under them, so an erase interrupted after this step leaves nothing readable.
3. Then the names Threnody writes there: `contacts.cbor`, `lock.cbor`, every `*.state` and `*.tmp`, `persona`, `desktop.lock`, `control.sock`, `restore-ready`, and the directories `personas/`, `media/`, `edited/`, `downloads/` and `wireguard/`. Anything else in the directory is kept and reported (a data directory set to the user's home by mistake must not lose their files). The directory itself is removed if it is then empty, and a data directory that is a symbolic link loses the link too.
4. The staged restore beside it (`<home>.restore`) is removed.

Before a new identity is created in a directory that has none, what an interrupted erase left there (contacts, state files, personas, `lock.cbor`) is removed, since it is encrypted under the old identity.

Each platform also removes what it keeps outside the data directory: the keystore key and sealed passphrase (Android Keystore, iOS Secure Enclave and Keychain, the Linux keyring entries for every identity and the app-lock pepper), preferences and the desktop app's `desktop.conf`, caches and received media, the app's own files in `Downloads/Threnody`, posted notifications and their channels, the UnifiedPush registration, and on Android any call in progress and the self-managed calling account registered with Telecom. On Android 13 and later the runtime permissions are revoked so the restarted app asks again; on older versions they stay granted, as does the VPN consent everywhere. On phones, deleting the keystore key is what makes the erase final, since overwriting isn't reliable on flash storage. A passphrase-sealed or unsealed identity on Linux depends on the file system freeing the data.

## Backups

A backup (spec §6.4) holds everything needed to bring an identity back on a new device after the old one is lost. It is sealed under a passphrase the user chooses, at least 12 characters long (`threnody-core::backup`):

```
file   = "threnody backup\n" || Sealed
Sealed = sealed(passphrase, "threnody v1 backup", Backup)   ; Argon2id + ChaCha20-Poly1305, as a passphrase-sealed identity
```

```cddl
Backup = {
  0 => 1,                    ; version
  1 => uint,                 ; made at (ms)
  2 => bstr .size 32,        ; identity seed
  3 => bstr,                 ; ContactsFile, decrypted (the backup itself is sealed)
  4 => [* [tstr, bstr]],     ; state name, state file as stored (still encrypted under the seed); not `contacts`, which is key 3
}
```

Message history goes into key 4 under the names versions before 0.18.0 gave it (`hist-p-<hex>`, `hist-g-<hex>`), encrypted again for those names, so that older versions restore it too. A conversation found under both names on disk goes in once, merged as moving it to its keyed name would. Conversations whose contact an older version blocked or deleted are left out. The backup is sealed as a whole, so the names inside it show nothing to anyone without the passphrase, and the contacts in key 3 already name every contact. Restoring moves the history to keyed names at once. So a backup made by 0.17.3 restores into 0.18.0 with its history, and one made by 0.18.0 restores into 0.17.3 with its contacts and history (0.17.3 then runs as any older version does, see *Going back* above).

A backup leaves out:

- **One-time prekeys,** ours (`prekeys`) and those we hold for contacts (`bundles`). A restored copy would let a sealed message that was already opened be opened again, or seal a second message to a prekey a contact already used. Fresh ones are made and handed out as sessions come up. Messages sealed to the lost device's prekeys can't be opened.
- **Relay tokens** (`relay-wallet`, Appendix P), each of which can be spent once.
- **Anonymous identities** (Appendix M). A backup would link them to the main identity.
- **Scheduled messages** (`scheduled`). A backup restored later must not send old messages, or send them a second time while the old device still runs. Restoring drops any that are pending on the device restored onto.
- **Saved tunnel peers** (`tunnels`, Appendix D, *Persistence*). Their PSKs come from sessions with this device, so they never leave it.
- **Received files' contents.** History keeps only where the app saved them.

**Restoring** replaces the identity, contacts, state and personas in a data directory. The backup is opened and checked first, so a wrong passphrase or a damaged file changes nothing. The restored identity is sealed the way a new one would be (the keyring, a passphrase, or the Android Keystore). Apps can't stop their node while they run, so they *stage* a restore in `<data dir>.restore` and restart. The staged files replace the old ones before the node next opens. The user's other files in the data directory, such as downloads, stay.

A restored device has the lost device's identity, so contacts see it as that device. MLS groups that moved on since the backup need a new invite. Restoring onto a device whose identity is still in use elsewhere makes the two fight over sessions; to use two devices as one, link them (Appendix J).

