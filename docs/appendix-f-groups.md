# Appendix F: MLS Groups (v1)

Status: implemented in `threnody-groups` (on [openmls](https://github.com/openmls/openmls) 0.9). Its `node` feature adds `GroupNode`, which handles persistence, consent and delivery for the `/group` CLI commands and the app bindings. Fulfils spec §6.2.

## Ciphersuite

`MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519` (draft-ietf-mls-pq-ciphersuites, provisional code point `0x004F`).

- **Post-quantum hybrid key agreement.** Group HPKE uses X-Wing, so group key agreement is post-quantum hybrid, matching the 1:1 protocol.
- **Signatures.** Ed25519, made with each device's Threnody identity key. The `Signer` is implemented directly over `Identity`, so the seed is never copied into openmls.

## Identity binding

Every leaf uses a `BasicCredential` whose identity is exactly the leaf's 32-byte Ed25519 signature key. Any key package, Welcome or sender whose credential differs from its signature key is rejected. An MLS member therefore *is* a Threnody identity, and safety numbers and fingerprints carry over to groups.

## Delivery

There is no delivery service. Group traffic is carried as `AppMessage::Group(GroupWire)` (Appendix C, kind 6) inside the authenticated 1:1 ratchets:

```
GroupWire = { 0: kind, 1: group_id (16), ? 2: payload, ? 3: name, ? 4: member (32) }
  1 KeyPackageRequest  owner -> invitee           (name)
  2 KeyPackage         invitee -> owner           (MlsMessage(KeyPackage))
  3 Welcome            owner -> new member        (MlsMessage(Welcome), ratchet tree in extension)
  4 Message            sender -> every other member (MlsMessage, PrivateMessage only)
  5 Forward            sender -> a reachable member (MlsMessage, member = the one to deliver it to,
                       reference = the sender's id for a receipt, or absent)
  6 Receipt            forwarder -> sender      (member = who acknowledged, reference)
  7 Leave              member -> owner          (remove me)
```

The sender fans every message out to each other member. The groups use the pure-ciphertext wire format, so commits are encrypted too.

### Message content

An MLS application message carries one of these:

```
Content = text                         ; UTF-8, as the first versions sent (no id)
        / 0xFF || { 0: kind, ? 1: text, file name or emoji, ? 2: file data,
                    ? 3: flags, ? 4: caption, ? 5: album id, ? 6: message id,
                    ? 7: clip duration in ms }
kind: 1 text, 2 file (at most 8 MiB, as in 1:1), 3 reaction
flags: 1 sensitive, 2 voice message, 4 video message (file); 1 remove (reaction)
```

A UTF-8 string never starts with the byte 0xFF, so plain text from older members still reads as text. Text and files carry the sender's id for the message, which receivers record (history key 11). A reaction names a message by that id; members apply it under the reactor's account, as in 1:1 chats (Appendix C). Receivers save a file and record it in the group's history with where they saved it. Since that traffic also travels inside the pairwise ratchets, the network sees nothing group-specific.

### Reaching every member

Members need not be contacts of each other, or online together. Each copy goes to its member by the first of these that works:

1. The live session with that member. Sessions acknowledge group messages end to end, and resend them in the next session if the current one dies first (Appendix C, *Acknowledgements*).
2. Sealed for the member's mailboxes (Appendix H), when we hold their prekeys.
3. Forwarded by another member we have a session with, the owner first. The `Forward` carries the same MLS ciphertext and names the member to deliver it to. This applies to MLS messages only, never to key packages or Welcomes.
4. Held by us and sent when the member next connects. Meanwhile we try to reach them through relays (Appendix G).

A forwarder checks that the sender and the target are both members of the group as it knows it, and that the target is neither itself nor the sender. It then delivers a plain `Message` by steps 1, 2 and 4, and never forwards again, so a message crosses at most one forwarding member. Since the owner invited everyone, it usually has a session with every member, and a member that only ever talks to the owner still reaches the whole group.

Held messages are kept encrypted under the identity (`group-held` state), at most 200 per member, oldest dropped first. Pending invitations are kept the same way (`group-invites`). A message held for longer than five epochs' worth of membership changes can no longer be decrypted (see *Past epochs*).

Delivery ticks (Appendix C, *Acknowledgements*) count only a member's own acknowledgement. So when the sender hands a copy to a forwarder, it includes a reference. Once the member acknowledges the copy the forwarder delivered (live, or later from its hold), the forwarder sends the sender a `Receipt` naming the member and the reference. The sender believes receipts only from members, and only about members. A member could claim a delivery that didn't happen, but that only affects the ticks, and members can already drop messages they forward.

A forwarder learns only that one member is sending something to another, and the size and time. Both are members, so it could read the content anyway. Like any relay, it can drop or delay what it holds, but it cannot alter it: MLS authenticates every message. The membership checks keep a member from being used to send to non-members.

## Membership policy

Spec §6.2 leaves admin semantics to the application layer.

- **Owner.** The creator owns the group and is its only committer. Adds and removals are owner commits, and members reject commits from anyone else. Without a delivery service to order commits, this rules out epoch forks.
- **Joining.**
  1. The owner sends a `KeyPackageRequest` to the invitee.
  2. The invitee consents by returning a fresh key package. Requests from mutually approved contacts are accepted automatically; anyone else needs `/group accept`.
  3. The owner completes adds only for key packages it asked for, and only when the key package comes from the identity it invited.
  4. The owner commits the add, sends the Welcome to the new member, and sends the commit to the existing members.
- **Joining checks.** On a Welcome, the joiner checks the group id, the ciphersuite, that every member credential is valid, and that the inviter and the joiner are both members.
- **Leaving.** A member leaves by sending the owner `Leave` and forgetting the group at once. The owner commits its removal for the remaining members. An owner can't leave, but deleting the group removes every other member in one commit and then forgets it.
- **Removal.** On removal, the owner's commit goes to every member, including the one removed. The removed member's group becomes inactive, and later epochs are unreadable to it.
- **Past epochs.** Application messages from up to five past epochs are still accepted, to tolerate reordering around commits.

## Persistence

After every change, the full openmls key-value store and the group metadata are exported to `<home>/groups.state`. The file is written mode 0600 and encrypted:

```
file = nonce (12) || ChaCha20-Poly1305(KDF("state encryption key", identity_seed), nonce, ad = "groups", export)
```

The key is derived from the identity seed, so group secrets are only as accessible as the identity: if the identity is passphrase-sealed, so is everything else. State is saved *before* any resulting message leaves, so a crash can't leave us behind an epoch our peers have already moved to.

## CLI

```
/group new <name>                 /group invite <group> <peer>
/group accept [n]                 /group decline [n]
/group remove <group> <peer>      /group leave <group>
/groups
/g <group> <text>                 /gfile <group> [-s] <path>... [| caption]
```

## Not yet done

- **Secure deletion.** Each save rewrites the whole state file atomically. Old epoch secrets are gone from the file system's view, but not necessarily from the storage medium (spec §16 secure deletion).
- **More committers.** Admin roles beyond a single owner.
- **Ciphersuite version.** openmls's X-Wing HPKE implements X-Wing draft-06, while the 1:1 protocol uses draft-11. Both are hybrid; they will converge once the MLS PQ ciphersuite draft settles.
