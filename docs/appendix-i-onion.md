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
| 8 | DEPOSITED | `status` (1): 0 declined, 1 held, 2 delivered now |

The end-to-end Threnody handshake and session (Appendices A and B) run over the byte stream carried in DATA cells, so the destination authenticates A in the usual way.

## Link messages

These are carried as `AppMessage::Onion` (Appendix C, kind 10):

```
OnionMsg = { 0: op, 1: circ u64, ? 2: e_pub, ? 3: id, ? 4: ct, ? 5: sig, ? 6: cell (2048) }
op: 1 create, 2 created, 3 cell, 4 destroy
```

## Relay rules

- **Who may create.** A node answers `CREATE` from any neighbour, with at most 64 circuits per neighbour. Only a hop created by a mutually approved neighbour, or paid with a relay token (Appendix P), may `EXTEND`; any other hop may only end there (`BEGIN` or `DEPOSIT`).
- **Who to extend to.** On `EXTEND`, a node checks for a live direct session with a mutually approved contact whose fingerprint is `to`. If there is one, it sends that contact `CREATE` on a new link circuit and returns its `CREATED` fields as `EXTENDED`. Otherwise it returns `EXTEND_FAILED`.
- **Passing cells on.** Cells that aren't recognized are forwarded on the paired link circuit. Cells from the next hop get this hop's backward layer and are forwarded upstream.
- **Teardown.** `DESTROY`, or losing either link, tears the circuit down in both directions.

## When circuits are used

Nodes reach contacts through onion circuits first, by default (`Node::reach`, the CLI and the apps). That happens whenever a two-relay path could exist: a live, mutually approved neighbour, plus another approved contact. If building a circuit fails or takes over 10 s, the node falls back to a direct dial, then to a relay circuit (Appendix G). `--no-onion`, or the app's toggle, turns this off. `/onion` still builds a circuit on demand.

## Path selection

A tries paths in this order:

- **Two relays:** each live, mutually approved neighbour `R1` paired with each other mutually approved contact `R2`, ending at the destination.
- **One relay**, but only if allowed (`--min-relays 1`): an `R1` that can extend straight to the destination.

If any extension fails, A destroys the circuit and tries the next path.

## Known limits

- **Unauthenticated layers.** As in Tor's original design, the per-hop layers aren't individually authenticated. Tampering is detected end to end, because the inner session is AEAD. But colluding first and last hops could tag cells to confirm they are on the same circuit, and such hops can already link them by timing.
- **No cover traffic inside circuits.** Link-level constant-rate mode (§9 layer 1) still applies to each hop.
- **Contacts-only relays.** Without directories, A can only use relays that are its own contacts. Volunteer relays from subscribed directories (Appendix P) lift this.
