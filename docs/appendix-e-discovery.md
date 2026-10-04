# Appendix E: Private LAN Discovery (v1)

Status: implemented in `threnody-core::discovery` (beacons) and `threnody-net::discovery` (UDP transport, automatic dialing). Fulfils spec §7.1 ("automatic discovery of nearby approved devices") and §10 ("automatic local mesh joining among already-approved devices").

## Keys

```
D(A,B) = channel.export("lan discovery key")      -- per session, symmetric
```

The discovery key is stored in each side's contact entry (Appendix C, contact key 8). It is replaced every time a mutually approved session starts, and deleted on revocation.

## Beacon

```
beacon = nonce (16, random) || port (u16 BE) || tag_1 || ... || tag_k
tag_i  = BLAKE3-keyed(D_i, label || sender_id || epoch (u64 LE) || nonce || port)[0..16]
epoch  = unix_seconds / 300
k      = number of approved peers rounded up to a multiple of 8 (max 64); spare slots random, order shuffled
```

- **Sending.** A node sends a beacon every 10 s to UDP `239.255.84.86:7451`, but only when it has at least one approved peer with a discovery key.
- **Receiving.** A node checks each mutually approved contact's key against the current epoch and its two neighbours.
- **Dialing.** On a match, the node with the smaller identity key dials `(source IP, port)`, pinning the peer's fingerprint. It waits at least 30 s before redialing the same peer.

## Properties

- **Unlinkable to outsiders.** Each beacon is a fresh nonce followed by pseudorandom tags. Without a discovery key, two beacons from the same device can't be linked, and they don't reveal any identity.
- **Recognisable only by approved peers.** Approval is mutual, and revocation deletes the key.
- **Contact count hidden.** The number of contacts is rounded up to a block of 8.
- **No self-matching.** Each tag binds the sender's identity, so a node doesn't mistake its own beacon for a peer's, even though `D` is symmetric.
- **Replays are harmless.** Dialing pins the fingerprint, and the handshake authenticates it, so a replayed or forged beacon can at most cause a connection attempt that fails. Epochs limit how long a replay works to about 15 minutes.
- **Leaks.** Beacons do reveal that some Threnody device is present, plus its listening port and a coarse contact count. Disable them with `--no-discover`.

## Not yet done

- IPv6 link-local multicast.
- Wi-Fi Aware advertising with the same tag scheme. Bluetooth LE uses it already (Appendix K).
- Rotating the TCP port so the port in the beacon isn't a stable fingerprint.
