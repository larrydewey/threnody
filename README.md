# Threnody

Threnody is an encrypted, metadata-resistant messaging protocol with post-quantum hybrid cryptography. This repository holds the Rust reference implementation of the [Threnody Protocol Specification](Threnody-Specification.md).

**Status: milestone 6.** Two devices can connect over IP and authenticate with an X-Wing (X25519 + ML-KEM-768) handshake. They can then chat and send files over a post-quantum double ratchet with encrypted headers, and mutually approve each other. Mutually approved devices find each other automatically on the local network through private beacons, and they get post-quantum-hybrid WireGuard tunnels. MLS groups use an X-Wing ciphersuite and need no server. Approved nodes relay end-to-end sessions over up to three hops. Messages to offline contacts are sealed to their prekeys and held by mutual contacts until the recipient returns. Onion circuits through two or more relays keep any single relay from seeing both ends. Several devices can share one account, linked with a one-time code; any device can add or remove others. The KEM matches the official X-Wing test vectors, the handshake and ratchet are machine-checked in Tamarin, every parser is mutation-tested, and identity keys can be protected with a passphrase. Bluetooth / Wi-Fi Direct and mobile are still to come (see [Roadmap](#roadmap)).

> ⚠️ Not audited. Do not rely on it for real-world safety yet.

## Quick start

```sh
cargo build --release
alias threnody=target/release/threnody

# Device B
threnody --home ~/.thr-b init
threnody --home ~/.thr-b invite 192.0.2.7:7450       # prints a link + QR code
threnody --home ~/.thr-b run --listen 0.0.0.0:7450

# Device A
threnody --home ~/.thr-a init
threnody --home ~/.thr-a run --no-listen -c 'threnody://<fingerprint>@192.0.2.7:7450'
```

Inside `run`, any line you type goes to the current peer. The available commands are below. Groups use `/group new|invite|accept|remove`, `/groups` and `/g <group> <text>`. `/relay <contact|fingerprint|invite>` reaches a peer through approved relays, and `/connect` falls back to relays when a direct dial fails. Text sent to a contact who isn't connected is sealed and left with mutual contacts, who deliver it when that contact returns. `/onion <peer> [min-relays]` builds an onion circuit (two relays by default) so no single relay learns both ends.

To add a device to your account, run `/device add` on a device you already have. It prints a one-time code (and a QR code). Then run `threnody link '<code>'` on the new device. `/devices` lists your devices and `/device remove <name>` revokes one. Your contacts see device changes, and refuse removed devices. Messages to a contact go to all of their devices.

```
/connect <invite|contact|host:port>   /to <peer>   /peers   /contacts
/name <peer> <name>   /approve [peer]   /revoke [peer]
/safety [peer]   /verify [peer]   /file <path>   /drop [peer]
/policy anyone|contacts|approved   /status   /quit
```

You can also manage contacts outside a session with `threnody contacts | name | approve | revoke | verify | forget`.

`--constant-rate-ms N` sends one padded frame every N ms on each session, filling idle slots with cover traffic. `--policy approved` only accepts mutually approved contacts.

While listening, nodes send private UDP beacons that only mutually approved peers can recognise, and reconnect to each other automatically. Use `--no-discover` to turn this off. See [Appendix E](docs/appendix-e-discovery.md).

`threnody init --passphrase` seals the identity key with Argon2id. `threnody passphrase` adds, changes or removes the passphrase. `$THRENODY_PASSPHRASE` supplies it non-interactively.

`--tunnel [PORT]` builds WireGuard tunnels to mutually approved peers. Each pair's preshared key comes from its Threnody session, which makes the tunnels post-quantum hybrid. The node keeps `<home>/wireguard/thr0.conf` current: bring the interface up with `sudo wg-quick up …`, then add `--wg-apply` to push changes live. See [Appendix D](docs/appendix-d-tunnels.md).

## Layout

| Crate | Contents |
|---|---|
| `threnody-core` | Sans-IO protocol: identity and fingerprints, the hybrid KEM, the handshake, the ratchet, the CBOR wire format (via [`const-cbor`](https://crates.io/crates/const-cbor)), padding, and the contact/identity store |
| `threnody-net` | Stream framing, the async handshake driver, and `Node` (sessions, trust policy, approval exchange, constant-rate mode). The TCP transport works today, and the session driver works over any byte stream. |
| `threnody-groups` | MLS groups on openmls with the X-Wing ciphersuite. Credentials are bound to Threnody identities, the group owner is the only committer, and messages travel as sans-IO fan-out over the 1:1 sessions |
| `threnody-cli` | The `threnody` binary |

The normative details that the spec deferred are written up in [`docs/`](docs/):

- [Appendix A: handshake](docs/appendix-a-handshake.md)
- [Appendix B: ratchet](docs/appendix-b-ratchet.md)
- [Appendix C: CBOR schemas](docs/appendix-c-wire-format.md)
- [Appendix D: WireGuard tunnels](docs/appendix-d-tunnels.md)
- [Appendix E: private LAN discovery](docs/appendix-e-discovery.md)
- [Appendix F: MLS groups](docs/appendix-f-groups.md)
- [Appendix G: relay circuits](docs/appendix-g-relay.md)
- [Appendix H: offline delivery](docs/appendix-h-offline.md)
- [Appendix I: onion circuits](docs/appendix-i-onion.md)
- [Appendix J: accounts and multiple devices](docs/appendix-j-devices.md)
- [Test vectors](docs/test-vectors/v1.txt), regenerated and checked by `cargo test`
- [Tamarin proofs of the handshake, ratchet, sealed messages, onion hops and device linking](proofs/README.md)

## Spec coverage

| Spec | Status |
|---|---|
| §3.2 Hybrid KEM X25519 + ML-KEM-768 | ✅ X-Wing (draft-11), passes the official vectors |
| §3.2 Ed25519, ChaCha20-Poly1305 + AES-256-GCM, domain-separated KDF | ✅ BLAKE3 KDF; both AEADs negotiated |
| §3.3 FS / PCS ratchet with hybrid PQ updates | ✅ KEM double ratchet with header encryption |
| §3.4 Formal verification | ✅ Tamarin, 33 lemmas: handshake, ratchet, sealed messages, onion hops, device linking and account chains; `proofs/check.sh` |
| §4.2 Pseudonymous identity, §4.4 fingerprint | ✅ 32-character Crockford Base32 |
| §4.1 Anonymous mode, §4.3 selective disclosure | ❌ |
| §4.5 Multi-device | ✅ signed device chains (equal peers, threshold-ready), link codes, revocation, own-device contact sync, account-wide offline keys and group invites; history not synced |
| §5 TOFU, out-of-band invites with pinned fingerprint, QR, safety numbers, mutual approval and revocation | ✅ (NFC, directories and web-of-trust not yet) |
| §6.1 1:1 text + files | ✅ (disappearing messages not yet) |
| §6.2 MLS groups | ✅ openmls with the X-Wing ciphersuite; owner-administered; encrypted persistence |
| §6.3 / §11 CBOR, versioning, unknown-field tolerance | ✅ |
| §6.4 Local-first store | ✅ identity and contacts (message history, sync and backup not yet) |
| §7 Transports | ✅ TCP/IP, private LAN discovery with auto-connect, multi-hop relay circuits (≤ 3 relays), store-and-forward mailboxes; ❌ BLE, Wi-Fi Direct |
| §8 WireGuard full-mesh tunnels | ✅ kernel WireGuard; PQ PSK from the session; gated on mutual approval |
| §9 Metadata layers | ✅ padding, constant-rate + cover, onion circuits (≥ 2 relays, fixed-size cells); ❌ volunteer relay directories, local-first preference |
| §10 Status indicators | ✅ `/status` |
| §3.1 Platform keystore | software fallback: 0600 files plus optional Argon2id passphrase |
| §15 Test vectors | ✅ `docs/test-vectors/v1.txt` |

## Roadmap

1. **Hardening (remaining).** OS keystores, ratchet persistence across reconnects, and an external audit.
2. **Groups (remaining).** Store-and-forward via members, and more admin roles.
3. **Tunnels (remaining).** A `boringtun` data plane for mobile and unprivileged use.
4. **Mesh (remaining).** BLE / Wi-Fi Direct transports with the same beacon scheme.
5. **Metadata (remaining).** Volunteer relay directories beyond your own contacts, onion-routed mailbox deposits, and anonymous and selective-disclosure identities.
6. **Mobile.** UniFFI bindings for iOS and Android. The core is sans-IO, so this is mostly glue code.

## Development

```sh
cargo test                    # unit, vector, mutation and TCP integration tests
cargo clippy --all-targets
cargo +nightly fuzz run envelope   # see fuzz/README.md
```

## License

Apache-2.0 OR MIT.
