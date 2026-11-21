# Appendix U: Large files (draft)

## Status

Draft. Phase 1 is under way. The codecs and piece cryptography are in `threnody_core::transfer`, and the node offers, serves and downloads files (`threnody_net::transfer`, see *Phase 1 as built*), but only once an app installs a large-file store, and no app does yet. Until then nothing changes on the wire. Today a file travels whole inside one `File` message (Appendix C), so it is capped at 8 MiB (`message::MAX_FILE`). Every receiver rejects anything larger as malformed. That number is load-bearing: the 16 MiB encoder cap (`cbor::MAX_ENCODED`) and transport frame, the sender's 32 MiB of tracked messages per peer, group holds (32 MiB per member), device sync (`Part.total ≤ 8 MiB`) and whole-buffer metadata stripping (`media::strip`) are all sized around it. Apps also read the whole file into memory before sending (Android's `readBytes`, `send_file(data: Vec<u8>)`).

This appendix replaces the cap for files over 8 MiB with an **offer** plus **pieces pulled by the receiver**. The only practical limit is then the receiver's free space, which the receiver checks itself and never reveals. The piece format is built so that a file can later come from several sources at once (a swarm) among parties who already hold it: the sender's devices, the receiver's own devices, and group members.

## Goals

- No fixed size limit other than a sanity bound in the decoders (see *Open questions*).
- The sender learns nothing about the receiver's storage. There is no "not enough space" reply.
- Memory use stays flat: files are read, encrypted, hashed, sealed and written a piece at a time.
- Transfers resume after a dropped session, a restart or a change of network.
- No third party (relay, volunteer relay, DHT, public swarm) ever stores file contents. Relays carry session ciphertext exactly as they do now.
- Two sends of the same file cannot be linked by anything they carry.
- Older peers keep working: files of at most 8 MiB still go inline to peers without the new feature bit.

## Overview

1. **Prepare.** The sender strips metadata into a temporary file (streaming; see *Metadata*), picks a fresh random 32-byte file key `K`, and encrypts the file in pieces under `K`. It hashes each encrypted piece and hashes the list of piece hashes into a `root`. Only `K`, `root` and where the file lives are kept, in the history entry. Pieces are produced again on demand, and encryption is deterministic given `K` and the piece index, so they come out identical every time.
2. **Offer.** The sender sends an `Offer` (name, size, `root`, `K`, piece size) in place of a `File`. It is small, so it travels like a text: tracked, acknowledged, and sealed into mailboxes when the receiver is offline (Appendix H). Inline files never use mailboxes.
3. **Decide.** The receiver applies its own policy: free space minus a margin, metered network, sensitive or disappearing, sender a contact or a message request, and the user's auto-download size. It then pulls the file at once or shows a **Download (size)** button. Declining is silent: the offer just stays undownloaded. If space is short the receiver's app says so and offers to try again.
4. **Pull.** The receiver fetches the piece-hash list, checks it against `root`, then fetches pieces in windows, checking each against its hash, decrypting it under `K` and sealing it to disk (`media_seal` streaming). Pieces come from any source that holds them (see *Sources*).
5. **Finish.** With every piece in, the receiver sends `Have { root }`. The sender's app shows a "downloaded" tick. A sender stops serving a 1:1 file once the receiver says `Have`.

