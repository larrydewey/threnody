# Appendix J: Accounts and Multiple Devices (v1)

Status: specified here, modelled in [`proofs/devices.spthy`](../proofs/README.md), implemented in `threnody-core::account` and `threnody-net::account`. Fulfils spec §4.5: devices are cryptographically bound to an identity, easy to add and revoke, and the loss of any one device costs nothing.

Every device keeps its own Ed25519 key. Sessions, ratchets, tunnels, prekeys and relays stay per device, exactly as before. An **account** groups devices under one stable identity.

## Account chain

An account is a hash-linked list of signed changes to its device set:

```
Link   = { 0: body bstr, 1: [ * Sig ] }
body   = { 0: version 1, 1: seq u64, 2: prev (32), 3: threshold u8, 4: action }
action = { 0: 1 add,    1: device key (32), 2: name tstr }
       / { 0: 2 remove, 1: device key (32) }
       / { 0: 3 rename, 1: device key (32), 2: name tstr }
Sig    = [ device key (32), signature (64) ]
sig    = Sign(device, L("account link") || L(body))
prev   = BLAKE3(previous link body)     -- 32 zero bytes for the genesis link
AccountId = KDF("account id", body of link 0)
```

### Validity

A chain is valid if, applying links in order:

1. **The genesis link** (seq 0, zero `prev`) adds exactly one device and is signed by that device. Its threshold is the initial threshold.
2. **Every later link** has `seq = previous + 1` and `prev = H(previous body)`.
3. **Authorisation.** Every later link is signed by at least `threshold` distinct devices in the device set *before* the link. The threshold is the one stated in the previous link.
4. **Add** also needs a signature from the device being added (proof of possession and consent), and that device must not already be in the set.
5. **Remove** names a current device and may not leave the set empty.
6. **No re-adding.** A device that has been removed can never be added again.
7. **Rename** names a current device and gives it a new display name (at most 64 characters, no control characters). It changes no membership, so it leaves the properties below untouched. Peers running a version from before renames reject a chain that contains one, so update every device first.

v1 always writes threshold 1, so every current device is an equal peer. Verifiers already enforce thresholds above 1, so raising it later needs no change to the format.

### Account fingerprint

The account fingerprint is computed like a device fingerprint (Appendix A), as `KDF("account fingerprint", AccountId)`. Invite links and safety numbers still name devices: a link carries the inviting device's fingerprint, and a safety number is computed from the two devices' keys. A single-device account is just a chain with one link.

### Updates and forks

