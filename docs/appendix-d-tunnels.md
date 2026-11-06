# Appendix D: WireGuard Full-Mesh Tunnels (v1)

Status: implemented in `threnody-core::tunnel`, `threnody-net::node` (offers and events), `threnody-net::tunnels` (persistence) and `threnody-cli::tunnel` (wg-quick config, live `wg set`). Fulfils spec §8.

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
   - endpoint `(session source IP, port)`. A Bluetooth link has no source IP: an offer over one keeps the IP an earlier session gave, or else leaves the endpoint out, and WireGuard learns it from the peer's authenticated packets (on Android, packets meanwhile go over the session),
   - `AllowedIPs = addr(peer)/128`,
   - PSK `psk(pair)`,
   - keepalive 25 s.
   An offer from a peer that is not approved is ignored, as is one over a relayed or onion session, whose source address is the relay's.

   A node MAY send `TunnelOffer` again within a session, after its network or port changes. The receiver treats a repeat as an update: same PSK; the endpoint is the session's current remote IP (a QUIC session's migrated address) with the offered port. An identical repeat changes nothing. Only sessions we accepted see the peer migrate: QUIC servers don't move, so a session we dialed dies when the peer moves, and the next session brings a new offer and PSK anyway.
3. When either side revokes, the approval exchange makes mutual approval false. Both sides then remove the peer (`TunnelDown`). Blocking a contact, or deleting its conversation, withdraws approval the same way: the peer goes from our side at once, and the session sends `Approval { approved: false }` as its last message, so the peer removes ours. A device leaving its account, or any of these done on another of our devices (which arrives through contact sync), removes the peer too.

Offers travel inside the authenticated ratchet, so only the genuine peer can set its tunnel key and endpoint. Each new session refreshes the PSK. A peer's WireGuard key is not secret, though: every mutual contact of its owner has it. WireGuard knows peers by key, so a node refuses an offer whose key is its own or already belongs to another peer. Otherwise one contact could offer another's key, take over that contact's peer entry, and remove it entirely once revoked. The first peer to hold a key keeps it.

## Operation

- `threnody run --tunnel [PORT]` (port 51820 by default) keeps `<home>/wireguard/<iface>.conf` (mode 0600) up to date, the interface being `thr0` unless `--wg-iface` names another. Bring the interface up with `sudo wg-quick up <file>`.
- Adding `--wg-apply` also pushes every change to the running interface with `wg set`, with the PSK passed on stdin. This needs CAP_NET_ADMIN.
- `threnody tunnel` shows this device's WireGuard public key, overlay address and overlay network.
- The Linux app has the same as a *WireGuard tunnels* setting (off by default, main identity only). It keeps `<data dir>/wireguard/thr0.conf` current on port 51820, for `wg-quick` as above. After a restart the file holds the restored peers at once (*Persistence*), so `wg-quick up` works before any session; with the setting off, the app deletes it.
- The CLI writes restored peers into its config at start too (`* N tunnel(s) restored from the last run`), and with `--wg-apply` also `wg set`s them; the config gets them even when that fails because the interface isn't up yet.

### Userspace WireGuard (Android)

Android gives apps no kernel WireGuard, so the node carries tunnels itself with `boringtun` (`threnody-net::wg_userspace`). The app's *WireGuard tunnels* setting (Network & Connectivity, off by default) asks for Android's VPN permission. It then opens a VPN interface with:

- the device's overlay address (`/128`);
- a route for the overlay `/48` only, so no other traffic, IPv4 included, goes through it;
- an MTU of 1280.

The node reads and writes whole IPv6 packets on that interface and WireGuard datagrams on the tunnel port's UDP socket (dual-stack):

- A packet to the overlay goes to the peer that owns its destination address.
- A decrypted packet is delivered only if its source is the sending peer's own overlay address, the same rule as `AllowedIPs`.
- Peers follow the node's tunnel state, checked every second. A new session's PSK starts a fresh WireGuard session, and revoking approval removes the peer. Peers restored at start (*Persistence*) start a WireGuard session at once.
- Packets authenticated by the peer's keys (handshakes and data, not cookie replies, which someone on the path who knows the peer's public key could forge) update the peer's endpoint, as WireGuard roaming does.

Android runs one VPN at a time, so turning tunnels on displaces any other VPN app, which is why the setting is opt-in. If another VPN takes the slot, tunnels turn off.

The descriptor Android hands over is adopted in `threnody-tun`. It is the one crate in the workspace that allows `unsafe`, kept to the `fcntl`, `read` and `write` calls this needs.

