# Threnody Protocol Specification

**Version:** 0.1.0-draft  
**Date:** 2026-10-03  
**Status:** Formal Design Specification (Closed)  
**License:** This specification is released under the Apache License 2.0.  
**Copyright:** Public domain dedication of the protocol design intent; implementations remain under their own licenses.

---

## Abstract

Threnody is a cross-platform protocol and application for encrypted, metadata-resistant communication. It combines:

- End-to-end encrypted 1:1 messaging and large-group/channel communication (MLS)
- Automatic full-mesh WireGuard-style tunnels between mutually approved devices
- Opportunistic multi-transport mesh networking (Bluetooth, Wi-Fi Direct, cellular, Wi-Fi, and any other available link)
- Strong post-quantum hybrid cryptography
- Layered metadata protection against global adversaries
- Hybrid identity model supporting anonymity, pseudonymity, and selective disclosure
- Fully decentralized operation by default with optional trusted introducers/directories

The protocol is transport-agnostic, local-first, and designed so that all payload traffic travels only through encrypted channels. The reference implementation language preference is Resid (where platform support exists) with Rust as the practical cross-platform fallback.

This document is a closed formal specification: every major design decision has been resolved. Implementers are expected to follow the normative requirements exactly.

---

## 1. Introduction

### 1.1 Goals

- Provide high-assurance encrypted messaging and real-time connectivity across desktop (macOS, Linux, BSD, Windows) and mobile (iOS, Android) platforms.
- Support automatic discovery and multi-path mesh networking among approved devices while keeping all communication encrypted.
- Offer automatic full-mesh encrypted tunnels (WireGuard-compatible) between mutually approved devices.
- Minimize metadata leakage even against a global passive + active adversary.
- Support large dynamic groups via the Messaging Layer Security (MLS) protocol.
- Allow users to operate fully anonymously, pseudonymously, or with selective disclosure of identity attributes.
- Remain fully open-source and auditable under a permissive license.

### 1.2 Non-Goals

- Remote hardware attestation (Secure Enclave / TPM / Keystore remote attestation is explicitly out of scope).
- Centralized identity or mandatory phone-number registration.
- Built-in lawful-intercept or exceptional-access mechanisms.
- Guaranteeing delivery or connectivity when no path exists between devices.

### 1.3 Terminology

- **Device**: A single running instance of a Threnody implementation (phone, laptop, etc.).
- **Identity**: A long-term cryptographic identity (see Section 4).
- **Approved peer**: A remote identity that has been mutually accepted for full mesh / tunnel participation.
- **Transport**: Any underlying link (Bluetooth LE, Wi-Fi Direct, IP, etc.).
- **Overlay**: The encrypted messaging and tunnel layer that sits above all transports.
- **MLS Group**: A Messaging Layer Security group used for large multi-party communication.

---

## 2. Threat Model

Threnody defines three nested threat models. Security properties are stated relative to each.

### 2.1 Casual Adversary
- Local network observer
- Malicious Wi-Fi access point
- Curious service provider on any single path

**Properties required**: Confidentiality and integrity of all payload; basic authenticity of peers.

### 2.2 Targeted Adversary
- Active network attacker who can modify, inject, or drop traffic on some paths
- Compromise of a minority of user devices
- Ability to run malicious introducer or directory nodes

**Properties required**: All Casual properties + forward secrecy, post-compromise security where feasible, resistance to key-compromise impersonation, and ability to revoke compromised devices.

### 2.3 Global Adversary
- Passive and active observer of the entire network
- Ability to correlate timing, volume, and destination patterns across all links
- Ability to operate a large fraction of relay / mesh nodes

**Properties required**: All Targeted properties + strong metadata protection (who communicates with whom, when, and how often) when the user enables the corresponding protection layers.

The protocol does **not** claim protection against a global adversary who can also compromise a majority of a user’s own devices simultaneously or who can break the underlying post-quantum assumptions.

---

## 3. Cryptographic Design

