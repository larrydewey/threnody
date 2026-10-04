# Appendix G: Relay Circuits (v1)

Status: implemented in `threnody-net::relay`, with `/relay` and fallbacks in the CLI. Fulfils spec §7.3 ("multi-hop routing among approved devices") and §7.4 ("any node MAY act as a relay … for other approved nodes"). Store-and-forward to offline peers is not part of v1.

## Model

A circuit is a chain of links between neighbours, each with a circuit id local to that link. Each relay stores only `(P, c) ↔ (Q, d)`.

The endpoints run the ordinary Threnody handshake (Appendix A) and ratchet (Appendix B) *over* the circuit. Relayed sessions are therefore authenticated end to end, forward-secret and post-quantum hybrid, just like direct ones.

Every relay message travels inside the ratchet of the link it crosses, so the end-to-end ciphertext is wrapped once more per hop.

```
A ──ratchet(A,B)──▶ B ──ratchet(B,C)──▶ C
   [ Data(c1, ratchet(A,C) frame) ]       [ Data(c2, same frame) ]
```

## Messages

These are carried as `AppMessage::Relay` (Appendix C, kind 7):

```
RelayMsg = { 0: op, 1: circ u64, ? 2: dest fingerprint (20), ? 3: ttl, ? 4: nonce (16), ? 5: frame }
op: 1 Open   2 Opened   3 Refused   4 Data   5 Close
```

Destinations are fingerprints, so anyone whose fingerprint you know (for example from an invite link) can be reached. The originator pins that fingerprint against the identity authenticated in the end-to-end handshake.

## Opening a circuit

1. **The originator** sends `Open { circ, dest, ttl = MAX_TTL - 1, nonce }` to each mutually approved neighbour in turn, until one answers `Opened`.
2. **A relay** receiving `Open` from `P` first checks four things, and replies `Refused` if any fails:
   - `P` is mutually approved with it.
   - `ttl ≤ 3`.
   - `P` has fewer than 64 circuits open through it.
   - It has never seen this `nonce` before. Nonces are remembered for 10 minutes, which breaks loops.

   It then extends the circuit:
   - If it is the destination, it binds the circuit to a new session and answers `Opened`.
   - Otherwise, if it has a live direct session with a mutually approved contact whose fingerprint is `dest`, it opens to that contact with `ttl = 0`.
   - Otherwise, if `ttl > 0`, it tries each other mutually approved neighbour with `ttl - 1`, depth-first.

   It answers `Opened` once a next hop accepts, or `Refused` when none does.
3. **On `Opened`**, the originator starts the handshake as initiator over the circuit. The destination accepts as responder and applies its normal accept policy.
4. **Data** frames are forwarded hop by hop. `Close` tears down both halves at every relay, and so does the loss of any underlying link. When the circuit goes, so does the end-to-end session.

The default `MAX_TTL` of 3 allows up to three relays between the endpoints.

## Policy

- **Who relays.** A node relays only between its own **mutually approved** contacts, and accepts circuits only from mutually approved neighbours. Relaying therefore follows existing trust, and strangers can't use a node as an open proxy.
- **No nesting.** Relayed sessions never carry relay traffic themselves.
- **No tunnels.** WireGuard tunnel offers aren't sent over relayed sessions, because they need a direct UDP path.

## Metadata

Relays learn who the destination is (its fingerprint), which neighbour a circuit came from, and when and how much traffic flows. Message contents stay hidden, and so does everything inside the end-to-end ratchet: headers, counters and keys. Hiding the communicating pair from relays requires layered (onion) encryption of the `Open` request. That belongs to the onion-routing layer of spec §9, not to v1 circuits.

## Transports

Circuits are made of sessions, and a session can run over any link: TCP, Bluetooth LE (Appendix K), or anything an app bridges in through `attach_link`. A phone that only has a Bluetooth link to a laptop can therefore reach peers the laptop reaches over IP. The relay forwards between its Bluetooth session and its TCP session like any other pair.

`Node::reach(addr, pin)` tries `addr` directly and then falls back to a circuit to `pin`. `Node::reach_peer(peer)` reuses a live session, or else tries the contact's last address and then relays. Apps use these through the FFI:

- `connect` accepts an invite link, a contact or a bare fingerprint, and falls back to relays.
- `send_text` tries to reach a peer that has no session (for up to 15 s) before falling back to sealed mailbox delivery.

Tested with a Pixel 8a linked to a laptop only by Bluetooth. The phone opened the invite of a second node listening on the laptop's `127.0.0.1`, an address the phone cannot reach. The session ran end to end through the laptop, and messages went both ways.

## CLI

- **`/relay <contact|fingerprint|invite>`** opens a relayed session.
- **`/connect <contact|invite>`** falls back to relays when the direct dial fails or no address is known. For an invite, the relay goes to the fingerprint in the link.
- **Group fan-out** tries a relay for members without a live session.
- **`/peers`** shows `relay through <neighbour>` for relayed sessions.

## Not yet done

- Store-and-forward for offline peers. This needs an asynchronous first message, using published hybrid prekeys in place of the interactive handshake.
- Onion-encrypted circuit setup, so relays can't see both ends.
- Bandwidth-aware or shortest-path route selection. The current search is depth-first in session order.