Turning tunnels on mid-session offers them on every direct session with an approved contact at once. A node answers the first offer it accepts in a session with its own, so a contact who offered while this side's tunnels were off still pairs. Changing the port, or a change of network, offers again the same way; peers are kept.

### Persistence

While tunnels are on, the node keeps its tunnel peers in the encrypted state file `<home>/tunnels.state`, written by `Home::save_state`:

```text
file      = nonce (12) || ChaCha20-Poly1305(KDF("state encryption key", seed), nonce, ad = "tunnels", plaintext)
plaintext = { 0 => 1,                         ; version
              1 => [* SavedPeer] }            ; at most 4096 read
SavedPeer = { 0 => bstr .size 32,             ; peer identity key
              1 => bstr .size 32,             ; peer WireGuard public key
              2 => bstr .size 16,             ; endpoint IP (IPv6; IPv4 as ::ffff:a.b.c.d)
              3 => uint,                      ; endpoint UDP port
              4 => bstr .size 32,             ; PSK (session export, "wireguard preshared key")
              5 => uint }                     ; ms since the epoch the PSK was exported
; unknown keys ignored (forward compatibility)
```

When tunnels turn on (at start, or later), the node brings the saved peers back, so a contact's overlay address answers before, or without, a new session, as long as the contact still holds the same PSK. That includes contacts reached only through relays: a restored tunnel rides the relayed session (*Over the session*). A first tunnel with a contact still needs a direct session. The overlay address and our own WireGuard key are derived, not stored. The endpoint saved is the last one the peer's authenticated packets came from (userspace device), else the offered one.

- **Kept.** Only peers of contacts that are mutually approved now, whose PSK is at most 7 days old. A new session replaces the PSK on both sides; an older one is neither restored nor written back.
- **Written** when an offer brings a new key, PSK or endpoint, when a peer goes, after a restore (which prunes the file), and when the userspace device learns a new endpoint (at most every 30 s).
- **Pruned.** A peer that stops being mutually approved for any reason (revoked, blocked, deleted, a device that left its account, or any of these on another of our devices) leaves memory and disk at once.
- **Deleted**, overwritten first, when tunnels turn off (including when the Android and Linux apps find them off at start), when the last peer goes, and when the file can't be opened or decoded. Only the current file is overwritten: earlier versions were replaced by a rename, so their blocks were only unlinked, still encrypted.
- **Never** in backups (Appendix C, *Backups*), in anonymous identities' homes, or synced to our other devices.

The PSK is the post-quantum half of the tunnel, and until now it lived only in memory. Using a saved one takes the device's storage, the identity seed (sealed by a passphrase or the keyring where the user chose one) and a quantum computer for the recorded X25519 handshakes: the protection other session exports kept at rest (recovery slots, discovery keys) already have. There is no separate switch: persistence happens only while the user's tunnels are on, and turning them off deletes it.

A restored device sends WireGuard handshake initiations to each peer's last endpoint right after start, where before it waited for a session. An endpoint gone stale (DHCP or CGNAT reuse) gets initiations it can't read: the initiator's static key is encrypted to the responder, and MAC1 is keyed by the responder's WireGuard key, which only approved peers know.

### Over the session

WireGuard's own UDP port is often unreachable. Mobile carriers' NATs map it to an outside port nobody can guess, and unlike the session's QUIC port, nothing punches a hole for it. Over LTE, a tunnel would come up and carry nothing. So the userspace device picks a path per packet:

- **Direct**, while an authenticated packet has come over the tunnel's UDP path in the last 35 seconds (the keepalive plus 10): everything goes over UDP.
- **Otherwise, the session.** Data packets go over the Threnody session with that peer: as QUIC datagrams on the session's hole-punched path when they fit, else in the session stream (`Tunnel`, Appendix C). Handshakes, cookie replies and keepalives go both ways, over the session and to the UDP endpoint, so a direct path that opens, or comes back, is found and takes over.

Packets that arrive over a session count only for the tunnel with that session's peer. WireGuard still authenticates every one, and the `AllowedIPs` rule still applies, so the session adds a path, not trust. Onion sessions never carry tunnel packets. Kernel WireGuard on desktops has no such fallback.

## Not yet done

- The userspace data plane on iOS (a Network Extension) and on unprivileged desktops, which still need a TUN device from somewhere.
- Kernel WireGuard's roamed endpoints are not read back, so the saved endpoint for a kernel tunnel is the offered one.
- Tunnels with contacts reached only through relays: offers are made on direct sessions only, so a tunnel needs a direct session at least once. It is then kept across restarts and rides relayed sessions too (*Over the session*).
- Turning tunnels off and on within one session leaves the toggling side without the peer's tunnel until the next session: the peer has already taken an offer this session, and doesn't answer another.