**No piece outlives its transfer.** Pieces exist only in flight. The receiver writes each verified piece straight into the sealed file, and a partial download is that sealed file plus a list of the pieces still missing. The moment the last piece lands, the list and any leftover temporary data are deleted; the same happens on cancel, expiry or deletion of the message. Nobody keeps a store of pieces in order to serve them. A device that serves a file makes each piece again from its own sealed copy (decrypt, re-encrypt under `K` with the piece's index), and gets byte-for-byte the piece the sender made.

## Wire format

Feature bit: `FEATURE_LARGE_FILES = 524288` in `Hello`. It is reserved but not yet sent: the codecs exist (`threnody_core::transfer`), the node doesn't take part in transfers yet. Message kinds (Appendix C): `Offer = { 0 => 32, 2 => bstr .cbor FileOffer }` and `Transfer = { 0 => 33, 2 => bstr .cbor TransferMsg }`.

```
FileOffer   = { 0: sent_ms uint, 1: sender's message id uint (0 = none), 2: name tstr (≤ 255 bytes),
                3: size uint (≤ 64 GiB), 4: root (32), 5: key (32), 6: piece size uint (262144),
                ? 7: flags uint (File key 6: 1 sensitive, 2 voice, 4 video, 8 forwarded),
                ? 8: duration_ms uint, ? 9: caption tstr, ? 10: album uint,
                ? 11: thumbnail bstr (≤ 24 KiB JPEG) }                     ; ≤ 96 KiB encoded
TransferMsg = { 0: op uint, ... }
  op 1 Want    { 1: request uint, 2: root (32), 3: first uint, 4: count uint (1..=16), ? 5: true (hashes) }
  op 2 Piece   { 1: request uint, 2: index uint, 3: data bstr (≤ 262144 + 40) }
  op 3 Hashes  { 1: request uint, 2: first uint, 3: data bstr (n × 32, 1 ≤ n ≤ 8192) }
  op 4 Missing { 1: request uint, 2: bstr (n × u64be piece index, n ≤ 16) }   ; none: has nothing of the file
  op 5 Have    { 2: root (32) }

piece_key = KDF(FILE_PIECE_KEY, K)
nonce_key = KDF(FILE_PIECE_NONCE, K)
nonce_i   = BLAKE3-keyed(nonce_key, u64be(i) || plaintext_i)[..24]
piece_i   = nonce_i || XChaCha20-Poly1305(piece_key, nonce_i, plaintext_i, aad = u64be(size) || u64be(i))
hash_i    = BLAKE3(piece_i)
root      = BLAKE3-derive_key(FILE_ROOT, u64le(size) || u64le(262144) || hash_0 || ... || hash_n)
```

The KDF labels are `"threnody v1 2026-11-20 file piece key"`, `"… file piece nonce"` and `"… file root"`; `KDF` is the one in the spec (§3.2). Test vectors are under *offered file* in `docs/test-vectors/v1.txt`.

- `Offer` is a new message kind for 1:1 chats and a new group content kind (Appendix F, *Message content*), so groups use the same offer inside MLS.
- A `Want` for pieces asks for `count` pieces from `first`. One with key 5 asks for `count` parts of the hash list (8192 hashes, 256 KiB, each), the first starting at hash `first`. Answers carry the `request` back.
- Every piece but the last is exactly 262144 bytes of plaintext. An empty file is one empty piece. The count follows from `size`. Truncating, reordering or appending pieces fails the hash check before decryption, and the AEAD binds each piece to its index and the file's size.
- **Nonces.** The nonce is derived from the piece's plaintext, so every holder makes byte-identical pieces. A source whose copy changed can't reuse a nonce for other bytes: it makes a different piece, which fails its hash. Sources still never send a piece whose hash isn't the committed one.
- Hashes are over ciphertext, so `root` reveals nothing about the plaintext. Because `K` is fresh for every send, the same file sent twice (or forwarded) has unrelated roots. Forwarding re-encrypts under a new key.
- The AEAD stops a party without `K` from forging pieces. Every legitimate source holds `K`, so it is the hash list committed by `root` inside the sender's authenticated offer that rules out a bad source poisoning a piece. A piece that fails its hash is dropped, and its source isn't asked again for that file.

## Sources

A requester asks for windows of up to 16 pieces, with at most 3 sources in flight per file and no piece asked of two sources at once. A source answers with `Piece`s for what it has and `Missing` for the rest. A window with nothing for 60 s is asked of another source. Who may be a source, in the order they are tried:

1. **The sender's device that sent it.** It always serves its own sends while the entry exists and the original still matches `root`. It serves on any network: the user chose to send.
2. **The receiver's own siblings** that already have the file (Appendix J, *Files*). Device sync moves onto this format, replacing `FileWant`/`FilePart` and their 8 MiB `total`.
3. **The sender's siblings** that have the file through device sync. A phone can send a video and go offline, and the receiver still gets it from the sender's laptop.
4. **Group members** (phase 3) that hold the file, complete or partial, and with whom the requester already has a session. The sender uploads roughly one copy and members spread it among themselves, the forwarder first since it already relays to members it reaches.

A source serves only:

- a `root` from its own history, read from a location it recorded itself (the device-sync rule);
- to a party the offer was sent to: the 1:1 contact's devices, or the members of the group at the time of the offer;
- over a session that already exists. It never opens a connection just for a transfer, and it never takes a session onto a more direct path than the user's reachability and anonymity settings allow (Appendices M and N).

## The swarm, and how far it goes

Swarming is worth having, but only among parties that already hold the file or may hold it:

- **In 1:1 chats** the swarm is the two accounts' devices. The win is availability (the sender's other devices keep serving after the sending phone goes away) more than speed.
- **In groups** it is a real win for both speed and the sender's data plan: 20 members no longer means 20 uploads from one phone.

