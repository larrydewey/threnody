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

- `threnody run --tunnel [PORT]` (port 51820 by default) keeps `<home>/wireguard/<iface>.conf` (mode 0600) up to date, the interface being `thr0` unless `--wg-iface` names another. Bring the interface up with `sudo wg-quick up <file>`.
- Adding `--wg-apply` also pushes every change to the running interface with `wg set`, with the PSK passed on stdin. This needs CAP_NET_ADMIN.
- `threnody tunnel` shows this device's WireGuard public key, overlay address and overlay network.
- The Linux app has the same as a *WireGuard tunnels* setting (off by default, main identity only). It keeps `<data dir>/wireguard/thr0.conf` current on port 51820, for `wg-quick` as above.

### Userspace WireGuard (Android)

Android gives apps no kernel WireGuard, so the node carries tunnels itself with `boringtun` (`threnody-net::wg_userspace`). The app's *WireGuard tunnels* setting (Network & Connectivity, off by default) asks for Android's VPN permission. It then opens a VPN interface with:

- the device's overlay address (`/128`);
- a route for the overlay `/48` only, so no other traffic, IPv4 included, goes through it;
- an MTU of 1280.

The node reads and writes whole IPv6 packets on that interface and WireGuard datagrams on the tunnel port's UDP socket (dual-stack):

- A packet to the overlay goes to the peer that owns its destination address.
- A decrypted packet is delivered only if its source is the sending peer's own overlay address, the same rule as `AllowedIPs`.
- Peers follow the node's tunnel state, checked every second. A new session's PSK starts a fresh WireGuard session, and revoking approval removes the peer.
- Authenticated packets update the peer's endpoint, as WireGuard roaming does.

Android runs one VPN at a time, so turning tunnels on displaces any other VPN app, which is why the setting is opt-in. If another VPN takes the slot, tunnels turn off.

The descriptor Android hands over is adopted in `threnody-tun`. It is the one crate in the workspace that allows `unsafe`, kept to the `fcntl`, `read` and `write` calls this needs.

Turning tunnels on mid-session offers them on every direct session with an approved contact at once. A node answers the first offer it accepts in a session with its own, so a contact who offered while this side's tunnels were off still pairs.

### Over the session

WireGuard's own UDP port is often unreachable. Mobile carriers' NATs map it to an outside port nobody can guess, and unlike the session's QUIC port, nothing punches a hole for it. Over LTE, a tunnel would come up and carry nothing. So the userspace device picks a path per packet:

- **Direct**, while an authenticated packet has come over the tunnel's UDP path in the last 35 seconds (the keepalive plus 10): everything goes over UDP.
- **Otherwise, the session.** Data packets go over the Threnody session with that peer: as QUIC datagrams on the session's hole-punched path when they fit, else in the session stream (`Tunnel`, Appendix C). Handshakes, cookie replies and keepalives go both ways, over the session and to the UDP endpoint, so a direct path that opens, or comes back, is found and takes over.

Packets that arrive over a session count only for the tunnel with that session's peer. WireGuard still authenticates every one, and the `AllowedIPs` rule still applies, so the session adds a path, not trust. Onion sessions never carry tunnel packets. Kernel WireGuard on desktops has no such fallback.

## Not yet done

- The userspace data plane on iOS (a Network Extension) and on unprivileged desktops, which still need a TUN device from somewhere.
- Endpoint updates when a peer roams without a new session. WireGuard's own roaming covers most of these cases.
- Tunnels with contacts reached only through relays: offers are made on direct sessions only, so a tunnel needs a direct session at least once. After that it rides relayed sessions too (*Over the session*).
- Tunnel peers are kept only while the node runs. After a restart, tunnels come back with the next session to each peer.
