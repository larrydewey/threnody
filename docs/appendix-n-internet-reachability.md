# Appendix N: Reaching Contacts Across the Internet

Status: **sections 1 to 4 are implemented** (QUIC, candidates, DHT rendezvous, hole punching) for 0.3.0. Router port mapping (section 0) is not built yet. Field test, 2026-10-05: a Pixel 8a on AT&T LTE (carrier-grade NAT, Wi-Fi and Bluetooth off) and a laptop behind a home router found each other through the public DHT and connected directly over QUIC through both NATs, about two minutes after the phone left Wi-Fi.

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

0. **Asking the router first.** Before anything else, the node asks its router to forward its port (section 0). When that works and the public address is known, contacts simply dial it, with no DHT at all.
1. **UDP transport.** Sessions also run over QUIC on the same port number as TCP (UDP 7450). NAT hole punching works with UDP. QUIC only carries bytes: the Threnody handshake (Appendix A) and ratchet (B) run inside one QUIC stream, unchanged.
2. **Learning your own addresses** (candidates): your public address and port as seen from outside, your LAN addresses, and global IPv6 addresses.
3. **Rendezvous through the public BitTorrent DHT** (Mainline, BEP 5 and BEP 44). Each node stores its current candidates for each mutually approved contact, encrypted under a key only the two of them share. The record is stored under a storage key that rotates.
4. **Hole punching.** Both sides send probes to each other's candidates, which opens a path through both NATs and firewalls. Then one side starts the QUIC connection.

## 0. Router port mapping

Most home routers let a device on the network request a port forward, using UPnP IGD, NAT-PMP or its successor PCP (RFC 6887).

- **Requesting the mapping.** A node asks its router to forward TCP and UDP 7450 to itself, renewing before the lease ends. It removes the mapping on shutdown, and makes no request when the setting is off.
- **Learning the public address.** The router reports the public address in its answer. Session peers confirm it with `Observed` messages (section 2).
- **Remembering it.** A contact who was told the address while connected (in the session, so never in the clear) remembers it as that contact's *home address*. A phone that left home Wi-Fi then dials the remembered address first.
- **When the DHT is still needed:** only when the remembered address fails, for example because the address changed or the router refused the mapping.

This covers the common case of a phone reaching a home computer cheaply, with nothing published anywhere. It doesn't help two phones on mobile data, whose carriers do the NAT and offer no mapping. That case is what sections 1 to 4 are for.

## 1. QUIC transport

- **The endpoint.** Each node runs one QUIC endpoint (the `quinn` crate) on one UDP socket, the same port number as its TCP listener. Every outgoing probe and connection comes from this socket, so the outside port a NAT assigns stays the same for all of them.
- **TLS is only a wrapper.** QUIC requires TLS, so each node uses a throwaway self-signed certificate, and certificates aren't checked. The Threnody handshake inside the stream authenticates both identities and pins fingerprints, exactly as over TCP, so QUIC's own encryption adds a layer without being relied on. ALPN is `threnody/1`.
- **One stream per session.** A session uses one bidirectional QUIC stream, handed to the existing `connect_stream` / `accept_stream`. Framing, padding and cover traffic (Appendix C) are unchanged.
- **Keeping the path open.** NAT mappings expire, often within 30 s for UDP, so idle sessions need keepalives. Cover traffic (one frame every 2 s, or 10 s on mobile data) already keeps the path open. With cover traffic off, a QUIC keepalive every 15 s does instead.
- **Dialing an address falls back to QUIC** when TCP fails, so an address that only answers on UDP still works. (An invite flag for "UDP too" isn't needed for that and isn't implemented.)
- **Dead TCP sessions are noticed within about a minute** (TCP keepalive every 10 s after 30 s idle, and a 60 s limit on unacknowledged writes). A phone that leaves Wi-Fi sends nothing, and until its old session ends the other side neither publishes for it correctly nor punches toward it.

## 2. Candidates

A node gathers its candidates as follows:

- **Reflexive (as seen from outside).**
  - Every session peer reports, inside the encrypted session, the address and port it sees us at: a new `Observed` message.
  - DHT nodes report the address they see a query come from (the `ip` field of BEP 42). The node pings a few DHT nodes *from its QUIC socket* (a few well-known ones plus some from its routing table), so the answer is the outside address and port of that very socket. No STUN server is needed. Libtorrent-based nodes answer with `ip`; some others don't. If none answers, the DHT client's own view of our IP, with our port, is used as a guess (many NATs keep the port).
  - A candidate that two independent observers agree on is trusted. One that changes for each destination marks this NAT as symmetric.
- **Local:** the LAN interfaces' addresses, for two devices behind the same NAT that can't see each other's discovery beacons.
- **IPv6:** global unicast addresses. With IPv6 there's no NAT to cross, only a firewall, and probing usually opens it. Many mobile carriers are IPv6-first, so this alone covers a large share of mobile cases.

