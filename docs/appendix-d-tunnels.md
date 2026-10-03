# Appendix D: WireGuard Full-Mesh Tunnels (v1)

Status: implemented in `threnody-core::tunnel`, `threnody-net::node` (offers and events) and `threnody-cli::tunnel` (wg-quick config, live `wg set`). Fulfils spec §8.

## Keys

```
wg_static   = clamp(KDF("wireguard static key", identity_seed))      -- stable per device
exporter    = KDF("exporter secret", ss, session_id)                 -- per Threnody session
psk(pair)   = KDF("exported key", exporter, "wireguard preshared key")
```

- **Static key.** The WireGuard static key is derived from the identity seed, so a device keeps the same WireGuard public key for its whole life and needs no extra secret storage. This matches spec §8.2's requirement that tunnel keys derive from identity material.
- **Preshared key.** The preshared key comes from the Threnody session's exporter, so it changes with every session. WireGuard mixes the PSK into every handshake. An attacker therefore has to break both WireGuard's X25519 and the Threnody session's X-Wing KEM (X25519 and ML-KEM-768) to decrypt tunnel traffic. In other words, the tunnels are post-quantum hybrid, the same approach Rosenpass takes. WireGuard's own 2-minute rekeying still provides classical forward secrecy within a session.

## Addresses

```
prefix  = fd || KDF("overlay ula prefix")[0..5]          -- one /48 shared by all Threnody devices
addr(B) = prefix || KDF("overlay address", id_B)[0..10]  -- /128 per device
```

Addresses are unique ULAs derived from identity keys, so no allocator or coordination is needed. With 80 bits per device, a collision is negligible. Each peer's `AllowedIPs` is exactly its own `/128`, which means a peer can never send traffic as any other address.

## Protocol

1. A session starts. Once the contact is **mutually approved** (spec §5.2) and tunnels are enabled, each side sends `TunnelOffer { wg_public, port }` once per session (Appendix C, kind 5).
2. When a node receives an offer, it checks mutual approval again. If approval holds, it configures the peer with:
   - public key `wg_public`,
   - endpoint `(session source IP, port)`,
   - `AllowedIPs = addr(peer)/128`,
   - PSK `psk(pair)`,
   - keepalive 25 s.
   An offer from a peer that is not approved is ignored.
3. When either side revokes, the approval exchange makes mutual approval false. Both sides then remove the peer (`TunnelDown`).

Offers travel inside the authenticated ratchet, so only the genuine peer can set its tunnel key and endpoint. Each new session refreshes the PSK.

## Operation

- `threnody run --tunnel [PORT]` keeps `<home>/wireguard/thr0.conf` (mode 0600) up to date. Bring the interface up with `sudo wg-quick up <file>`.
- Adding `--wg-apply` also pushes every change to the running interface with `wg set`, with the PSK passed on stdin. This needs CAP_NET_ADMIN.
- `threnody tunnel` shows this device's WireGuard public key, overlay address and overlay network.

## Not yet done

- A userspace data plane (`boringtun`) for platforms without kernel WireGuard (iOS, Android, unprivileged desktops).
- Endpoint updates when a peer roams without a new session. WireGuard's own roaming covers most of these cases.
- Relaying tunnel traffic through mesh nodes when no direct UDP path exists (Appendix E, future).
- Tunnel peers are kept only while the node runs. After a restart, tunnels come back with the next session to each peer.
