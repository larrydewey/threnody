# Appendix S: Handles

## The problem

Adding someone needs an invite: a link or QR code passed from one person to the other. That works when both are in the same room, or already talk somewhere else. It doesn't work when someone reads your name on a business card, a profile or a mailing list. Phone numbers and usernames solve this in other messengers, but a directory of who is on the network is exactly what Threnody avoids.

## Goals

- Something short a person can say aloud or type: `larry-7K3M-QX9P`.
- No directory, server or registration. Nothing to enumerate: only someone who has the handle can find its owner.
- No one can take or imitate someone else's handle.
- Off unless the user makes one, and easy to change or drop.

## Non-goals

- Finding people by name alone. A name without its code finds nothing.
- Hiding the owner from people who have the handle. A handle is an invitation: whoever has it can write to the owner (as a message request, Section 6.1) and learn its IP address, as with an invite link.

## Format

```
handle = name "-" code[0..4] "-" code[4..8]
name   = 1-24 of a-z 0-9 _ .  (lower-cased; not starting or ending with .)
code   = Crockford Base32 of the first 40 bits of
         Argon2id(password = id || version_le64,
                  salt = "threnody handle v1" || name,
                  m = 64 MiB, t = 3, p = 1, 32 bytes)
```

`id` is the owner's 32-byte identity key and `version` a counter the owner raises to get a new code. Parsing ignores case, spaces, a leading `@` and the dash inside the code, and folds the usual Crockford confusables (O→0, I/L→1). Names have no dashes, so the code is whatever follows the first one.

The code binds the handle to its owner's key. Making up a key whose code matches someone else's handle takes about 2^40 Argon2id runs, thousands of CPU-years at these parameters. A finder checks the code against the key in the record before connecting, and the session's handshake then proves the key. Safety numbers (Section 5) remain the way to be sure.

## Records in the DHT

Handles use the Mainline DHT as rendezvous does (Appendix N, section 3), with keys derived from the handle instead of a pairwise discovery key:

```
base       = KDF("… handle", handle string, normalized)
epoch      = unix_seconds / 3600
record:     seed, salt, enc_key = KDF("… handle signing key" | "… salt" | "… encryption", base, epoch)
knock slot: seed, salt, enc_key = KDF(…, base, epoch, "knock", slot)      slot in 0..4
probe key  = KDF("… handle probe key", base)
value      = nonce (24) || XChaCha20-Poly1305(enc_key, nonce, pad(payload, 448), aad)
Record     = { 0: id bstr(32), 1: version uint, 2: Candidates bstr }   aad "threnody handle record v1"
Knock      = { 0: id bstr(32), 1: Candidates bstr }                    aad "threnody handle knock v1"
```

`Candidates` is Appendix N's record body. Every value is 488 bytes, whatever it holds.

- **Publishing.** The owner publishes its record when its addresses change, when the epoch changes, and otherwise every 30 minutes.
- **Finding.** A finder reads the record for the current and previous epoch, checks the code, and dials the owner's addresses for five seconds. Public IPv6 addresses and mapped ports usually answer.
- **Knocking.** If the dial fails, the finder writes a knock, with its own addresses, to a random slot and starts punching toward the owner (Appendix N, section 4) for two minutes.
- **Answering.** The owner polls the four knock slots of the current and previous epoch at the rendezvous rates (every 20 s while the app is open, every 15 minutes in the background). It punches toward each fresh knock (under five minutes old) it hasn't acted on.
- **Probes.** Both sides key their probes with the handle's probe key, in place of the discovery key two contacts share.

The session that results is an ordinary session with a stranger: its messages are a message request until the owner accepts.

## Invite links

An invite link made on mobile data used to carry an address nobody could dial: the phone's address behind carrier-grade NAT, or none at all on an IPv6-only network. Invites now meet the same way handles do, with a random key instead of a handle:

```
link  = "threnody://" fingerprint [ "@" addr *( "," addr ) ] [ "?r=" key ]
addr  = host ":" port                -- IPv6 in brackets: [2001:db8::1]:7450
key   = Crockford Base32 of 16 random bytes (26 symbols)
base  = KDF("… invite", key)         -- then records, knock slots and probe key as above
```

