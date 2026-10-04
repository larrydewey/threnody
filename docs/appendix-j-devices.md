# Appendix J: Accounts and Multiple Devices (v1)

Status: specified here, modelled in [`proofs/devices.spthy`](../proofs/README.md), implemented in `threnody-core::account` and `threnody-net::account`. Fulfils spec §4.5: devices are cryptographically bound to an identity, easy to add and revoke, and the loss of any one device costs nothing.

Every device keeps its own Ed25519 key. Sessions, ratchets, tunnels, prekeys and relays stay per device, exactly as before. An **account** groups devices under one stable identity.

## Account chain

An account is a hash-linked list of signed changes to its device set:

```
Link   = { 0: seq u64, 1: prev (32), 2: threshold u8, 3: action, 4: [ Sig ] }
action = { 0: 1 add,    1: device key (32), 2: name tstr }
       / { 0: 2 remove, 1: device key (32) }
Sig    = { 0: device key (32), 1: signature (64) }
body   = Link without field 4
sig    = Sign(device, L("account link") || L(body))
prev   = H(previous link body)     -- 32 zero bytes for the genesis link
AccountId = KDF("account id", body of link 0)
```

### Validity

A chain is valid if, applying links in order:

1. **The genesis link** (seq 0, zero `prev`) adds exactly one device and is signed by that device. Its threshold is the initial threshold.
2. **Every later link** has `seq = previous + 1` and `prev = H(previous body)`.
3. **Authorisation.** Every later link is signed by at least `threshold` distinct devices in the device set *before* the link. The threshold is the one stated in the previous link.
4. **Add** also needs a signature from the device being added (proof of possession and consent), and that device must not already be in the set.
5. **Remove** names a current device and may not leave the set empty.
6. **No re-adding.** A device that has been removed can never be added again.

v1 always writes threshold 1, so every current device is an equal peer. Verifiers already enforce thresholds above 1, so raising it later needs no change to the format.

### Account fingerprint

The account fingerprint is computed the same way as a device fingerprint (Appendix A), with `AccountId` in place of the identity key. Invite links and safety numbers use account fingerprints. A single-device account is just a chain with one link.

### Updates and forks

- **Exchange.** Peers send their chain as `AppMessage::Account` (kind 11) at the start of every session, and again whenever it changes.
- **Accepting a chain.** A receiver accepts a chain only if it is valid, its id matches what the receiver has stored for that account (pinned at first sight), and the session's device is in its current set.
- **Updates.** A newer chain must *extend* the stored one. A different chain at or below the stored height is a **fork**: the receiver rejects it and warns, because forking requires a compromised device.
- **Removal.** When a chain removes a device, every peer drops its sessions with that device, revokes its approvals, deletes its discovery key and prekeys, and refuses it from then on.

## Linking a device

```
existing device E                                  new device N
  code = threnody-link://<fp(E)>@<addr>#<s>   ──(out of band: QR / typed)──▶
                                              ◀──  session (handshake; N pins fp(E))
                                              ◀──  LinkRequest { proof = KDF("link proof", s, session_id, N), name, sig_N(body) }
  check proof; build link Add(N, name), signed by E (and N's sig over the same body)
                                              ──▶ LinkAccepted { chain, contacts }
```

- **The secret.** `s` is 16 random bytes, good for one use and 10 minutes.
- **Authenticating each side.** The code pins `fp(E)`, which authenticates E to N. The proof binds `s` to *this* session and *this* device, which authenticates N to E. An attacker without the code can't be added, and can't replay a proof into another session.
- **Signing.** N's signature over the link body is computed in advance from `prev`, `seq` and its own key, both of which E sends first (`LinkOffer`).
- **Aftermath.** E then broadcasts the new chain to every session.

## Own devices

- Devices in the same account approve each other automatically.
- They exchange `ContactSync` snapshots at the start of each session and after changes. Approval and verification flags merge last-writer-wins on `approval_changed_ms`, and petnames and addresses fill in where missing.
- Message history isn't synced in v1.

## Contacts and messages

- **Grouping.** A contact entry is still per device, and records the account the device belongs to.
- **Approval.** Approving an account approves all of its devices. Devices that appear later inherit the account's approval.
- **Sending.** A message to an account goes to every device: over live sessions where possible, otherwise sealed (Appendix H) for each device with a prekey bundle.
- **Display.** Incoming messages show the account, with the sending device's name.

## Properties (proved in `proofs/devices.spthy`)

- **Chain authority.** Every device in an account's current set was added by a link signed by a device already in the set (and by itself), unless one of those signing devices was compromised. An attacker who controls no device can't add one.
- **Link codes.** A device is added through the link protocol only if it is the device that received the code over the out-of-band channel. A code captured off the network is useless without `s`, and a proof is bound to one session.

## Not yet done

- Gathering co-signatures for thresholds above 1, so removals need several devices.
- Groups across devices. MLS leaves stay per device, so add each device to a group separately.
- Syncing message history between devices.
