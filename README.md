# Threnody

Threnody is an encrypted, metadata-resistant messaging protocol with post-quantum hybrid cryptography. This repository holds the Rust reference implementation of the [Threnody Protocol Specification](Threnody-Specification.md).

**Status: milestone 2.** Two devices can connect over IP and authenticate with an X-Wing (X25519 + ML-KEM-768) handshake. They can then chat and send files over a post-quantum double ratchet with encrypted headers, and mutually approve each other. Mutually approved devices get post-quantum-hybrid WireGuard tunnels. The KEM matches the official X-Wing test vectors, the handshake has a Tamarin model, every parser is mutation-tested, and identity keys can be protected with a passphrase. Mesh routing, MLS groups and onion routing are still to come (see [Roadmap](#roadmap)).

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

Inside `run`, any line you type goes to the current peer. The available commands are:

```
/connect <invite|contact|host:port>   /to <peer>   /peers   /contacts
/name <peer> <name>   /approve [peer]   /revoke [peer]
/safety [peer]   /verify [peer]   /file <path>   /drop [peer]
/policy anyone|contacts|approved   /status   /quit
```

You can also manage contacts outside a session with `threnody contacts | name | approve | revoke | verify | forget`.

`--constant-rate-ms N` sends one padded frame every N ms on each session, filling idle slots with cover traffic. `--policy approved` only accepts mutually approved contacts.

`threnody init --passphrase` seals the identity key with Argon2id. `threnody passphrase` adds, changes or removes the passphrase. `$THRENODY_PASSPHRASE` supplies it non-interactively.

`--tunnel [PORT]` builds WireGuard tunnels to mutually approved peers. Each pair's preshared key comes from its Threnody session, which makes the tunnels post-quantum hybrid. The node keeps `<home>/wireguard/thr0.conf` current: bring the interface up with `sudo wg-quick up …`, then add `--wg-apply` to push changes live. See [Appendix D](docs/appendix-d-tunnels.md).

## Layout

| Crate | Contents |
|---|---|
| `threnody-core` | Sans-IO protocol: identity and fingerprints, the hybrid KEM, the handshake, the ratchet, the CBOR wire format (via [`const-cbor`](https://crates.io/crates/const-cbor)), padding, and the contact/identity store |
| `threnody-net` | Stream framing, the async handshake driver, and `Node` (sessions, trust policy, approval exchange, constant-rate mode). The TCP transport works today, and the session driver works over any byte stream. |
| `threnody-cli` | The `threnody` binary |

The normative details that the spec deferred are written up in [`docs/`](docs/):

- [Appendix A: handshake](docs/appendix-a-handshake.md)
- [Appendix B: ratchet](docs/appendix-b-ratchet.md)
- [Appendix C: CBOR schemas](docs/appendix-c-wire-format.md)
- [Appendix D: WireGuard tunnels](docs/appendix-d-tunnels.md)
- [Test vectors](docs/test-vectors/v1.txt), regenerated and checked by `cargo test`
- [Tamarin model of the handshake](proofs/handshake.spthy)

## Spec coverage

| Spec | Status |
|---|---|
| §3.2 Hybrid KEM X25519 + ML-KEM-768 | ✅ X-Wing (draft-11), passes the official vectors |
| §3.2 Ed25519, ChaCha20-Poly1305 + AES-256-GCM, domain-separated KDF | ✅ BLAKE3 KDF; both AEADs negotiated |
| §3.3 FS / PCS ratchet with hybrid PQ updates | ✅ KEM double ratchet with header encryption |
| §3.4 Formal verification | 🟡 Tamarin model of the handshake (ratchet not yet modelled) |
| §4.2 Pseudonymous identity, §4.4 fingerprint | ✅ 32-character Crockford Base32 |
| §4.1 Anonymous mode, §4.3 selective disclosure | ❌ |
| §4.5 Multi-device | ❌ |
| §5 TOFU, out-of-band invites with pinned fingerprint, QR, safety numbers, mutual approval and revocation | ✅ (NFC, directories and web-of-trust not yet) |
| §6.1 1:1 text + files | ✅ (disappearing messages not yet) |
| §6.2 MLS groups | ❌ next milestone |
| §6.3 / §11 CBOR, versioning, unknown-field tolerance | ✅ |
| §6.4 Local-first store | ✅ identity and contacts (message history, sync and backup not yet) |
| §7 Transports | ✅ TCP/IP; ❌ BLE, Wi-Fi Direct, mesh routing, discovery |
| §8 WireGuard full-mesh tunnels | ✅ kernel WireGuard; PQ PSK from the session; gated on mutual approval |
| §9 Metadata layers | ✅ padding, constant-rate + cover; ❌ onion routing, local-first preference |
| §10 Status indicators | ✅ `/status` |
| §3.1 Platform keystore | software fallback: 0600 files plus optional Argon2id passphrase |
| §15 Test vectors | ✅ `docs/test-vectors/v1.txt` |

## Roadmap

1. **Hardening (remaining).** A Tamarin model of the ratchet, OS keystores, ratchet persistence across reconnects, and an external audit.
2. **MLS groups.** Use `openmls` with an X-Wing ciphersuite.
3. **Tunnels (remaining).** A `boringtun` data plane for mobile and unprivileged use.
4. **Local mesh.** Beacons that only approved peers can recognise, BLE / Wi-Fi Direct transports, and multi-hop relay.
5. **Metadata.** Onion routing through volunteer nodes; anonymous and selective-disclosure identities.
6. **Mobile.** UniFFI bindings for iOS and Android. The core is sans-IO, so this is mostly glue code.

## Development

```sh
cargo test                    # unit, vector, mutation and TCP integration tests
cargo clippy --all-targets
cargo +nightly fuzz run envelope   # see fuzz/README.md
```

## License

Apache-2.0 OR MIT.
