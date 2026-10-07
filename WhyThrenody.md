**Why Threnody**

I used Signal exclusively since its inception—over a decade. I believed the marketing: “private by default,” end-to-end encryption, open source, no ads. And on content confidentiality it delivered. But over time I realized it was only a partial solution to the confidentiality I actually expected.

The moment that made it concrete was the large migration from WhatsApp to Signal. The entire service was brought down by the sudden load—centralization and lack of physical resources turning a privacy-focused tool into a single point of failure. Combined with my 4.5 years at AMD working on confidential computing, which repeatedly highlighted how many gaps still exist in most “solutions,” it became clear: content encryption alone is not enough. I needed something that treated metadata resistance and the absence of central infrastructure as first-class requirements, not optional extras.

That is why Threnody exists.

**The clean value proposition**

- **No central servers by design.** No phone numbers. No provider accounts. No company that can be subpoenaed, pressured, or compelled to keep logs of connection patterns. Offline messages are sealed to prekeys and held by mutual contacts. Relays (when needed) are peer-run or volunteer, paid with unlinkable tokens, and preferentially onion-routed so no single hop sees both ends. Direct paths are preferred; local mesh (Bluetooth LE → Wi-Fi Direct upgrade) is first-class.

- **Metadata resistance is the default, not an afterthought.** Constant-rate padded frames + cover traffic, encrypted headers, onion circuits through two or more relays by preference, private discovery beacons that only mutually approved peers can recognize. The protocol is explicit about the threat model: casual observer, targeted attacker, and global adversary. Cover traffic has a real cost (~230 MB/day per connected contact at the default rate); the project does not pretend otherwise.

- **Post-quantum hybrid cryptography from day one.** X-Wing (X25519 + ML-KEM-768) handshake, post-quantum double ratchet with encrypted headers, MLS groups on an X-Wing ciphersuite. No “we’ll add PQ later” plan. Core pieces have Tamarin machine-checked proofs.

- **True multi-device and identity flexibility without a central identity provider.** Equal-peer devices linked by one-time codes; any device can add or revoke others. Anonymous personas that are cryptographically unlinkable to your main identity (and burnable). Selective disclosure via zero-knowledge credentials (BBS with post-quantum-signed issuance). You prove only what you choose to prove, to whom you choose.

- **Transport-agnostic mesh that actually works offline and across mixed connectivity.** Approved devices auto-discover on the LAN, form post-quantum-hybrid WireGuard tunnels, and can relay across Bluetooth/IP boundaries. A phone with only Bluetooth can still reach the wider network through contacts that have IP.

**Why not just use Signal, WhatsApp, Threema, or SimpleX Chat?**

Signal is the best of the centralized options: solid crypto, open source, nonprofit, post-quantum key agreement. It still runs servers. Those servers necessarily see connection timing, approximate volume, and (historically) more metadata than pure end-to-end content encryption requires. WhatsApp is Meta.

Threema improves on several of Signal’s weaknesses: no phone number, Swiss jurisdiction, paid one-time model with no advertising incentive, and strong metadata minimization. It remains fundamentally centralized. Servers still sit in the middle of every conversation path and can observe connection patterns.

SimpleX Chat goes further still. It eliminates user identifiers, uses user-chosen or preset relays, and treats metadata protection more seriously than most messengers. That is a real advance. Yet it is still relay-centric rather than local-first mesh. It does not provide automatic full-mesh WireGuard-style tunnels between approved devices, multi-transport opportunistic networking (Bluetooth LE → Wi-Fi Direct), or the same depth of post-quantum hybrid cryptography and formal verification that Threnody builds in from the start. Offline delivery and group messaging also take different trade-offs.

Threnody’s stance is unapologetic: if your threat model includes sophisticated network observers, compelled providers, or the desire for communication that does not *require* any third party (central or relay) to stay available, the existing options remain incomplete. Local-first + optional decentralized onion relays + aggressive default metadata protection + post-quantum hybrid cryptography + true multi-device without a central identity provider is the coherent answer.

**Be real about the current state**

It is early (milestone 6 as of early October 2026). It is not audited. Do not bet lives on it yet. The UX is still CLI-first with a Linux desktop client and Android sample; it is not a polished consumer app for your entire contact list tomorrow. Cover traffic and multi-hop paths cost bandwidth and latency. Adoption requires your counterparties (or a critical mass of relays) to run it.

That is the honest trade-off. Threnody is not trying to be the next mass-market chat app. It is trying to be the protocol you reach for when the existing ones still leave you exposed on the axes that actually matter against serious adversaries: metadata, central points of failure, and long-term cryptographic strength.

If that is the bar you care about, the why is straightforward: everything else still makes compromises Threnody refuses to make.