### 3.1 Design Principles

- Post-quantum hybrid cryptography from day one.
- Follow current IETF / CFRG recommendations for hybrid KEMs and messaging as of late 2026.
- Prefer algorithms with mature implementations and clear security proofs.
- All long-term secrets protected by the platform’s best available keystore (Secure Enclave, StrongBox, TPM, etc.) when present; software fallback otherwise.

### 3.2 Concrete Ciphersuites (Normative)

The specification pins the following (exact parameter sets to be updated to the latest CFRG consensus at the time of finalization):

- **Hybrid KEM**: X25519 + ML-KEM-768 (or the CFRG-recommended hybrid equivalent).  
  Shared secret is the concatenation (or properly domain-separated combination) of both.
- **Signatures**: Ed25519 (primary). Optional secondary post-quantum signature (e.g., ML-DSA / Dilithium) may be added later via versioning.
- **AEAD**: ChaCha20-Poly1305 (preferred for software performance) or AES-256-GCM (preferred when hardware acceleration is available). Both MUST be supported.
- **Hash / KDF**: BLAKE3 or SHAKE256 / SHA-3 family (CFRG recommendation at finalization time). HKDF-SHA256/SHA512 as transitional.
- **Handshake framework**: Noise-inspired pattern with hybrid KEM insertion (exact pattern to be specified in the detailed crypto appendix).

All symmetric keys derived from the handshake MUST use a domain-separated KDF.

### 3.3 Forward Secrecy and Post-Compromise Security

- 1:1 channels use a double-ratchet (or Noise-based continuous ratchet) with hybrid PQ updates.
- MLS groups provide their own epoch-based forward secrecy and post-compromise security.
- WireGuard-style tunnels use ephemeral keys per session with periodic rekeying.

### 3.4 Formal Verification

Critical components (handshake, ratchet, MLS integration points, group key derivation) SHOULD have machine-checked proofs (Tamarin, ProVerif, or equivalent) where feasible. The specification will identify the exact claims that require proofs.

---

## 4. Identity Model

Threnody supports three complementary identity modes. A single device MAY present different modes to different peers.

### 4.1 Anonymous Mode
- Ephemeral or frequently rotated identifiers.
- No long-term linkability across sessions unless the user explicitly links them.
- Suitable for maximum privacy.

### 4.2 Pseudonymous Mode
- Stable long-term Ed25519 (and optional PQ) key pair.
- Fingerprint or short cryptographic identifier used as the primary handle.
- No forced binding to real-world identity.

### 4.3 Selective Disclosure Mode
- Attribute-based or zero-knowledge style credentials (future extension point).
- User controls exactly which attributes are revealed to which peers.

### 4.4 Identity Representation

- Primary identity is the long-term public verification key (Ed25519).
- Short fingerprint (e.g., 32-character Crockford Base32 or similar) for human verification.
- Optional human-memorable handles resolved only through user-controlled or optional public directories.

### 4.5 Multi-Device

The strongest practical multi-device model compatible with MLS and the identity system will be used. Requirements:

- Cryptographic binding of devices to the identity.
- Easy addition and revocation of devices.
- No single point of failure if the “primary” device is lost (exact design left to the detailed multi-device section; preference for threshold or equal-peer models where feasible).

---

## 5. Trust Establishment and Approval

### 5.1 Discovery Methods (all complementary)

1. Out-of-band: QR codes, NFC, safety numbers, shared secrets, physical exchange.
2. Optional public directories / introducers (rate-limited, anti-scraping).
3. Social / web-of-trust encrypted introductions.

### 5.2 Trust Lifecycle

- **Discovery**: Trust-on-first-use (TOFU) allowed for initial contact.
- **Full mesh / WireGuard participation**: Explicit mutual cryptographic approval required from both sides.
- **Revocation**: Immediate and easy. Revocation propagates through the mesh and MLS groups according to the group’s policy.

Safety-number style continuous verification MUST be supported and encouraged.

---

