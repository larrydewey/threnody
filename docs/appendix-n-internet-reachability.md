# Appendix N: Reaching Contacts Across the Internet (draft)

Status: **planned for 0.3.0, not implemented.** This is the design to build. Details may change during implementation, and this document will be updated to match.

## The problem

Two devices can only form a session if one of them accepts an incoming connection from the other. Today that rarely holds away from home:

- **Phones on mobile data** sit behind carrier-grade NAT and accept nothing from outside.
- **Home devices** sit behind a router, at a private address such as `192.168.x.y`.
- **Threnody has no server**, so there is no rendezvous point or relay in the middle.

So two contacts meet only on the same network (LAN discovery, Appendix E), over Bluetooth (K) or Wi-Fi Direct (L), or through a mutual contact who is reachable (relays, G; onion circuits, I; mailboxes, H). This appendix makes contacts reach each other directly across the internet, with no server of Threnody's own.

## Goals

- **Two NATed contacts connect directly:** for example, a phone on mobile data and a laptop at home, with nothing configured on either router.
- **No Threnody infrastructure.** No servers to run, trust or take down.
- **No new metadata for a passive observer.** Who talks to whom stays hidden from everyone but the two devices themselves.
- **Approved contacts only.** Nobody else can find or reach a device this way.
- **Graceful fallback.** When a direct path can't be made, relays, onion circuits and mailboxes through contacts still work as they do now.

## Non-goals

- **Getting through symmetric NAT on both ends.** When both devices' NATs assign a new outside port for every destination, port prediction is unreliable. That case still needs a relay through a reachable contact; volunteer relay directories (spec §9 layer 2) would help later.
- **Reaching strangers.** First contact still uses an invite with an address, a QR code, Bluetooth or the LAN.

## Overview

1. **UDP transport.** Sessions also run over QUIC on the same port number as TCP (UDP 7450). NAT hole punching works with UDP. QUIC only carries bytes: the Threnody handshake (Appendix A) and ratchet (B) run inside one QUIC stream, unchanged.
2. **Learning your own addresses** (candidates): your public address and port as seen from outside, your LAN addresses, and global IPv6 addresses.
3. **Rendezvous through the public BitTorrent DHT** (Mainline, BEP 5 and BEP 44). Each node stores its current candidates for each mutually approved contact, encrypted under a key only the two of them share. The record is stored under a storage key that rotates.
4. **Hole punching.** Both sides send probes to each other's candidates, which opens a path through both NATs and firewalls. Then one side starts the QUIC connection.

## 1. QUIC transport

- **The endpoint.** Each node runs one QUIC endpoint (the `quinn` crate) on one UDP socket, the same port number as its TCP listener. Every outgoing probe and connection comes from this socket, so the outside port a NAT assigns stays the same for all of them.
- **TLS is only a wrapper.** QUIC requires TLS, so each node uses a throwaway self-signed certificate, and certificates aren't checked. The Threnody handshake inside the stream authenticates both identities and pins fingerprints, exactly as over TCP, so QUIC's own encryption adds a layer without being relied on. ALPN is `threnody/1`.
- **One stream per session.** A session uses one bidirectional QUIC stream, handed to the existing `connect_stream` / `accept_stream`. Framing, padding and cover traffic (Appendix C) are unchanged.
- **Keeping the path open.** NAT mappings expire, often within 30 s for UDP, so idle sessions need keepalives. Cover traffic (one frame every 2 s, or 10 s on mobile data) already keeps the path open. With cover traffic off, a QUIC keepalive every 15 s does instead.
- **Invites can say "UDP too"** with a new optional flag. Dialing tries QUIC and TCP together and keeps whichever completes first.

## 2. Candidates

A node gathers its candidates as follows:

- **Reflexive (as seen from outside).**
  - Every session peer reports, inside the encrypted session, the address and port it sees us at: a new `Observed` message.
  - DHT responses include the requester's public address (BEP 42), so DHT queries provide one too. No STUN server is needed.
  - A candidate that two independent observers agree on is trusted. One that changes for each destination marks this NAT as symmetric.
- **Local:** the LAN interfaces' addresses, for two devices behind the same NAT that can't see each other's discovery beacons.
- **IPv6:** global unicast addresses. With IPv6 there's no NAT to cross, only a firewall, and probing usually opens it. Many mobile carriers are IPv6-first, so this alone covers a large share of mobile cases.