- **Exchange.** Peers send their chain as `AppMessage::Account` (kind 11) at the start of every session, and again whenever it changes, but only to their own devices and to peers they accepted (or approved). The chain names every device of the account, often after its owner, so a stranger gets it only once accepted, when it is sent at once. Older peers send it to everyone; nothing depends on receiving it sooner.
- **Accepting a chain.** A receiver accepts a chain only if it is valid, its id matches what the receiver has stored for that account (pinned at first sight), and the session's device is in its current set.
- **Strangers' chains wait.** Anyone can open a session and present a chain, and a kept chain stays, with every device in it added as a contact. So a receiver checks and keeps a chain only if the user accepted the presenting device, or the chain claims to be the receiver's own account or one it already holds (which is how a contact's new device is recognised). It reads the claimed id from the genesis link without verifying anything. Any other chain waits, unchecked, until the user accepts that device in the same session, and is dropped when the session ends; the next session presents it again. Until then a stranger's devices aren't grouped into one account.
- **Updates.** A newer chain must *extend* the stored one. A different chain at or below the stored height is a **fork**: the receiver rejects it and warns, because forking requires a compromised device.
- **Removal.** When a chain removes a device, every peer drops its sessions with that device, revokes its approvals, deletes its discovery key and prekeys, and refuses it from then on. Nothing more is sealed to it, and no group message goes to it by any path. The owner of each group it is in removes it, as for any member (Appendix F, *Removal*), so later epochs are unreadable to it; the device that removed it does the same in groups it owns.

## Linking a device

```
existing device E                                  new device N
  code = threnody-link://<fp(E)>@<addr>#<s>   ──(out of band: QR / typed)──▶
                                              ◀──  session (handshake; N pins fp(E))
                                              ◀──  LinkRequest { proof = KDF("link proof", s, session_id, N), name }
  check proof; build link Add(N, name), signed by E
                                              ──▶ LinkOffer { chain, link }
                                                   N checks the link and countersigns it
                                              ◀──  LinkConsent { link }
  append the link; broadcast the new chain
```

`s` travels in the code as 32 hex digits after `#`. Account messages (`AppMessage::Account`) are numbered: 1 chain, 2 `LinkRequest`, 3 `LinkOffer`, 4 `LinkConsent`, 5 `ContactSync`, 6 `LinkRefused`, 7 shared bundle, 8 `Transcript`, 9 `Folders`, 10 `List`, 11 `FileWant`, 12 `FilePart`, 13 `FileMissing`, 14 `Ticks` (11–14 only to siblings whose `Hello` has the device-sync bit, Appendix C).

- **The secret.** `s` is 16 random bytes, good for one use and 10 minutes.
- **Authenticating each side.** The code pins `fp(E)`, which authenticates E to N. The proof binds `s` to *this* session and *this* device, which authenticates N to E. An attacker without the code can't be added, and can't replay a proof into another session.
- **Signing.** E sends the current chain and a proposed `Add(N)` link that E has already signed (`LinkOffer`). N checks that the proposal is exactly the next link adding N on top of that chain, then countersigns it (`LinkConsent`).
- **Refusal.** A wrong or expired code gets an explicit `LinkRefused`, so N fails at once instead of waiting.
- **Aftermath.** E then broadcasts the new chain to every session with a peer that may see it (see *Exchange*). Every device of the account tells its user that a device joined: E when the link completes, the others when the new chain reaches them. Anyone holding a leaked code can link a device without asking, so the user learns of it on whichever device they are using and can remove it.

Apps name a device on creation: the CLI uses the host name, and the Android app uses the phone's model (it renames a device still called "device"). Only our own devices and accepted contacts see these names. `/device rename <device> <name>` and the app's *Devices* screen rename any device of the account.

## Own devices

- Devices in the same account approve each other automatically.
- They exchange `ContactSync` snapshots at the start of each session and after changes. Approval, verification and block flags merge last-writer-wins on `approval_changed_ms`, and petnames and addresses fill in where missing. Blocking moves `approval_changed_ms` like approving does, so approving a blocked contact again unblocks it on every device (only approving unblocks); at equal times a block wins, because older devices block without moving the time.
- They share message history (below).

## History

Contacts send to every device of an account, so each device receives incoming messages itself. What a device *sends*, its siblings would never see. So devices exchange `Transcript` account messages (`{ 0 => 8, 1 => conversation, 2 => history }`). `conversation` is `0 ‖ account or device id (32)` for a 1:1 chat and `1 ‖ group id (16)` for a group, and `history` uses the history encoding (Appendix C, *Local storage*) for some of that conversation's entries.

- **Live.** An outgoing 1:1 entry goes at once to every connected sibling.
- **Catching up.** When a session with a sibling starts, a device sends its outgoing entries newer than what that sibling last had from it. Each device keeps that position per sibling, encrypted (`history-sync`). Deletions, edits, reactions and pins made while a sibling was away aren't in transcripts; they wait for it as tracked messages (Appendix C, *Deleting and editing messages*).
- **New devices.** The first time, a device sends all of its history, 1:1 and group, so a newly linked device starts complete. Linking triggers this right away, without waiting for a new session.
- **Merging.** Receivers merge entries that aren't already present, and accept transcripts only from devices of their own account. Entries match by the sender's id for the message where both have one (an outgoing entry's local id, or an incoming one's remote id). Others match by sender and content within ten minutes, because each device stamps its own receipt time.
- **Reliability.** Transcripts are sent tracked (Appendix C, *Acknowledgements*), so one lost with a dying session is resent.
- **Files.** A file entry travels without its location (a path or URI on the device that recorded it). Receivers drop any location a transcript carries anyway. The contents come on request (*Files*, below).
- **Groups.** A device can only read a group's messages from when it joined (MLS), so its siblings transcribe the group's history to it, and have it added to the group (Appendix F, *Linked devices*). When catching up, a device sends every group entry newer than what that sibling last had, not only its own, to cover the time between linking and joining. Once a member, a device receives the group's messages itself; those a sibling sends reach it as a member and are recorded as the account's own.
- **Notes to self.** Each account has a chat with itself, `0 ‖ account id`. A device writes notes there like outgoing messages, and they reach the other devices as transcripts (live, catching up, and on linking). A file note stays on the device that has it; the others get its record.
- **Folders.** The user's chat folders, pinned chats and archived chats are one record (Appendix C, *Local storage*: `{ 0: version 1, 1: updated_ms, 2: [* folder], 3: [* pinned conversation], 4: [* archived conversation] }`). Each change stamps the whole record with its time (never earlier than the last stamp) and sends it to connected siblings as account message `Folders` (`{ 0 => 9, 1 => record }`); a sibling sends it as a session starts and when a device is linked. The newest stamp wins. A folder holds its picked conversations, plus, by rule, all contacts, all groups or verified contacts; "unread" and "mentions" rules keep only chats with unread messages, or unread mentions of us. A folder may set when its chats notify, and quiet slots (days of the week and a local time span, possibly overnight). A quiet folder still lets a message through when one person has written to us directly (1:1, or mentioning or replying to us) a set number of times within a set number of minutes. A chat set to "off" itself stays off. A new incoming message takes a chat out of the archive. Unread counts and quiet hours are worked out on each device.
- **Lists.** Our devices send each other every list as it changes, and all of them as a session starts or a device is linked: account message `List` (`{ 0 => 10, 1 => { 0: list, 1: [* conversation it's shared with], ? 2: true (deleted), ? 3: ms } }`), sent tracked. Key 3 is when the device last changed whom the list is shared with (never earlier than the last such time; 0 if not known). Receivers merge the list (Appendix C, *Lists*) and take the sender's sharing if its key 3 is later than theirs, or equal with a sharing that sorts larger (comparing the conversations' bytes in order), so every device ends with the latest; a sender without key 3 (an older version) has its sharing taken as before. A deleted list goes on every device, while those it was shared with keep theirs. Each device remembers the last 200 lists it deleted; when a device that was away offers one of them, it answers with the deletion rather than taking the list back. A list deleted and then shared with us again by a contact counts as new.
- **Conversation ids.** A device that has a contact from contact sync, but hasn't seen the contact's account chain, files the chat under the account recorded in its contact book. Its siblings do the same, so transcripts land in the right conversation.

