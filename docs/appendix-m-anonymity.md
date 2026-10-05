# Appendix M: Anonymous Identities and Selective Disclosure (v1)

Spec §4.1 asks for an anonymous mode: identifiers that are ephemeral or rotated, and no long-term linkability unless the user explicitly links them. §4.3 asks that users control exactly which attributes each peer sees. This appendix describes how Threnody does both (`threnody_core::persona`, `threnody_net::identity`).

## Personas

A persona is an anonymous identity. It is a separate Threnody identity with its own Ed25519 and X-Wing keys, contacts, message history, prekeys and state. It lives in its own directory under the main home, `personas/<id>/`, and runs as a node of its own. Nothing a persona sends is derived from the main identity or from other personas, so its contacts can't link it to them. A device can present its main identity to some peers and different personas to others (spec §4).

- **Making one.** The main identity creates a persona with a label, which stays local and is never sent. A persona can also have an expiry. Its key is sealed like the main identity's: with the system keyring, the Android Keystore or a passphrase. The list of personas is kept encrypted under the main identity's key.
- **What a persona doesn't do.** It doesn't link other devices or join an account; its node refuses. Its device name is always the generic "device", never the hostname or phone model. Apps and the CLI don't run LAN discovery, Bluetooth LE, Wi-Fi Direct or WireGuard tunnels for it, since each would show nearby devices or contacts who it is. It listens on its own port, never the main identity's.
- **Burning.** Burning a persona deletes its directory, its identity file first. Everything else in that directory was encrypted under the persona's key, so it becomes unreadable even if deleting the rest is interrupted. Apps also delete the persona's received files. A persona whose expiry has passed is burned the next time its main identity's node or the CLI starts.

## Revealing

A persona's user can later choose to show a peer who they are. The main identity signs a link statement, and the persona sends it:

```
LinkProof = Ed25519_sign(main, "threnody persona link v1" || persona_public_key)
IdentityMsg (kind 2) = { 0: 2, 2: main_public_key (32), 3: signature (64), ? 4: invite tstr }
```

The receiver verifies the signature against the main key it names, and checks it is for the persona that sent it. A proof for another persona, or one claiming someone else's identity, fails. The receiver keeps the result with the contact (`Contact::revealed`) and tells the app, which can offer to add the main identity using the invite. Nothing is added automatically.

A reveal can't be taken back: the peer now holds a signature linking the two identities, and could show it to others. Apps say so before sending one.

## Profiles and selective disclosure

Each identity, main or persona, can describe itself with up to 16 attributes. Keys are up to 32 characters and values up to 256, with no control characters. The attribute `name` is the one apps show as a contact's name. For each contact the user chooses which keys that contact sees; by default, it sees none.

```
IdentityMsg (kind 1) = { 0: 1, 1: [* [key tstr, value tstr]] }
```

A node sends a contact the attributes shared with it when a session starts, and again whenever the profile or the choice changes. Each message replaces what the contact had, so unsharing an attribute removes it from the contact's view. They may have kept a copy, though. The choice applies to all of a contact's account's devices. Own devices are never sent profiles.

A name a stranger shares isn't shown until they're accepted (message requests), since it could be anything. A name the user gave a contact takes precedence.

Identity messages travel as `AppMessage::Identity` (kind 17), only to peers whose `Hello` sets feature bit 8 (`FEATURE_IDENTITY`).

## What this protects, and what it doesn't

- **Linking by contacts.** Peers who talk to two of your identities can't tell from the protocol that they're the same person. Neither can mailboxes, relays or onion hops, which only ever see the identity they handle.
- **Linking by network address.** A peer who reaches your main identity and a persona directly sees the same IP address for both. Onion routing first (Appendix I) hides that when two approved relays exist. A new persona has few contacts and often none to relay through. With a directory subscribed (Appendix P), a persona instead reaches contacts through two volunteer relays, entered with a fresh identity. Without one, apps and the CLI say that the address shows.
- **Timing.** Personas run on the same device as the main identity, so an observer who sees both identities' traffic could correlate when they're online. Cover traffic (Appendix C) makes sending times uniform, but not online times.
- **Selective disclosure** of a profile is a choice about what to send, not a proof: a peer who was shown an attribute knows it, but nothing vouches for it. For attributes someone else vouches for, zero-knowledge credentials (Appendix O) prove chosen attributes without revealing the others, and unlinkably.