Content is never the problem with outside holders: a piece without `K` is noise. Metadata is. A public swarm (BitTorrent, IPFS, the Mainline DHT for content) would show every peer the IP address of everyone who uploads or fetches a given `root`, which ties the sender to the receivers, gives their rough location, and reveals the file's size and timing. That is the "who talks to whom" Threnody exists to hide, so a plain public swarm stays out of scope. *Blind piece stores* (below) get the availability of outside holders without that leak.

## Blind piece stores (phase 4)

Volunteer relays (Appendix P) may opt into a **store** role: holding opaque, encrypted 256 KiB blobs for a short time. A sender uses stores when no direct source will be around, so a file stays available while every holder's phone is off. Each rule below closes off one thing a store or a network observer could otherwise learn:

- **Who uploads and who fetches.** Stores are reached only through onion circuits (Appendix I), deposits and fetches alike, each on its own circuit chosen at random. A store sees a circuit, never an address. This is how mailbox deposits already work (Appendix H).
- **Which blobs belong to one file.** Blobs aren't filed under `root`. Each one gets its own locator, `loc_i = KDF("threnody file locator", K, u64be(i))`, which looks random without `K`. Shares go to different stores, so one store sees a few unrelated blobs, not a file.
- **How big the file is.** The store sees only fixed-size blobs, a few of them at most. Only someone holding `K` can tell how many there are in all.
- **Stores going away.** Pieces are erasure-coded before upload (Reed-Solomon, 16 data shares to 24 stored per group of pieces), so any 16 of 24 rebuild the group. Shares are checked against hashes committed by `root`, as pieces are.
- **How long blobs live.** A store keeps a blob for at most 72 hours. With each blob it keeps `BLAKE3(KDF("threnody file delete", K, u64be(i)))`, and anyone who knows `K` deletes it by showing the preimage. In a 1:1 chat the receiver deletes every share the moment its download completes; in a group the sender deletes them once every member has sent `Have`. The expiry covers the rest.
- **Floods.** A deposit spends a relay token (Appendices O and P, unlinkable BBS presentations), so a store can enforce quotas without knowing who is depositing. A store has a fixed budget and declines past it.
- **Timing.** A global observer could still match a deposit to a later fetch by timing, as for all onion traffic (Appendix I, *Known limits*). Receivers fetch in random order over several circuits, and don't start the instant the offer arrives, so that matching is harder, not impossible.

**Cost.** Every byte crosses three relays on the way up and three on the way down, plus 50% for the erasure code: about 9 bytes of volunteer traffic per byte delivered. So stores are a fallback, not the default path. They are used only when no direct source will be reachable (the sender is going offline, or a group member is unreachable), and only for files up to a store limit (proposed 256 MiB) until the volunteer network has capacity to spare. Speed through stores comes from many circuits in parallel, but a single circuit is slower than a direct link, so a file from an online sender always comes directly.

Simpler, and private to you: **your own always-on relay node** (docs/relay-node.md) can take the store role for your own sends only. It already sees your metadata, so it needs no circuits or erasure coding.