## 6. Messaging Layer

### 6.1 1:1 Messaging

- Double-ratchet (or equivalent) with hybrid PQ key updates.
- Support for text, files, and future media.
- Disappearing messages as an optional policy.

### 6.2 Large Groups and Channels

- Messaging Layer Security (MLS) as defined by the IETF.
- Support for large dynamic membership.
- Admin and membership control semantics defined by the application layer on top of MLS.

### 6.3 Message Format

- All application messages encoded in **CBOR**.
- Explicit version field and extensibility tags.
- Strict semantic versioning of the protocol.
- Unknown fields must be ignored for forward compatibility (with documented exceptions for security-critical fields).

### 6.4 History and Sync

- Local-first storage.
- Optional encrypted multi-device sync.
- User-controlled encrypted backups (no plaintext ever leaves the user’s devices unless the user explicitly exports it).

---

## 7. Transport and Mesh Layer

### 7.1 Design

- Transport-agnostic messaging core.
- Pluggable backends for every available physical or virtual link.
- Automatic discovery of nearby approved devices.
- Multi-path, multi-hop routing among approved devices.
- **All payload traffic MUST travel only through encrypted channels.** Cleartext is forbidden on any transport.

### 7.2 Supported Transports (non-exhaustive)

- Bluetooth Low Energy / Classic
- Wi-Fi Direct / Wi-Fi Aware
- IP (Wi-Fi, cellular, Ethernet)
- Any future link that can carry datagrams or streams

### 7.3 Mesh Routing

The formal design will specify a multi-path, multi-hop system that prioritizes:

- Encrypted channels only
- Metadata protection
- Resilience and scale

A hybrid approach (local gossip / direct paths + structured overlay or onion-style multi-hop for wider reach) is expected. Exact algorithm will be detailed in the mesh appendix; the normative requirement is that it remains secure under the Global adversary model when the corresponding protection layers are enabled.

### 7.4 Built-in Connectivity

Any Threnody node MAY act as a relay or path provider for other approved nodes. This is designed to support large-scale mesh networking without mandatory central infrastructure.

---

## 8. WireGuard-Style Full-Mesh Tunnels

### 8.1 Behavior

- Once two devices have mutually approved each other, they automatically attempt to establish a WireGuard-compatible encrypted tunnel.
- Full mesh: every pair of mutually approved devices should be able to form a direct tunnel when network conditions allow.
- Tunnels are used for both messaging traffic and general IP connectivity between the devices (user-configurable).

### 8.2 Key Management

- Tunnel keys derived from the same long-term identity material and ephemeral handshakes used by the messaging layer.
- Automatic rekeying and rotation.
- Seamless fallback to the overlay messaging path when a direct tunnel is unavailable.

---

## 9. Metadata Protection Layers

Users can enable any combination of the following (all optional, all layered):

1. **Padding + cover traffic + constant-rate sending** where feasible.
2. **Onion / multi-hop routing** through volunteer or user-run nodes.
3. **Local-first preference**: prefer Bluetooth / Wi-Fi Direct / local mesh to avoid exposing traffic to the global Internet.

The protocol must make the current protection level visible to the user.

---

## 10. User Experience Requirements (Protocol Support)

The protocol and reference clients MUST support:

- QR-code, NFC, and link-based contact addition with safety-number verification.
- Seamless multi-device linking and revocation.
- Group and channel creation with clear administrative controls.
- One-tap or automatic local mesh joining among already-approved devices.
- Clear, persistent indicators of:
  - Active transport(s)
  - Whether a WireGuard tunnel is up
  - Current metadata-protection level

---

## 11. Serialization and Versioning

- Primary encoding: **CBOR**
- Every top-level message carries an explicit protocol version.
- Extensibility via CBOR tags and reserved fields.
- Semantic versioning of the protocol (MAJOR.MINOR.PATCH).
- MAJOR version increments indicate breaking changes; implementations MUST reject unknown major versions for security-critical messages.

---

