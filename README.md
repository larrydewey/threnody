# Threnody

Threnody is an encrypted, metadata-resistant messaging protocol with post-quantum hybrid cryptography. This repository holds the Rust reference implementation of the [Threnody Protocol Specification](Threnody-Specification.md).

**Status: milestone 6.** Two devices can connect over IP and authenticate with an X-Wing (X25519 + ML-KEM-768) handshake. They can then chat and send files over a post-quantum double ratchet with encrypted headers, and mutually approve each other. Mutually approved devices find each other automatically on the local network through private beacons, and they get post-quantum-hybrid WireGuard tunnels. MLS groups use an X-Wing ciphersuite and need no server. Approved nodes relay end-to-end sessions over up to three hops. Messages to offline contacts are sealed to their prekeys and held by mutual contacts until the recipient returns. Onion circuits through two or more relays keep any single relay from seeing both ends. Several devices can share one account, linked with a one-time code; any device can add or remove others. The KEM matches the official X-Wing test vectors, the handshake and ratchet are machine-checked in Tamarin, every parser is mutation-tested, and identity keys can be protected with a passphrase. Apps can embed a node through generated Kotlin, Swift and Python bindings. Over Bluetooth LE, approved contacts recognise each other's private beacons and connect without any taps. A Pixel 8a and a laptop have chatted this way, and the phone keeps its sessions while in the background. Relays work across transports, so a phone with only a Bluetooth link can reach peers that its contacts reach over IP. Sessions over Bluetooth can be upgraded to Wi-Fi Direct for bandwidth.

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

Inside `run`, any line you type goes to the current peer. The available commands are below. Groups use `/group new|invite|accept|decline|remove`, `/groups` and `/g <group> <text>`. `/relay <contact|fingerprint|invite>` reaches a peer through approved relays, and `/connect` falls back to relays when a direct dial fails. Text sent to a contact who isn't connected is sealed and left with mutual contacts, who deliver it when that contact returns. `/ble scan` finds nearby Threnody devices over Bluetooth LE, and `/ble connect <n>` opens a session over the radio (Linux, BlueZ). `run --ble` makes a Linux machine advertise a private beacon, accept Bluetooth sessions, and connect to approved contacts it hears. The Android sample app advertises after you tap *Start Bluetooth*, and it can scan and dial. With `run --wifi-direct`, `/wifi-direct request` asks the current (nearby, approved) peer to host a Wi-Fi Direct group, and the laptop joins it through NetworkManager. The session then moves to the faster link.

`/onion <peer> [min-relays]` builds an onion circuit (two relays by default) so no single relay learns both ends.

`/history [peer] [n]` shows recent messages, numbered and stored encrypted. `/del <n>` deletes message *n* of that list on all your devices, and `/del <n> all` deletes one of your own messages for everyone too. Messages disappear after a week by default. `/disappear 1h` (or `30s`, `10m`, `1d`, `1w`, `off`) sets the timer for the current chat, and the peer adopts the same timer. `--disappear-default <time|off>` changes the default for chats that haven't chosen one.

To add a device to your account, run `/device add [host:port]` on a device you already have. It prints a one-time code (and a QR code) with this machine's LAN address, or the one given. Then run `threnody link '<code>'` on the new device. `/devices` lists your devices, `/device rename <name> <new name>` renames one, and `/device remove <name>` revokes one. Your contacts see device changes, and refuse removed devices. Messages to a contact go to all of their devices. Your devices share your message history, and a new device gets it all when linked.

```
/connect <invite|contact|host:port>   /to <peer>   /peers   /contacts
/name <peer> <name>   /approve [peer]   /revoke [peer]
/safety [peer]   /verify [peer]   /file <path>   /drop [peer]
/policy anyone|contacts|approved   /status   /quit
```

**Message requests.** Messages from someone you haven't accepted are kept apart as requests. You accept someone by approving them, dialing their invite, or writing to them. `/requests` lists them, `/accept <peer>` takes them in, `/block <peer>` refuses that sender from then on and deletes the conversation, and `/delete <peer>` discards the request.

You can also manage contacts outside a session with `threnody contacts | name | approve | revoke | verify | forget`.

**Metadata protection is on by default.** Every message is padded. Each session also sends one padded frame every 2 s, with cover traffic in the gaps, so an observer can't tell when you send. Each frame is about 2.7 kB, so that's roughly 230 MB a day per connected contact, both ways. Contacts are also reached through two-relay onion circuits first, whenever approved relays make one possible. `--constant-rate-ms N` changes the interval, `--no-cover` turns cover traffic off, and `--no-onion` dials directly. `--policy approved` only accepts mutually approved contacts.

While listening, nodes send private UDP beacons that only mutually approved peers can recognise, and reconnect to each other automatically. Use `--no-discover` to turn this off. See [Appendix E](docs/appendix-e-discovery.md).