Apps show a sibling's messages as yours, marked "from your other device", with the ticks that device passed on (*Ticks*, below). They never show a lone "sent" tick for one: only the sending device knows that.

## Files

A device that lacks a file's contents pulls them from a sibling that has them. All of it travels in account messages between our own devices, and only to siblings whose `Hello` has `FEATURE_DEVICE_SYNC` (Appendix C).

```
op 11 FileWant    { 1: conversation, 2: bstr .cbor Want }      requester -> own device
op 12 FilePart    { 2: bstr .cbor Part }                       own device -> requester
op 13 FileMissing { 2: bstr .cbor Missing }                    own device -> requester

FileRef = { 0: author device (32), 1: message id uint (0 = none), 2: name tstr (≤ 255 bytes), 3: size uint, 4: at_ms uint }
Want    = { 0: request uint, 1: FileRef, 2: offset uint, ? 3: content hash (32) }
Part    = { 0: request uint, 1: offset uint, 2: total uint (≤ 8 MiB), 3: content hash (32), 4: data bstr (≤ 262144) }
Missing = { 0: request uint }
content hash = BLAKE3(the whole file as sent)
```

- **Naming a file.** A `FileRef` names a history entry the way both devices recorded it: by its author device and the sender's id for the message, or, for a message without one, by name, size and a time within ten minutes.
- **Windows.** The source answers a `Want` with up to 4 `Part`s from `offset`, or with `Missing`. A zero-length file is one empty `Part`. After the last part of a window the requester asks for the next, under the same random `request` id. It takes a fresh id when it changes source or starts again.
- **Integrity.** Parts are taken only from the device asked, under its request id, in order, with `total` equal to the recorded size. The requester checks the hash over the whole file before keeping it. A hash that changes partway (the file changed at the source) starts the transfer again, so a file is never stitched from two versions.
- **Losses.** `Want`, `Part` and `Missing` are plain sends. A transfer that hears nothing for 60 s, or whose source goes away, asks again from where it got to: of another sibling, or of the same one when it is back.
- **Who is asked.** The author device first, if it is a connected sibling; else the longest-standing session. A sibling that says `Missing` isn't asked again for that file. The file counts as unavailable only once every other device of the account has said so. A sibling that can't serve (an older version, or the CLI, which keeps no files) doesn't fail a fetch.
- **What a source serves.** A source looks the `FileRef` up in its *own* history and reads only a location it recorded itself, never one named in a message, and at most 32 MiB. It serves the metadata-stripped form of an image if that has the recorded size, else the raw bytes if they have, else `Missing`. So a sender's original never leaves with metadata the sent copy didn't carry. Two contents are kept in memory for 2 minutes for the windows still to come.
- **Automatic downloads.** New photos and voice and video messages in synced history come over by themselves, on by default: only those neither sensitive nor disappearing, at most 30 days old, on unmetered networks, and only from a sibling reached directly (not through a relay, an onion circuit or Bluetooth). Sensitive and disappearing ones never do. Everything else comes when the user asks. Apps turn automatic downloads off on metered networks. The conditions hold for every window, not only the first: an automatic download under way pauses, keeping what it has, when automatic downloads are turned off, the device moves to mobile data, or its sibling is no longer reached directly. It goes on from there once they hold again.
- **The queue.** A device keeps at most 64 queued fetches (2 at a time), newest first; automatic ones make room for the user's. The queue survives restarts (state `file-fetches`, references only, never bytes). Fetches for entries since deleted, expired or cleared are dropped.
- **Keeping.** Apps keep fetched files privately (never in Downloads), and delete them when their message goes, as they do received photos.