Candidates are re-gathered whenever the network changes (Android's network callback) and every 10 minutes, or every 30 seconds while the outside address is still unknown.

The QUIC socket carries these pings and the probes as well as QUIC. A wrapper around the socket takes them out before the QUIC endpoint sees them: probes have the QUIC fixed bit clear (QUIC "bit greasing" is turned off, so no peer clears it), and DHT replies are bencoded dictionaries marked as replies.

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
  - For each mutually approved contact, connected or not, a node publishes its record when its candidates change, and otherwise every 30 minutes. Records stay current even during a session: a session can die unnoticed, and an old record sends the contact to an old address.
  - When it wants to reach a contact (a message is waiting, the user opened the chat, or it starts punching), it sets `seeking_until` to two minutes ahead and republishes at once.
  - DHT write tokens are bound to the writer's IP address. After a network change the node leaves the DHT and joins again from the new address; otherwise its writes are silently refused for several minutes. It also rejoins after two publishing rounds in which every write failed.
- **Watching.** A node polls the records of contacts it has no session with.
  - It polls every minute while the app is open, every 15 seconds while it is seeking a contact, and every 15 minutes in the background.
  - A record that is newer than the last one acted on starts hole punching (section 4). A record that can't be acted on yet (backing off, too many punches) stays "new".
- **Cost.** One DHT read or write takes a few round trips and a few kilobytes. Polling ten contacts every 15 minutes is under 1 MB a day.

## 4. Hole punching

Once either side sees the other's fresh candidates:

1. **Both sides probe.** Each side sends a small UDP probe to every candidate of the other (at most 8), once a second, from its QUIC socket. Probes are a 16-byte nonce and a 16-byte tag keyed with `D` (as LAN beacons are), so only the intended contact recognises them. Sending a probe opens this side's NAT mapping and firewall for replies from that address. A probe that arrives tells the receiver a path that works: its source address goes first in the receiver's list.
2. **Overlapping in time.** Both sides must punch at once, but each only learns of the other through polling. So the side that starts (it found a new record) marks its own record as seeking the contact and keeps punching for two minutes; the contact, seeing a record that seeks it, joins in for 45 seconds, even while backing off. A probe from the contact also makes a node join in.
3. **One side dials.** When a probe arrives, or after 3 seconds, the device with the smaller identity key dials every candidate at once over QUIC, retrying every 5 seconds; the first connection wins. As LAN discovery does, it pins the expected fingerprint.
4. **The handshake decides.** The Threnody handshake inside the QUIC stream authenticates both sides. A probe from anyone else can at most cause a connection attempt that then fails.
5. **Fallback.** If nothing connects, the existing paths are tried as today: relays and onion circuits through reachable contacts, and mailboxes for offline delivery. The node backs off before starting to punch that contact again: 2, 5, then 15 minutes.

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

The feature has a toggle: *Reach contacts over the internet* in the app's menu, and `--no-rendezvous` in the CLI. It covers DHT rendezvous and hole punching (and, later, router port mapping). With it off, the node never joins the DHT. QUIC itself stays on: it carries sessions to addresses the user dials.

The CLI's `/status` and the app's Diagnostics screen show whether the node is in the DHT, its addresses, and whether its NAT looks symmetric. Events (`Addresses`, `Punching`, `PunchFailed`, and diagnostic `ReachNote`s for records published and found) go to the CLI output and the app's diagnostic log.

**Decided (2026-10-05): on by default.** It is what makes mobile data work, and the cost is stated above. The DHT shows your IP address to strangers on that network, but not who you talk to. Anyone who prefers can turn it off. Anonymous identities have it off, since a direct path shows their address to the peer.

## Later: relays take over most of this

Once a volunteer relay network has critical mass (spec §9 layer 2; "volunteer relay directories" in the README), reachability changes:

- **Relays become the default path.** A device behind NAT keeps one outgoing connection to a relay, and contacts reach it there, as Tor onion services do. That also hides each side's IP address from the other, which a direct path can't, so it suits anonymous identities too.
- **Rendezvous remains, in a smaller role.** A contact still has to learn which relay to use, through a relay directory or, as a fallback, the DHT records described here.
- **Direct paths stay as an option** for bandwidth (photos and files), lower delay, and not depending on volunteers' capacity.

So DHT rendezvous is the bridge to a relay network, and afterwards a fallback. The record format and key schedule here are meant to carry a relay's address as a candidate too (a new candidate kind), so the same rendezvous serves both.

## Work plan

0. **Router port mapping:** UPnP IGD, NAT-PMP and PCP, lease renewal, removal on shutdown, and remembered home addresses. *Not built yet.*
1. **QUIC transport:** done (`threnody-net` `quic`): the endpoint on the TCP port's number, dual-stack, dialing and accepting into the existing session code, keepalives, TCP-to-QUIC fallback. Tests over loopback.
2. **Observed addresses:** done: the `Observed` session message (feature bit 32), candidate gathering, DHT pings for the outside address, symmetric-NAT detection.
3. **DHT records:** done (`threnody-core` `rendezvous`, `threnody-net` `reach`): key derivation, record format, padding, publishing and polling with the `mainline` crate. Unit tests for keys and records; an end-to-end test against a local DHT testnet.
4. **Hole punching:** done: probes, the dialing rule, overlap through seeking records, timeouts and backoff. Not yet tested with network namespaces simulating NAT types; tested in the field instead (above).
5. **Wiring:** done: the app toggle, Diagnostics, foreground and network-change hooks, seeking when a chat opens or a message waits; the CLI flag and `/status`. Field test from a phone on mobile data to a laptop at home: passed.

## Dependencies

- `quinn` (QUIC), with `rustls` on the `ring` backend.
- `rcgen`, for the throwaway certificate.
- `mainline`, for the DHT client and BEP 44 storage.
- Later, for section 0: a small UPnP IGD / NAT-PMP / PCP client, written here or from a crate (to be chosen).

All are Rust, pure or with `ring`, and build for Android.