## Policy and settings

- **Receiver.** Automatic downloads follow today's rules (photos, voice and video, not sensitive), with an extra limit: files of at most 25 MB on Wi-Fi and 8 MB on mobile data. Message requests never download automatically. Anything else waits for a tap. Any download, automatic or asked for, starts only if the space free covers the partial file, the kept copy and 512 MiB more; otherwise it shows *not enough space* and nothing goes on the network.
- **Serving for others** (siblings serving the other account, group members serving each other): on unmetered networks only by default; a setting "Help deliver files in groups" turns it off. The sender always serves its own sends.
- **Disappearing messages.** When the entry expires, the holder discards `K`, stops serving and deletes the file, partial or complete, along with any shares it can still reach in stores.
- **Sensitive files.** Never downloaded automatically. Holders that downloaded one may serve it to other parties of the same chat.
- **How long a sender serves.** While the history entry exists, until every recipient has said `Have`, or 30 days, whichever comes first. After that the receiver sees "no longer available from the sender".

## Metadata

For now a photo or video whose metadata is removed is read whole to strip it, so one over 64 MiB is refused while metadata removal is on (the app says to turn it off to send it). A sender serving it strips it again, keeping the stripped copy in memory for two minutes between windows. `media::strip` and `as_sent` take the whole file in memory. Large videos need streaming stripping (the BMFF walker in `media/bmff.rs` rewriting box by box into a temporary file). Images over 8 MiB are rare once converted, but conversion still decodes at most 4096 px on the longer side (memory-bound bitmaps); that stays. A file that can't be stripped streaming is refused, as now, never sent with its metadata.

## Limits that change