## Ticks

Only the sending device hears a contact's acknowledgements and receipts. So each device passes on what it learns of an outgoing entry's ticks to its siblings with the device-sync bit:

```
op 14 Ticks { 1: conversation, 2: bstr .cbor Ticks }
Ticks = { 0: 1, 1: [* Tick] }                                   ; at most 128
Tick  = { 0: message id uint, 1: author device (32), ? 2: true (delivered),
          ? 3: read_ms uint, ? 4: recipients uint, ? 5: bstr (n x 32 delivered devices, n ≤ 1024) }
```

- **Live.** A change goes to connected siblings within a second, batched per conversation (plain sends).
- **Catching up.** Each entry records when this device last learned something new about its ticks (`ticked_ms`, local). When a session with a sibling starts, a device sends every tick that changed since it last caught that sibling up (state `history-sync-ticks`), tracked. A first full transcript already carries every tick, so it sets that position: a receiver merges the ticks of every transcript entry, also of those it had already (from a third device, say). A sibling linked before ticks were synced gets every tick once.
- **Merging.** Receivers merge ticks into their outgoing entries, only ever forward: delivered stays delivered, a read time once set stays (and is never in the future), delivered group devices only grow, and a group message is delivered once all its recipients have it. A device never forwards ticks it was told; its siblings get the change at their next catch-up from it. Ticks for an entry not here yet wait in memory (at most 4096) until a transcript brings it.
- **Notes to self** have no ticks.
- Unread counts of incoming messages stay per device.

## Contacts and messages

- **Grouping.** A contact entry is still per device, and records the account the device belongs to.
- **Approval.** Approving an account approves all of its devices. Devices that appear later inherit the account's approval.
- **Sending.** A message to an account goes to every device: over live sessions where possible, otherwise sealed (Appendix H) for each device with a prekey bundle.
- **Display.** Incoming messages show the account, with the sending device's name.

## Offline keys across an account

A contact may never have met some of an account's devices. To let it seal messages to them anyway (Appendix H), the account's devices share keys with each other:

- **Shared bundles.** Each device gives its siblings a *shared bundle*. This is its signed prekey without one-time prekeys, because those are reserved per contact.
- **Forwarding.** Siblings forward these bundles alongside their own prekeys (`AppMessage::Prekeys`) to their mutually approved contacts.
- **Acceptance.** A contact accepts a forwarded bundle for device D from peer P only if all of these hold:
  - P is mutually approved;
  - P and D are in the same account, according to the chain P presented;
  - D hasn't been revoked;
  - the bundle's signature by D verifies.
- **No downgrades.** A forwarded bundle never replaces a direct bundle from D that still has one-time prekeys.
- **Forward secrecy cost.** Messages sealed to a forwarded bundle use the signed prekey only, so their forward secrecy begins when that prekey retires (Appendix H, SPK-only mode).

A sibling that is online, and mutually approved with the sender, also acts as a mailbox for the absent device.

## Groups

MLS leaves stay per device. `/group invite` sends a key-package request to every device in the contact's account; each device accepts on its own.

## Properties (proved in `proofs/devices.spthy`)

- **Chain authority.** Every device in an account's current set was added by a link signed by a device already in the set (and by itself), unless one of those signing devices was compromised. An attacker who controls no device can't add one.
- **Link codes.** A device is added through the link protocol only if it is the device that received the code over the out-of-band channel. A code captured off the network is useless without `s`, and a proof is bound to one session.

## Not yet done

- Gathering co-signatures for thresholds above 1, so removals need several devices.