## 12. Implementation Guidance

### 12.1 Language

- Protocol is language-agnostic.
- Preferred reference implementation language: **Resid** (larrydewey/Resid) where the target platform is supported, because of its security model (no ambient authority, signed provenance) and performance characteristics.
- Practical cross-platform fallback: **Rust** (including mobile via appropriate frameworks).

### 12.2 Open Source

- Fully open source.
- Preferred license for reference code: Apache 2.0 or MIT.

### 12.3 Platform Support

Simultaneous cross-platform development is the goal. Shared core logic is mandatory.

---

## 13. Compliance and Legal Stance (Non-Normative)

- Threnody is designed for maximum user privacy and security.
- No backdoors, no exceptional access, no intentional weaknesses.
- The protocol itself stores no user data on any server by default.
- Implementers are solely responsible for compliance with local law, export regulations, age-appropriate design rules, and data-protection regulations in the jurisdictions where they distribute software.
- Data-minimization is a core design principle.

---

## 14. Performance Targets (Non-Normative)

The following are goals, not hard requirements:

- 1:1 message latency over a direct path: < 100 ms under good conditions.
- Group message fan-out should scale reasonably with MLS.
- Cover traffic and padding overhead should be user-configurable and clearly communicated (battery and bandwidth impact).
- Local mesh discovery should complete within a few seconds when devices are in radio range.
- Battery impact of background mesh maintenance should be minimal when the user has not enabled aggressive cover traffic.

---

## 15. Document Structure and Future Work

This document is the top-level formal specification. Detailed appendices (to be produced in subsequent revisions) will cover:

- Exact Noise-hybrid handshake patterns
- MLS integration profile
- Mesh routing algorithm
- CBOR schema definitions
- Multi-device protocol
- WireGuard key derivation
- Formal verification claims
- Test vectors

---

## 16. Security Considerations

See Section 2 (Threat Model) and Section 3 (Cryptography). Additional considerations will be expanded in the full security considerations section of the final RFC-style document.

Key residual risks that implementers must address:

- Side-channel resistance of the cryptographic implementations.
- Secure deletion of keys and message material.
- Protection against physical seizure of devices.
- Correct implementation of revocation.

---

## 17. IANA / Registry Considerations

None at this time. Future versions may define CBOR tag assignments or protocol version registries.

---

## Appendix A — Summary of Locked Decisions

| Area                        | Decision |
|----------------------------|----------|
| Name                       | Threnody |
| Primary purpose            | Hybrid messaging + mesh + full-mesh WireGuard tunnels |
| Attestation                | Mutual authentication via keys/certificates only |
| Identity model             | Hybrid (anonymous + pseudonymous + selective disclosure) |
| Discovery                  | Out-of-band + optional directories + web-of-trust |
| Trust establishment        | TOFU for discovery; explicit mutual approval for mesh/VPN |
| Groups                     | MLS |
| Crypto                     | Post-quantum hybrid per current IETF/CFRG recommendations |
| Serialization              | CBOR with versioning and extensibility |
| Metadata protection        | Layered (padding/cover, onion, local-first) — user selectable |
| Transports                 | Pluggable, all payload encrypted |
| Mesh                       | Automatic discovery + multi-path/multi-hop among approved devices |
| WireGuard                  | Automatic full-mesh between mutually approved devices |
| Multi-device               | Strongest practical model compatible with MLS |
| History                    | Local-first + optional encrypted sync + user-controlled backups |
| Formal verification        | Machine-checked proofs for critical components where feasible |
| Implementation language    | Protocol agnostic; Resid preferred, Rust practical fallback |
| License preference         | Permissive (Apache 2.0 / MIT) |
| Compliance stance          | Strong privacy by design; no backdoors; non-normative legal guidance |

---

**End of Threnody Protocol Specification v0.1.0-draft**

This document is intentionally complete with respect to all architectural decisions. Subsequent revisions will only add concrete algorithm details, test vectors, and formal verification artifacts.