| Limit | Today | With offers |
| --- | --- | --- |
| File size | 8 MiB | receiver's free space; decoder bound (see *Open questions*) |
| Largest message | 16 MiB | unchanged (pieces are 256 KiB) |
| Tracked per peer | 32 MiB | unchanged (offers are small, pieces aren't tracked) |
| Mailbox per recipient | 16 MiB | unchanged (offers only) |
| Group held per member | 32 MiB | unchanged (offers only) |
| Device sync `Part.total` | 8 MiB | replaced by this format |

Inline `File` stays for files of at most 8 MiB to peers without the bit. To such a peer a larger file fails at once with "Alex's app needs an update to receive files over 8 MB".

## Phase 1 as built

- **Opting in.** An app installs a large-file store (`set_large_file_store`): size and range reads of a location, a reserved private path for a received file, and the free space. The node then adds `FEATURE_LARGE_FILES` to the `Hello` of sessions that start afterwards. Without a store (the CLI, personas for now), nothing changes.
- **Sending.** `send_file_at(peer, name, location, options)` reads through the store. A file of at most 8 MiB, or any file to a peer without the bit, goes inline as before (a larger one to such a peer fails with "their app needs an update"). Otherwise the node prepares it once, keeps `K`, the root and where it is in `transfers-out`, and the hash list in `xfer-<first 16 bytes of the root, hex>`, then sends the `Offer` tracked, like a file: it waits for a session, since offers aren't sealed for mailboxes yet. History records the file at its location.
- **Receiving.** The node records an offer in history itself, without contents, keeps it in `transfers-in` (at most 256), and tells the app (`FileOffered`). Downloads run at most two at a time, only over a live session with the sender. The hash list comes first, one part at a time, and is checked against the root. Pieces follow in windows of 8: each is checked against its hash and written, still encrypted under `K`, to `transfers/<root>.part` in the node's home. After a restart that file is checked piece by piece and the download goes on from the first missing piece. A window with nothing for 60 s is asked again. A `Missing` from the sender makes the download *unavailable*.
- **Finishing.** With every piece in, the node decrypts the partial file a piece at a time into the path the store reserved, sealing it as received media is sealed, records that location in history, deletes the partial file and the hash list, and sends `Have`. The sender then forgets the offer.
- **Serving.** The sender answers only a `Want` from the conversation it offered the file to. Each piece is made again from the file as it is now and sent only if its hash is the one committed to; otherwise the piece is listed in `Missing`. Offers go after 30 days, or once their history entry is deleted or expired, as do incoming offers whose entry is gone (with their partial files).
- **Cover traffic.** `Transfer` messages skip constant-rate ticks, as calls do: a transfer's volume shows whatever the ticks do, and one 256 KiB piece per tick (every 2 s by default) would take hours over a large file. The `Offer` itself waits for a tick like any message.
- **Backups** leave transfer state out (`transfers-out`, `transfers-in`, `xfer-*`): a restore has none of the files it refers to.

## Work plan

Each phase is its own PR (or a few), and each leaves the app shippable.

**Phase 1: offers and pull from the sender (1:1)**

- Core (`threnody-core`): new `transfer.rs` with `Offer`, `Want`, `Piece`, `Hashes`, `Missing`, `Have` codecs, piece encryption and the hash list and root. `FEATURE_LARGE_FILES` and the new kinds in `message.rs`. History file record gains `root`, `K` and transfer state (encrypted state, older readers skip). Test vectors in `docs/test-vectors`, a fuzz target `fuzz/fuzz_targets/transfer.rs`, cases in `robustness.rs`.
- Net (`threnody-net`): `transfer.rs` engine generalised from `filesync.rs` (queue, windows, stall and resume, progress persisted in `file-transfers` state, partial files sealed on disk). Offers sent as tracked messages and through mailboxes. Serving from a location with range reads.
- FFI: path-based `send_file_path(peer, location, size, options)`. `FileStore` gains range reads, a streaming writer (create, append, finish, abort) and `free_space()`. Events for offer, progress, done, failed. `download`, `cancel` and settings.
- Streaming metadata stripping for video.
- Android: SAF streaming (no `readBytes`), `StatFs` for free space, progress in the bubble, **Download (size)**, long transfers in a foreground service or WorkManager so they survive the background.
- Linux: the same through GIO streams, at parity with Android.
- iOS: the same (built and tested on the Mac).
- Spec: this appendix finalised. Appendix C (feature bit, kinds, history record) and Appendix H (offers in mailboxes) updated.

**Phase 2: several sources across devices**

- Requester asks up to 3 sources in parallel. Bad-piece handling.
- Device sync (Appendix J, *Files*) moves onto offers and pieces. `FileWant`/`FilePart`/`FileMissing` stay readable for older siblings for one release.
- The sender's siblings and the receiver's siblings become sources.

**Phase 3: groups**

- `Offer` as group content (Appendix F). Members pull from the sender, the forwarder and members they have sessions with. Partial holders serve the pieces they have.
- "Help deliver files in groups" setting. The group-held limits stay as they are, since only offers are held.

**Phase 4: blind piece stores**

- Store role for volunteer relays (Appendix P): descriptor flag, token-gated `Deposit`, `Fetch` and `Delete` by locator, 72-hour expiry, a fixed storage budget.
- Erasure coding (Reed-Solomon, 16 of 24) in core. Uploads and fetches over separate onion circuits.
- The own-relay-node variant first, since it needs none of the circuit machinery.
- Spec: Appendix P (store role), Appendix H (how stores relate to mailboxes). A Tamarin model for locator unlinkability and deletion in `proofs/`.

## Open questions

- **Decoder sanity bound.** Proposed 64 GiB (an 8 MiB hash list at 256 KiB pieces). Without some bound, a malformed offer can make a receiver plan an absurd transfer.
- **Window size over high-latency paths.** 16 × 256 KiB per window may starve onion circuits or saturate Bluetooth. Possibly adapt the window to the path.
- **Should small files also use offers in groups?** A 5 MB photo to 50 members is 250 MB of upload from one phone. Offers above about 1 MiB in groups would let members share the load.
- **Default for serving on mobile data** in groups (proposed: off).
- **Stores by default?** Proposed: on only as a fallback when no direct source will be reachable, and only up to 256 MiB per file. The limit depends on how much volunteer capacity there is.
- **Erasure parameters.** 16 of 24 survives a third of stores vanishing. The right figure depends on how often volunteer relays actually drop out.