Candidates are re-gathered whenever the network changes (Android's network callback, or a changed interface list) and every 10 minutes.

## 3. Rendezvous in the Mainline DHT

The Mainline DHT is the public network BitTorrent clients use: millions of nodes, no operator. BEP 44 lets anyone store a small signed, mutable value (up to 1,000 bytes) under an Ed25519 public key plus an optional salt.

### Keys

These keys are derived from the pairwise discovery key `D(A,B)` that LAN discovery already keeps for each mutually approved contact (Appendix E). Both sides hold it, and revocation deletes it.

```
epoch      = unix_seconds / 3600
seed(X→Y)  = KDF(D, "rendezvous signing key" || id_X || epoch)  -- X's records for Y
signer     = Ed25519 key pair from seed(X→Y)
salt       = KDF(D, "rendezvous salt" || id_X || epoch)[0..16]
enc_key    = KDF(D, "rendezvous encryption" || id_X || epoch)
```

Both sides can derive both directions, so each reads the other's record without having published anything. The DHT key changes every hour. The values look random, and no Threnody identity appears anywhere. Records are fetched for the current hour and the previous one, and for each discovery key still kept (the current one and the two before it), as LAN discovery does.

### Record

```
value = nonce (24) || XChaCha20-Poly1305(enc_key, nonce, Candidates, aad = "threnody rendezvous v1")
Candidates = { 0: [* candidate], 1: issued_ms uint, 2: flags uint, ? 3: seeking_until_ms uint }
candidate  = [kind uint (1 reflexive, 2 local, 3 IPv6), addr bstr (4 or 16), port uint]
flags: 1 symmetric NAT suspected
```

Every record is padded to the same length, so its size doesn't reveal how many candidates a node has.

- **Publishing.**
  - For each mutually approved contact without a live session, a node publishes its record when its candidates change, and otherwise every 30 minutes.
  - When it wants to reach a contact (a message is waiting, or the user opened the chat), it sets `seeking_until` to two minutes ahead and republishes at once.
- **Watching.** A node polls the records of contacts it has no session with.
  - It polls every 2 minutes while the app is open or a contact is being sought, and every 15 minutes in the background.
  - A record that is newer, or says it's seeking us, starts hole punching (section 4).
- **Cost.** One DHT read or write takes a few round trips and a few kilobytes. Polling ten contacts every 15 minutes is under 1 MB a day.

## 4. Hole punching

Once either side sees the other's fresh candidates:

1. **Both sides probe.** For 30 seconds, each side sends a small UDP probe to every candidate of the other, once a second, from its QUIC socket. Probes are a random 32-byte nonce with a keyed tag (as LAN beacons are), so only the intended contact recognises them. Sending a probe opens this side's NAT mapping and firewall for replies from that address.
2. **The smaller identity dials.** When a probe arrives, or after 3 seconds, the device with the smaller identity key starts a QUIC connection to each candidate in turn: IPv6, then local, then reflexive. As LAN discovery does, it pins the expected fingerprint.
3. **The handshake decides.** The Threnody handshake inside the QUIC stream authenticates both sides. A probe from anyone else can at most cause a connection attempt that then fails.
4. **Fallback.** If nothing connects within 30 s, the existing paths are tried as today: relays and onion circuits through reachable contacts, and mailboxes for offline delivery. The node backs off before punching that contact again: 2, 5, then 15 minutes.

**Symmetric NAT.** With `flags & 1` set on exactly one side, the other side (which has a stable port) dials, and the symmetric side's probes open its own mapping toward that port. That usually works. When both sides are symmetric, the relay fallback applies.

## Privacy

| Who | Learns |
|---|---|
| A passive network observer | UDP traffic to some IP addresses, like any QUIC or BitTorrent traffic. Not who the devices are. |
| DHT nodes | That this IP address stores and fetches small values under random-looking, hourly-changing keys. Not identities, contacts, or how many contacts (records are padded and keys don't link). |
| A contact | Our current public addresses. They learn this anyway once a direct session exists. |
| Anyone else | Nothing. Without `D(A,B)` they can neither find nor decrypt our records, nor recognise our probes. |

**Trade-offs, stated honestly:**
- **Any DHT participant sees our IP address.** That's inherent in using a public peer-to-peer network, and true of every direct peer-to-peer connection too.
- **A direct connection shows each device's IP address to the other.** Anonymous identities (Appendix M) that must hide their address from the peer should not punch holes; they keep using relays. Personas will have this off, and say why.
- **Rendezvous timing:** someone who could watch both devices' DHT traffic could correlate when they look each other up. Hourly key rotation and background polling at fixed intervals blur this but don't remove it.

## Security

- **Authentication is unchanged:** the Threnody handshake inside QUIC, with fingerprints pinned. A forged DHT record or probe can only cause a failed attempt.
- **Records are encrypted and authenticated** under keys only the two contacts hold. BEP 44 signatures (from keys derived per direction and per epoch) stop DHT nodes from altering them.
- **Revocation** deletes `D(A,B)`. From then on the node neither publishes records for that contact nor reads theirs.
- **Resource limits:** at most 32 probes in flight, punching at most 4 contacts at once, and backoff per contact.
- **The QUIC endpoint** accepts connections only to run the Threnody handshake. `check_policy` applies as for TCP.

## Settings

The feature will have a toggle: *Reach contacts over the internet* in the app, and `--no-rendezvous` in the CLI. It's off for anonymous identities.

**Decision needed before release:** whether it's on by default. It is what makes mobile data work. But it adds DHT participation, which shows your IP address to strangers on that network, though not who you talk to. The project's rule is that protections default on. This is a reachability feature with a privacy cost, so the default is the user's call.

## Work plan

1. **QUIC transport:** the endpoint on UDP 7450, dialing and accepting into the existing session code, invites with the UDP flag, keepalives. Tests over loopback.
2. **Observed addresses:** the `Observed` session message, candidate gathering, symmetric-NAT detection.
3. **DHT records:** key derivation, record format, publishing and polling (the `mainline` crate), padding, and epochs. Unit tests for the key schedule; tests against a local DHT testnet.
4. **Hole punching:** probes, the dialing rule, timeouts, fallback and backoff. Tests with network namespaces simulating cone and symmetric NATs.
5. **Wiring:** the app toggle and status ("reachable directly", "via relay"), the CLI flag, `/status`, docs (Appendix C, N, README), and a field test from a phone on mobile data to a laptop at home.

## Dependencies

- `quinn` (QUIC), with `rustls` on the `ring` backend.
- `rcgen`, for the throwaway certificate.
- `mainline`, for the DHT client and BEP 44 storage.

All are Rust, pure or with `ring`, and build for Android.