A finder tries, in order: an onion circuit through approved contacts or volunteer relays when one can be had (Appendices I and P), as for any dial; every address in the link; the meeting place the key names; and last, relay circuits through approved contacts (Appendix G), which work only through a contact both people already share.

- **Addresses.** A link carries up to four: global IPv6 addresses first (no NAT to cross, often reachable directly), then the address the app suggests, then local and public IPv4 ones. The finder dials them all, 300 ms apart and each for at most 6 s, over TCP and, half a second later, QUIC, since mobile IPv6 firewalls often drop TCP silently. A dead address no longer holds up the rest.
- **The key.** Each device makes one the first time it shows an invite and keeps it (state `invite-key`). From then on it publishes its record under the key and answers knocks there, as for a handle. When none of the addresses answers, the finder reads the record, checks that it names the fingerprint in the link (that check replaces a handle's code), dials, then knocks and punches. Relays come last, as before.
- **Off with reachability.** Without internet reachability (and so for anonymous identities) a link the apps make has no key and only its addresses work. The CLI's `threnody invite <addr>[,<addr>…]` always adds the key; a `run` started after that keeps the record under it, unless started with `--no-rendezvous`.
- **Versions.** Both sides need 0.17.1 or later: older versions can't read a link with several addresses or a key, and never publish under one.
- **Limits.** The meeting place ends in a hole punch (Appendix N, section 4), which often fails behind symmetric NAT, as many VPNs are, especially when both people use one. Relays need a contact the two already share, so a brand-new contact with both sides behind VPNs may not connect.
- **Cost.** One more record to keep, and knock slots to read at the handle rates. Last hour's slots are read only in the first five minutes of an hour, when a knock there can still be fresh; this halves the reads for handles too.
- **Privacy.** Whoever has one of your invite links can find your current address for as long as the key lasts, as a handle allows. `reset_invites` (FFI) / `Node::reset_invite_key` makes a new key; older links then keep only their addresses. The Android app offers it under Network & Connectivity → Reset invite links, and the Linux app under Preferences → Invite links; the iOS app does not yet. Links without `?r=` and links with one address still parse.

## Changing or removing a handle

Raising `version` gives a new code. Removing the handle, or changing it, stops publishing; the old record expires from the DHT within about two hours and is ignored after its epoch passes. People who already added you keep you as a contact.

## Privacy

| Who | Learns |
|---|---|
| Someone with the handle | The owner's identity key and current addresses, and that the owner is online when the record is fresh. They can write a message request. |
| DHT nodes | Small values under random-looking keys that change every hour. Not the handle, the name or the identity. |
| Anyone else | Nothing: the keys come from the whole handle, including its 40-bit code. |

**Trade-offs, stated honestly:**
- **A handle is as public as you make it.** Posting it publicly is posting an invite: anyone can send message requests and see your IP address. Change the code if it gets abused.
- **Anyone with the handle can overwrite its DHT records**, since the DHT keys come from the handle. They can stop people from finding you (until the next hour's keys, or a new code), but not pretend to be you: a forged record fails the code check or the handshake.
- **Background delay.** A finder reaches an owner behind NAT only while the owner polls: within about 20 s if its app is open, otherwise when the knock is seen within the finder's two minutes of punching. Owners reachable directly (IPv6, mapped port) answer at once.

## Implementation

`threnody-core::handle` (format, code, records, knocks), `threnody-core::invite` (invite links and keys), `threnody-net::handle` (publishing, polling knocks, `Node::set_handle`, `Node::find_handle`, `Node::invite_link`, `Node::reach_invite`), the FFI's `set_handle`, `own_handle`, `find_handle` (not `handle`, which UniFFI's Swift objects already use), `invite_link` and `reset_invites`, and `connect`, which takes handles and invite links too. The CLI publishes one with `/handle <name>`; `/connect` takes handles. The Android and Linux apps have "Your handle" and "Reset invite links" in their settings, and their "Add a contact" boxes take handles. `proofs/meeting.spthy` proves that a finder reaches only the device a link or handle names, even when records at its meeting place are forged.
