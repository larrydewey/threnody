# Appendix I: Onion Circuits (v1)

Status: specified here, hop handshake modelled in [`proofs/onion.spthy`](../proofs/README.md), implemented in `threnody-core::onion` and `threnody-net::onion`. Fulfils spec §9 layer 2 ("onion / multi-hop routing through volunteer or user-run nodes").

Relay circuits (Appendix G) show every relay the destination. Onion circuits are built hop by hop instead, so each relay learns only the hop before it and the hop after it:

```
A ──▶ R1 ──▶ R2 ──▶ C
R1 knows A, R2       R2 knows R1, C       C knows R2 (and A, via the end-to-end handshake)
```

With two or more relays, no single relay knows both ends. With one relay that relay necessarily knows both ends; the client warns when that is the only path.

## Hop handshake (one-way authenticated, post-quantum hybrid)

```
A → Ri   CREATE  { e_pub }                                 fresh X-Wing key pair (e_sk, e_pub)
Ri → A   CREATED { id_Ri, ct, sig }
         (ct, ss) = Encap(e_pub)
         sig = Sign(id_Ri, L("onion created") || L(H(e_pub)) || L(H(ct)) || L(id_Ri))
A        checks fingerprint(id_Ri) is the hop it asked for, then verifies sig and decapsulates
both     kf || kb || df || db = KDF("onion layer keys", ss, e_pub, ct, id_Ri)   (4 × 32 bytes)
```

- **`Ri` is authenticated to A.** A forged `CREATED` needs `Ri`'s identity key.
- **A stays anonymous.** A's ephemeral key is fresh, and A signs nothing.
- **Forward secrecy.** The layer keys depend only on ephemeral secrets.
- **Post-quantum confidentiality.** `ss` comes from X-Wing.

For the first hop, `CREATE` and `CREATED` travel over the link session with R1. For every later hop they travel inside `EXTEND` and `EXTENDED` cells, encrypted for the previous hop only.

## Cells

A cell body is always **2048 bytes**. A layer is applied by XOR with a ChaCha20 keystream:

```
layer(k, n, body) = body XOR ChaCha20(key = k, nonce = le96(n))      n = per-hop, per-direction cell counter
plaintext = recognized (u16 = 0) || digest (4) || cmd (1) || len (u16 BE) || data || zero padding
digest    = BLAKE3-keyed(d, le64(n) || plaintext with digest zeroed)[0..4]
```

**Forward direction.** To address hop `t`, A builds the plaintext with `df_t` and counter `n_t`. It then applies `kf_t`, `kf_{t-1}`, …, `kf_1` in that order. Each hop `i` removes `kf_i`. If the result is recognized under `df_i`, the cell is for that hop. Otherwise the hop passes it on to the next hop.

**Backward direction.** Hop `i` builds a plaintext with `db_i` and applies `kb_i`. Every earlier hop adds its own `kb` layer. A removes `kb_1`, `kb_2`, … and checks after each one whether the cell is recognized, which tells it which hop sent the cell.

| cmd | Name | Data |
|---|---|---|
| 1 | EXTEND | `to` fingerprint (20) ‖ `e_pub` (1216), optionally followed by an address and a token (Appendix P) |
| 2 | EXTENDED | `id` (32) ‖ `ct` (1120) ‖ `sig` (64) |
| 3 | EXTEND_FAILED | — |
| 4 | BEGIN | — (the last hop is the destination and attaches the stream) |
| 5 | DATA | stream bytes (≤ 2039) |
| 6 | END | — |
| 7 | DEPOSIT | `to` (32) ‖ `total_len` (u32 BE) ‖ sealed bytes; the rest follows in DATA (Appendix H) |
| 8 | DEPOSITED | `status` (1): 0 declined, 1 held, 2 delivered now (older nodes; now reported as held, see Appendix H) |

The end-to-end Threnody handshake and session (Appendices A and B) run over the byte stream carried in DATA cells, so the destination authenticates A in the usual way.