`threnody init` seals the identity key (Argon2id) with a random key kept in the system keyring: the Secret Service on Linux (GNOME Keyring, KWallet, KeePassXC), the Keychain on macOS, or the Credential Manager on Windows. A copy of the data directory is then useless without your unlocked keyring. Without a keyring (say, on a headless server), or with `--no-keyring`, the key is stored unprotected. `threnody keyring on|off|status` moves an existing identity into or out of the keyring. Alternatively, `threnody init --passphrase` seals it with a passphrase you type, and `threnody passphrase` adds, changes or removes it. `$THRENODY_PASSPHRASE` supplies it non-interactively.

`--tunnel [PORT]` builds WireGuard tunnels to mutually approved peers. Each pair's preshared key comes from its Threnody session, which makes the tunnels post-quantum hybrid. The node keeps `<home>/wireguard/thr0.conf` current: bring the interface up with `sudo wg-quick up …`, then add `--wg-apply` to push changes live. See [Appendix D](docs/appendix-d-tunnels.md).

## Layout

| Crate | Contents |
|---|---|
| `threnody-core` | Sans-IO protocol: identity and fingerprints, the hybrid KEM, the handshake, the ratchet, the CBOR wire format (via [`const-cbor`](https://crates.io/crates/const-cbor)), padding, and the contact/identity store |
| `threnody-net` | Stream framing, the async handshake driver, and `Node` (sessions, trust policy, approval exchange, constant-rate mode). The TCP transport works today, and the session driver works over any byte stream. |
| `threnody-groups` | MLS groups on openmls with the X-Wing ciphersuite. Credentials are bound to Threnody identities, the group owner is the only committer, and messages travel as sans-IO fan-out over the 1:1 sessions |
| `threnody-ffi` | UniFFI bindings (Kotlin, Swift, Python) for embedding a node in apps; see [its README](crates/threnody-ffi/README.md) |
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
- [Appendix K: Bluetooth LE transport](docs/appendix-k-bluetooth.md)
- [Appendix L: Wi-Fi Direct link upgrade](docs/appendix-l-wifi-direct.md)
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
| §4.5 Multi-device | ✅ signed device chains (equal peers, threshold-ready), link codes, revocation, own-device contact sync and history sync (with a backfill for newly linked devices), account-wide offline keys and group invites |
| §5 TOFU, out-of-band invites with pinned fingerprint, QR, safety numbers, mutual approval and revocation | ✅ (NFC, directories and web-of-trust not yet) |
| §6.1 1:1 text + files | ✅ including disappearing messages (the timer travels with each message); end-to-end acknowledgements, with resending after a dropped session or a restart |
| §6.2 MLS groups | ✅ openmls with the X-Wing ciphersuite; owner-administered; encrypted persistence; members forward and hold messages for members who can't be reached directly |
| §6.3 / §11 CBOR, versioning, unknown-field tolerance | ✅ |
| §6.4 Local-first store | ✅ encrypted identity, contacts, groups and message history, including file transfers, synced across an account's devices (backups not yet) |
| §7 Transports | ✅ TCP/IP, Bluetooth LE (L2CAP, tested phone ↔ laptop), private LAN discovery with auto-connect, multi-hop relay circuits (≤ 3 relays), store-and-forward mailboxes, Wi-Fi Direct upgrade (Android hosts or joins; Linux joins) |
| §8 WireGuard full-mesh tunnels | ✅ kernel WireGuard; PQ PSK from the session; gated on mutual approval |
| §9 Metadata layers | ✅ on by default: padding, constant-rate cover traffic, onion-first routing (≥ 2 relays, fixed-size cells), each only off when switched off; ❌ volunteer relay directories, onion-routed mailbox deposits |
| §10 Status indicators | ✅ `/status` |
| §3.1 Platform keystore | ✅ Android: the identity is sealed under an Android Keystore key (StrongBox or TEE). Desktop: sealed with a key in the system keyring by default, or a passphrase; 0600 files without either |
| §15 Test vectors | ✅ `docs/test-vectors/v1.txt` |

## Roadmap

1. **Hardening (remaining).** An iOS keystore, ratchet persistence across reconnects, and an external audit.
2. **Groups (remaining).** More admin roles and self-removal, and files in groups.
3. **Tunnels (remaining).** A `boringtun` data plane for mobile and unprivileged use.
4. **Mesh (remaining).** Upgrading to Wi-Fi Direct automatically for large transfers, and a phone-to-phone Wi-Fi Direct test.
5. **Metadata (remaining).** Volunteer relay directories beyond your own contacts, onion-routed mailbox deposits, and anonymous and selective-disclosure identities.
6. **Mobile (remaining).** An Android app is in [`apps/android`](apps/android), with conversations, chat, files, groups, invites by QR code and link, approval and safety numbers. Its transports have been tested on a Pixel 8a, and it keeps sessions alive in the background with a foreground service. Still to do: an iOS sample, and push-style wake-ups for when the process is gone.

## Development

```sh
cargo test                    # unit, vector, mutation and TCP integration tests
cargo clippy --all-targets
cargo +nightly fuzz run envelope   # see fuzz/README.md
crates/threnody-ffi/bindings.sh    # generate bindings, run the Python binding test
```

## License

Apache-2.0 OR MIT.
