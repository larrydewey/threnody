# Appendix F: MLS Groups (v1)

Status: implemented in `threnody-groups` (on [openmls](https://github.com/openmls/openmls) 0.9) and the `/group` CLI commands. Fulfils spec §6.2.

## Ciphersuite

`MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519` (draft-ietf-mls-pq-ciphersuites, provisional code point `0x004F`).

- **Post-quantum hybrid key agreement.** Group HPKE uses X-Wing, so group key agreement is post-quantum hybrid, matching the 1:1 protocol.
- **Signatures.** Ed25519, made with each device's Threnody identity key. The `Signer` is implemented directly over `Identity`, so the seed is never copied into openmls.

## Identity binding

Every leaf uses a `BasicCredential` whose identity is exactly the leaf's 32-byte Ed25519 signature key. Any key package, Welcome or sender whose credential differs from its signature key is rejected. An MLS member therefore *is* a Threnody identity, and safety numbers and fingerprints carry over to groups.

## Delivery

There is no delivery service. Group traffic is carried as `AppMessage::Group(GroupWire)` (Appendix C, kind 6) inside the authenticated 1:1 ratchets:

```
GroupWire = { 0: kind, 1: group_id (16), ? 2: payload, ? 3: name }
  1 KeyPackageRequest  owner -> invitee           (name)
  2 KeyPackage         invitee -> owner           (MlsMessage(KeyPackage))
  3 Welcome            owner -> new member        (MlsMessage(Welcome), ratchet tree in extension)
  4 Message            sender -> every other member (MlsMessage, PrivateMessage only)
```

The sender fans every message out to each other member. The groups use the pure-ciphertext wire format, so commits are encrypted too. Since that traffic also travels inside the pairwise ratchets, the network sees nothing group-specific.

## Membership policy

Spec §6.2 leaves admin semantics to the application layer.

- **Owner.** The creator owns the group and is its only committer. Adds and removals are owner commits, and members reject commits from anyone else. Without a delivery service to order commits, this rules out epoch forks.
- **Joining.**
  1. The owner sends a `KeyPackageRequest` to the invitee.
  2. The invitee consents by returning a fresh key package. Requests from mutually approved contacts are accepted automatically; anyone else needs `/group accept`.
  3. The owner completes adds only for key packages it asked for, and only when the key package comes from the identity it invited.
  4. The owner commits the add, sends the Welcome to the new member, and sends the commit to the existing members.
- **Joining checks.** On a Welcome, the joiner checks the group id, the ciphersuite, that every member credential is valid, and that the inviter and the joiner are both members.
- **Removal.** On removal, the owner's commit goes to every member, including the one removed. The removed member's group becomes inactive, and later epochs are unreadable to it.
- **Past epochs.** Application messages from up to five past epochs are still accepted, to tolerate reordering around commits.

## CLI

```
/group new <name>                 /group invite <group> <peer>
/group accept [n]                 /group remove <group> <peer>
/groups                           /g <group> <text>
```

## Not yet done

- **Persistence.** Group state lives in openmls's in-memory store and is lost when the node exits. Next step: persist the storage provider, sealed with the identity passphrase.
- **Store-and-forward.** Members must be online when a message is sent. Relaying through other members would fix this, and it fits the planned mesh relay.
- **More committers.** Admin roles beyond a single owner, and self-removal ("leave" proposals committed by the owner).
- **Ciphersuite version.** openmls's X-Wing HPKE implements X-Wing draft-06, while the 1:1 protocol uses draft-11. Both are hybrid; they will converge once the MLS PQ ciphersuite draft settles.