## Link messages

These are carried as `AppMessage::Onion` (Appendix C, kind 10):

```
OnionMsg = { 0: op, 1: circ u64, ? 2: e_pub, ? 3: id, ? 4: ct, ? 5: sig, ? 6: cell (2048), ? 7: token }
op: 1 create, 2 created, 3 cell, 4 destroy
```

`token` appears only on `create`, paying a volunteer relay (Appendix P).

## Relay rules

- **Who may create.** A node answers `CREATE` from any neighbour, with at most 64 circuits per neighbour. Only a hop created by a mutually approved neighbour, or paid with a relay token (Appendix P), may `EXTEND`; any other hop may only end there (`BEGIN` or `DEPOSIT`).
- **Who to extend to.** On `EXTEND` on a hop a mutually approved neighbour created, a node checks for a live direct session with a mutually approved contact whose fingerprint is `to`. If there is one, it sends that contact `CREATE` on a new link circuit and returns its `CREATED` fields as `EXTENDED`. A volunteer relay may instead dial the address the `EXTEND` carries (Appendix P); a hop paid with a token goes on only that way, never to the relay's contacts. Otherwise it returns `EXTEND_FAILED`.
- **Passing cells on.** Cells that aren't recognized are forwarded on the paired link circuit. Cells from the next hop get this hop's backward layer and are forwarded upstream.
- **Teardown.** `DESTROY`, or losing either link, tears the circuit down in both directions, including a next hop still being made: one whose `CREATED` arrives after the circuit is gone gets `DESTROY`. A hop takes one `EXTEND` at a time. A hop that has neither extended nor begun a stream 120 s after its `CREATE` is torn down too (a deposit still arriving then is declined).

## When circuits are used

Nodes reach contacts through onion circuits first, by default (`Node::reach`, the CLI and the apps). That happens whenever a two-relay path could exist: a live, mutually approved neighbour, plus another approved contact. If building a circuit fails or takes over 10 s, the node tries volunteer relays when its subscribed directories offer a route and an address is known (Appendix P, for up to 60 s), then falls back to a direct dial, then to a relay circuit (Appendix G). `--no-onion`, or the app's toggle, turns this off. `/onion` still builds a circuit on demand.

### Background reconnection

Reconnections the node starts by itself go onion first too: after a network change (the standby dials and the punches toward contacts we moved away from, Appendix N §5), recovery of a lost session, DHT rendezvous and hole punching, LAN discovery (Appendix E), and the apps' reconnect on start and when the network comes back (`Node::reconnect_peer`, the FFI's `reconnect`). With onion routing on, each of them reaches a mutually approved contact through a two-relay circuit whenever one can be built, and dials directly only when it can't.

