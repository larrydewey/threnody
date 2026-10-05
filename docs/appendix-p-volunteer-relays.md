# Appendix P: Volunteer Relays and Directories (v1)

Status: specified here, modelled in [`proofs/relay_tokens.spthy`](../proofs/README.md), implemented in `threnody-core::directory`, `threnody-net::anon`, `threnody-net::volunteer` and `threnody-net::onion`. Fulfils spec §5.1 (optional directories) and §9 layer 2 ("onion / multi-hop routing through volunteer or user-run nodes"). It closes the gaps Appendix I ("contacts-only relays"), Appendix M (a persona's network address) and Appendix N (symmetric NATs on both ends) left open.

Onion circuits (Appendix I) can only use relays that are your own approved contacts, and a new user or a new persona has few or none. Volunteer relays carry circuits for strangers. Directories tell users which volunteers exist. Anonymous tokens (Appendix O) pay for each circuit, so relays can limit abuse without learning who is asking.

```
A ─anonymous link─▶ V1 ─anonymous link─▶ V2 ─anonymous link─▶ C
     fresh identity      (V1's identity)      (V2's identity)
```

## Roles

- **Directory.** An identity that lists volunteer relays and signs the list with its issuer keys (Appendix O). It also issues relay tokens. Anyone can run one. There are no built-in directories.
- **Volunteer relay.** A node that registers with directories and carries strangers' circuits when they pay with a token from one of them. Off by default.
- **Client.** Subscribes to directories and builds circuits through their relays. With at least one subscription this is on by default (it can be switched off). Subscriptions are the user's choice: nothing is subscribed by default.

Personas can be clients but never relays or directories, since both publish an identity with an address.

## Anonymous links

A link to a relay or directory isn't a contact session. Its initiator's first frame is the bytes `threnody anonymous link v1` instead of the first handshake message. After that, the usual handshake (Appendix A) and an encrypted session run, with the node's cover traffic. Neither side becomes the other's contact, and the app hears nothing about it. Only `AppMessage::Onion` (kind 10) and `AppMessage::Directory` (kind 22) are processed on such a link; anything else is ignored.

- **Clients dial with a fresh identity** for every link to a directory and every circuit's first hop. The far end learns an address, never who is dialing.
- **Relays dial onward as themselves**, since their identity is public anyway.
- **Limits.** A node holds at most 1024 inbound anonymous links, 16 from one address. An idle outbound link closes after 2 minutes.

## Relay descriptors

```
Descriptor = { 0: 1, 1: identity (32), 2: [* addr tstr] (1–8, each ≤ 64), 3: published_ms,
               4: expires_ms, 5: Ed25519(identity, KDF("… relay descriptor", identity,
               le64(published), le64(expires), le64(count), addr…)) }
```

A descriptor lives at most 48 hours; relays register a fresh one every 6 hours, valid 24 hours. Addresses are `host:port` for TCP.

## Directory documents

```
Document = { 0: 1, 1: issuer_id (32), 2: published_ms, 3: valid_until_ms,
             4: [* Descriptor bstr] (≤ 1024), 5: sig_ed (64), 6: sig_mldsa }
d        = KDF("… directory document", issuer_id, le64(published), le64(valid_until),
               le64(count), Descriptor…)
sig_ed   = Ed25519(issuer identity, d)
sig_mldsa = ML-DSA(issuer, d, ctx "threnody directory")
```

A document is accepted only if:

- its `issuer_id` is the subscription's;
- both signatures verify against the subscription's pinned issuer key, so forging one takes breaking Ed25519 and ML-DSA;
- it isn't published in the future (10 minutes of skew allowed);
- it hasn't expired, and it was valid for at most 48 hours.

Every descriptor in it must verify too. A directory re-signs its document every 30 minutes, or when its relays change. Each document is valid 6 hours.

## Links and subscribing

```
threnody-dir://<issuer_id, 52 Crockford base32 symbols>@<host:port>
```

To subscribe, a client dials the address over an anonymous link and asks for the directory's issuer key. It checks two things: that the key's id is the one the link pins, and that the node it reached holds that key's identity. Then it fetches the document and its tokens. A link pointing at another directory's address is refused. Clients refresh subscriptions every 30 minutes.

**Selection.** A client uses a relay only when at least `k` of its subscribed directories list it in a current document. By default `k` is 1 with one subscription and 2 with more (`--directory-threshold`). A directory that lists Sybil relays then gains nothing unless another subscribed directory lists them too. Of the matching descriptors, the newest is used.

## Relay tokens

A relay token is a presentation (Appendix O) of a credential with schema `threnody/relay-access/1`, no attributes, and `expires_day` = the day it is for:

```
RelayToken = { 0: epoch (day), 1: slot (u16 < 64), 2: Presentation }
context    = "threnody relay token v1" || relay identity || le32(epoch) || le16(slot)
binding    = KDF("… relay token binding", e_pub)        the hop's ephemeral X-Wing key
```

- **Issuing.** A client asks each directory for credentials for today and tomorrow, over a fresh anonymous link. A directory issues at most 8 per day to one network (an IPv4 /24 or an IPv6 /56). It learns an address, which says nothing about the circuits the credential later pays for.
- **Spending.** For each circuit through a volunteer relay, the client picks an unused slot for that relay and day. The resulting pseudonym is the same whenever that slot is reused, and unrelated across slots, relays and days. A relay accepts a token if all of these hold:
  - it is for this relay;
  - it is for today or yesterday;
  - it is bound to the CREATE's `e_pub`;
  - a directory this relay is registered with issued it;
  - its pseudonym hasn't been seen this epoch.

  So one credential opens at most 64 circuits per relay per day, and the relay learns nothing else about it. A relay checks at most 120 tokens per neighbour per minute, since each check costs pairings.
- **Clients** keep the credentials and used slots in encrypted state (`relay-wallet`). Used slots are never reused, and credentials older than yesterday are dropped.

## Circuits

Appendix I changes as follows:

```
OnionMsg CREATE gains  ? 7: token bstr
EXTEND data          = to (20) || e_pub (1216) [ || flags (1) [|| u16 len || addr] [|| u16 len || token] ]
                       flags: 1 address present, 2 token present
```

Without the optional part, EXTEND is exactly Appendix I's.

- **Who may create.** Any neighbour may CREATE a hop, but a hop may EXTEND only if it is *full*: the CREATE came from a mutually approved contact's session (as before), or carried a valid token. Other hops may only end there, with BEGIN (a session to that node, subject to its accept policy) or DEPOSIT (Appendix H, rate-limited). This is how a destination accepts the last relay's circuit without being a relay itself.
- **Extending.** A full hop extends as before to a live, mutually approved contact. A volunteer relay may also dial the EXTEND's address over an anonymous link, pinned to `to`, and send CREATE with the EXTEND's token. Non-volunteers never dial addresses for others.
- **Paths.** A client tries circuits in this order: two contact relays (Appendix I), then two volunteer relays. Then it dials directly, then uses relay circuits (Appendix G). A volunteer path is entered over an anonymous link with a fresh identity: `V1` (paid), then `V2` (paid, by address), then the destination (by its last known address). The two volunteers are picked from different networks when possible. Anonymous mailbox deposits (Appendix H) use the same fallback. Circuits over volunteers allow 20 s for the first hop and 40 s for each extension (60 s overall when reached automatically), since cover traffic on every link spaces messages.

## Registering

```
DirMsg = { 0: op, 1: id u64, ? 2: IssuerKey, ? 3: Document, ? 4: epoch, ? 5: commitment,
           ? 6: Issued, ? 7: Descriptor, ? 8: status, ? 9: reason tstr }
op: 1 get key, 2 key, 3 get document, 4 document, 5 token request, 6 token,
    7 register, 8 registered (status 1 listed, 2 pending review), 9 refused
```

A volunteer registers with every subscribed directory over an anonymous link, as itself. The directory checks that the descriptor verifies and is current, and accepts at most 8 registrations per network per day. It then dials one of the descriptor's addresses over a fresh anonymous link, expecting the relay's identity. Only a relay that answers is listed, at once or after the operator's review (`--review-relays`, then `/directory list <relay>`).

## Running one

```sh
threnody run --listen 0.0.0.0:7450 --serve-directory dir.example.org:7450      # prints the link
threnody run --listen 0.0.0.0:7450 --volunteer-relay relay.example.org:7450    # then /dir add <link>
threnody run                                                                    # /dir add <link>, then as usual
```

## What this protects, and what it doesn't

- **Against a single relay.** The entry relay sees the client's address but neither its identity nor the destination. The middle relay sees two relays. The last relay sees the destination and the middle relay. The destination sees the last relay's address, and authenticates the client end to end.
- **Sybil relays.** A directory vouches for reachability only. The threshold `k` and the user's choice of directories are the defence. A user who subscribes to a single directory trusts it not to list colluding relays.
- **Directories see addresses** of clients fetching documents and tokens. They don't see identities, or which relays or destinations those clients use.
- **Global adversary** (spec §2.3). As in Appendix I, an observer of every link could correlate circuits by timing. Cover traffic applies on every anonymous link but not inside circuits.
- **Reachability.** Volunteers must accept TCP at the addresses they publish. Destinations reached through volunteers need a TCP address the last relay can dial; others are reached through contact relays or rendezvous (Appendix N).
- **Tokens and quantum adversaries.** Tokens are BBS presentations (Appendix O). A quantum adversary could forge them and so ride relays for free. That would cost relays bandwidth, not users' anonymity.
