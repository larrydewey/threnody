# Appendix E: Private LAN Discovery (v1)

Status: implemented in `threnody-core::discovery` (beacons) and `threnody-net::discovery` (UDP transport, automatic dialing). Fulfils spec §7.1 ("automatic discovery of nearby approved devices") and §10 ("automatic local mesh joining among already-approved devices").

## Keys

```
D(A,B) = channel.export("lan discovery key")      -- per session, symmetric
```

The discovery key is stored in each side's contact entry (Appendix C, contact key 8). It is replaced every time a mutually approved session starts, and deleted on revocation.

The previous two keys are kept as well (contact key 11) and are accepted when recognising beacons. When both sides dial at once, two sessions complete and each side may keep a different one as "latest". Without the older keys, the two sides would then stop recognising each other. Revocation deletes all of them. Blocking a contact is a revocation too, and so is one that arrives through contact sync from our other devices (Appendix J): either way the keys go.

Internet rendezvous (Appendix N) derives its DHT keys and hole-punching probes from `D` as well, so only mutually approved contacts find each other that way. Invite links and handles, which reach people who aren't contacts yet, use a meeting place of their own (Appendix S).

## Beacon

```
beacon = nonce (16, random) || port (u16 BE) || tag_1 || ... || tag_k
tag_i  = BLAKE3-keyed(D_i, label || sender_id || epoch (u64 LE) || nonce || port)[0..16]
epoch  = unix_seconds / 300
k      = number of approved peers rounded up to a multiple of 8 (max 64); spare slots random, order shuffled
```

- **Sending.** A node sends a beacon every 10 s to UDP `239.255.84.86:7451`, but only when it has at least one approved peer with a discovery key.
- **Receiving.** A node checks each mutually approved contact's key against the current epoch and its two neighbours.
- **Dialing.** On a match, the node with the smaller identity key dials `(source IP, port)`, pinning the peer's fingerprint. It waits at least 30 s before redialing the same peer. With onion routing on, it first tries an onion circuit when one can be built (Appendix I, *Background reconnection*), so an observer on the network doesn't see the two talk. A LAN contact that is the only possible first hop is dialed directly.

## Properties

- **Unlinkable to outsiders.** Each beacon is a fresh nonce followed by pseudorandom tags. Without a discovery key, two beacons from the same device can't be linked, and they don't reveal any identity.
- **Recognisable only by approved peers.** Approval is mutual, and revocation deletes the key.
- **Contact count hidden.** The number of contacts is rounded up to a block of 8.
- **No self-matching.** Each tag binds the sender's identity, so a node doesn't mistake its own beacon for a peer's, even though `D` is symmetric.
- **Replays get no one in, but can show who is a contact.** Dialing pins the fingerprint, and the dialer checks it before showing its own identity in the handshake (Appendix A), so a replayed or forged beacon can at most cause a connection attempt that fails. That attempt still says something: someone who records a beacon and replays it elsewhere within its epochs (about 15 minutes) sees which nearby device dials back, and so learns that the device is a contact of the beacon's sender, and where it is. By the dialing rule that is the contacts with the smaller key; over Bluetooth, after 45 s, every one (Appendix K). Binding a beacon to where it was sent would take a new beacon format, and a relay in real time would still work; until then this is a known limit.
- **Leaks.** Beacons do reveal that some Threnody device is present, plus its listening port and a coarse contact count. Disable them with `--no-discover`.

## Not yet done

- IPv6 link-local multicast.
- Wi-Fi Aware advertising with the same tag scheme. Bluetooth LE uses it already (Appendix K).
- Rotating the TCP port so the port in the beacon isn't a stable fingerprint.