- **When a circuit can be built.** It needs two other mutually approved contacts and a first hop: a live direct session with one of them that began, or brought in a frame, since the network last changed, or a radio link, which a network change leaves alone. A session from before the change that has been silent since may already be dead without the node knowing, and a circuit through it would only time out, so the circuit starts only on a first hop that passes this test, the one heard from most recently first. A healthy session passes again with its next frame (a heartbeat comes every 10 s at most), so a signal for a change that wasn't one costs little.
- **The first hop.** With none (just after start-up, or just after a network change), a reconnection that gets a turn dials the contact directly so that the others get one, straight to its last known address. Up to 2 turns run at once, the second at least 1 s after the first: a contact that answers by then is the first hop, and no second contact is dialed in the open. A turn ends with its dial, or after 5 s, so an unreachable contact doesn't hold up the rest. The others wait and go through a circuit as soon as a first hop is up. If none is up after 30 s, they dial directly too. LAN discovery never waits for a turn (see below).
- **Falling back.** If the circuit fails or takes over 10 s, the reconnection goes on as it would have without onion routing (punching, the standby hole, the LAN address, a relay circuit). Since no message is involved, the node says so in a diagnostic `ReachNote`, not a `Fallback`.
- **Nothing waits** when no circuit could ever be built (fewer than two approved contacts besides this one), with onion routing off, or for someone met through a handle who isn't mutually approved yet: those reconnections go direct at once.
- **LAN contacts.** A contact found on the LAN goes through a circuit too when one can be built. Otherwise anyone watching a shared network (a café's Wi-Fi) would see the two devices talk. A circuit never uses its destination as its own first hop, so a LAN contact that is the only possible first hop is dialed directly, at once, and its session then carries the circuits to everyone else. It doesn't wait behind reconnections over the internet for a turn: it is the quickest first hop there is. Bluetooth and Wi-Fi Direct links stay direct. They are the local fallback for when there is no network, and a circuit through the internet can't replace them.
- **Stays direct.** The always-on relay (Appendix N, `set_anchor`) is dialed directly by design: it exists to be a first hop and to relay. So is the direct path a call asks for when it runs over a relay circuit (Appendix Q), since its media needs datagrams. And a contact in a call with us now is reconnected as before, without waiting for a first hop or a circuit: the call needs its path back at once. A call that starts later reaches its contact onion first anyway (`reach_for_call`).
- **Twins.** Both sides may build a circuit to each other at once. As with two direct dials (Appendix E), two onion sessions that start within 5 s of each other in opposite directions keep the one the smaller identity key dialed.
- **Costs.** Which contacts end up direct depends on which come back first: after start-up or a network change usually one, two when the first is slow to answer, and more when contacts are unreachable. A reconnection may wait a second or so for a first hop, and when the approved relays can't reach the contact, it spends up to 10 s on the circuit before dialing. Sessions through circuits have more latency and less bandwidth for files and calls. They carry no typing indicators, and since WireGuard tunnels (spec §8) are offered over direct sessions only, a contact reached through a circuit gets no tunnel until a direct session comes up. Anyone who wants the old behaviour can turn onion routing off.

### Fallback

Falling back is a privacy downgrade: a direct dial shows the contact our address and shows anyone watching our network whom we talk to, and a relay circuit's relays know both ends. An attacker can force it, for example as the only live first hop dropping our cells. So when onion routing is on and a contact (or an invite's maker) is reached in the open after hidden paths failed or weren't possible, the node emits `Event::Fallback { peer, path }`, with `path` `Direct` or `Relayed`. The same goes for mailbox deposits (Appendix H): each deposit left with a mailbox directly gives `Fallback` with `path` `Deposit { mailbox }`. With onion routing off, going direct is the user's choice and nothing is emitted. The FFI passes it on as `NodeEvent::Fallback { peer, notice }`, with a short notice such as "Sent directly: no relay circuit was available". The Android and Linux apps show it quietly in the chat with that contact, as a dismissable line above the messages, until it is dismissed. Notices are kept in memory only. The CLI prints them. There is no strict mode that refuses to fall back.

## Path selection

A tries paths in this order:

- **Two relays:** each live, mutually approved neighbour `R1` paired with each other mutually approved contact `R2`, ending at the destination.
- **One relay**, but only if allowed (`/onion <peer> 1`): an `R1` that can extend straight to the destination.

If any extension fails, A destroys the circuit and tries the next path.

## Known limits

- **Unauthenticated layers.** As in Tor's original design, the per-hop layers aren't individually authenticated. Tampering is detected end to end, because the inner session is AEAD. But colluding first and last hops could tag cells to confirm they are on the same circuit, and such hops can already link them by timing.
- **No cover traffic inside circuits.** Link-level constant-rate mode (§9 layer 1) still applies to each hop, and pads every frame of the link to at least an onion cell's size (Appendix C, *Padding*), so a link's ticks look the same whether they carry a cell or cover, when both ends pad this way.
- **Contacts-only relays.** Without directories, A can only use relays that are its own contacts. Volunteer relays from subscribed directories (Appendix P) lift this.
