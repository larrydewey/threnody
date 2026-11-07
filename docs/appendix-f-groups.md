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
GroupWire = { 0: kind, 1: group_id (16), ? 2: payload, ? 3: name, ? 4: member (32), ? 5: reference,
              ? 6: owner (32), ? 7: [* proposal], ? 8: sync hint }
  1 KeyPackageRequest  inviter -> invitee         (name)
  2 KeyPackage         invitee -> inviter         (MlsMessage(KeyPackage))
  3 Welcome            coordinator or inviter -> new member
                                                  (MlsMessage(Welcome), ratchet tree in extension)
  4 Message            sender -> every other member (MlsMessage, PrivateMessage only;
                       7 = the proposals a coordinator's commit references, 8 = the sender's hint)
                       proposer -> coordinator    (a proposal)
  5 Forward            sender -> a reachable member (MlsMessage, member = the one to deliver it to,
                       reference = the sender's id for a receipt, or absent; 7 and 8 as for 4)
  6 Receipt            forwarder -> sender      (member = who acknowledged, reference)
  7 Leave              member -> coordinator    (remove me; payload = our own Remove proposal,
                                                 in a group with a roster)
  8 AddDevice          member -> coordinator, and (member = another device of the sender's account)
                       member -> that device    (and owner = the group's coordinator)
  9 PassWelcome        coordinator -> proposer  (payload = the Welcome, name, member = the joiner)
 10 Resync             stale member -> member, member -> coordinator
                                                (8 = the stale device's hint, member = it when relayed)
sync hint = [epoch, term, auth (8), lead (8)]   ; epoch authenticator and coordinator, first 8 bytes each
```

Receivers ignore keys they don't know, so older members skip `owner`, the attached proposals and the hint. Keys 7 and 8 appear only in groups with a roster, so the messages of other groups are byte-for-byte as before.

The sender fans every message out to each other member. The groups use the pure-ciphertext wire format, so commits are encrypted too.

### Message content

An MLS application message carries one of these:

```
Content = text                         ; UTF-8, as the first versions sent (no id)
        / 0xFF || { 0: kind, ? 1: text, file name or emoji, ? 2: file data,
                    ? 3: flags, ? 4: caption, ? 5: album id, ? 6: message id,
                    ? 7: clip duration in ms, ? 8: quoted message id,
                    ? 9: quoted sender's device, ? 10: quoted text,
                    ? 11: mentions [* [start, len, who (32)]] }
kind: 1 text, 2 file (at most 8 MiB, as in 1:1), 3 reaction, 4 pin (6: message id; flag 1 unpins),
      5 poll (1: question, 12: [* option], flag 1 several choices), 6 vote (6: poll id, 13: [* choice]),
      7 close poll (6: poll id), 8 list (2: the list, Appendix C *Lists*),
      9 call (2: `GroupCallMsg`, Appendix Q *Group calls*; older members ignore it)
flags: 1 sensitive, 2 voice message, 4 video message, 8 forwarded (file); 8 forwarded (text);
       1 remove (reaction)
```

A UTF-8 string never starts with the byte 0xFF, so plain text from older members still reads as text. Text and files carry the sender's id for the message, which receivers record (history key 11). A reply quotes a message by that id, its sender and its start, as in 1:1 chats (Appendix C, *Replies*). A text may mention members: each mention covers characters `start..start+len` of the text (as written, "@Name") and names who (an account, or a device whose account isn't known, as reactions do). Receivers keep mentions that fit the text, in order and apart, at most 32, store them (history key 16: `[* [start, len, who]]`), and show their own name for each. A message that mentions us, or replies to one of ours, counts as mentioning us. Each conversation has a notification setting (history key 4: 0 all, 1 only mentions and replies to us, 2 off), on each device; messages still arrive and count as unread. A poll is a message with a question (at most 300 characters) and 2 to 12 options (at most 100 each). Members vote by option index; a vote replaces the voter's last one, and no choices take it back. A single-choice poll keeps one choice. Votes count per voter as reactions do (account, or device), and are stored with the poll (history key 17: `[options, multi, closed, [* [voter, [* choice]]]]`). Only the poll's author (any device of their account) closes it, and a closed poll takes no votes. A pin names a message by that id too. Members apply it only if the sender is the owner or an admin, and keep at most three, as in 1:1 chats (Appendix C, *Pinned messages*). A reaction names a message by that id; members apply it under the reactor's account, as in 1:1 chats (Appendix C). Receivers save a file and record it in the group's history with where they saved it. Since that traffic also travels inside the pairwise ratchets, the network sees nothing group-specific.

### Reaching every member

Members need not be contacts of each other, or online together. Each copy goes to its member by the first of these that works:

1. The live session with that member. Sessions acknowledge group messages end to end, and resend them in the next session if the current one dies first (Appendix C, *Acknowledgements*).
2. Sealed for the member's mailboxes (Appendix H), when we hold their prekeys.
3. Forwarded by another member we have a session with: the coordinator first, then the owner's other devices, then admins. The `Forward` carries the same MLS ciphertext and names the member to deliver it to. This applies to MLS messages only, never to key packages or Welcomes. A member being welcomed by the same change (an add's commit goes out with its Welcome) is never the forwarder: it doesn't know the group until its Welcome is in, and would drop the message.
4. Held by us and sent when the member next connects. Meanwhile we try to reach them through relays (Appendix G).

A forwarder checks that the sender and the target are both members of the group as it knows it, and that the target is neither itself nor the sender. It then delivers a plain `Message` by steps 1, 2 and 4, and never forwards again, so a message crosses at most one forwarding member. Since the owner invited everyone, it usually has a session with every member, and a member that only ever talks to the owner still reaches the whole group.

Held messages are kept encrypted under the identity (`group-held` state): at most 200 and 32 MiB per member and 128 MiB in all. Any member can have a forwarder hold what it sends, so these limits bound what one member can make another keep. Past a limit, the forwarder drops from whoever has it hold the most (counting itself for its own messages): their oldest application message, and only when they have none left, their oldest commit, proposal, Welcome (or one to pass on), `Resync` or `Leave` carrying its proposal. A member needs every commit to read anything after it, and there is no resync yet, so files sent to an offline member go before the commit that added it, and a member flooding the forwarder through `Forward` only loses its own messages. Anyone can tell a commit from an application message, as MLS leaves the content type in the clear. Dropped messages are not reported. Pending invitations are kept the same way (`group-invites`). A message held for longer than five epochs' worth of membership changes can no longer be decrypted (see *Past epochs*).

Delivery ticks (Appendix C, *Acknowledgements*) count only a member's own acknowledgement. So when the sender hands a copy to a forwarder, it includes a reference. Once the member acknowledges the copy the forwarder delivered (live, or later from its hold), the forwarder sends the sender a `Receipt` naming the member and the reference. The sender believes receipts only from members, and only about members. A member could claim a delivery that didn't happen, but that only affects the ticks, and members can already drop messages they forward.

A forwarder learns only that one member is sending something to another, and the size and time. Both are members, so it could read the content anyway. Like any relay, it can drop or delay what it holds, but it cannot alter it: MLS authenticates every message. The membership checks keep a member from being used to send to non-members.

## Membership policy

Spec §6.2 leaves admin semantics to the application layer.

### Roles and the roster

- **Owner.** The creator owns the group: all of its account's devices in the group are owner devices. One of them is the *coordinator* (the lead), the only device that commits in steady state. Without a delivery service to order commits, one committer rules out epoch forks.
- **Admins** invite and remove members (not admins or owners) and pin messages. An admin may step down; only owners promote, demote, choose a successor or hand the group over.
- **Successor.** The owner may name an admin account (its devices in the group) that can take ownership should the owner disappear. There is none by default.
- **The roster.** Roles live in the group's MLS GroupContext, as a private-use extension (`0xFA01`, RFC 9420 §17.3): `{ 0: 1, 1: term, 2: lead, 3: [owner devices], 4: [admin devices], ? 5: [successor devices] }`, deterministic CBOR, lists sorted, a term below 2^32 - 1 (so another can always follow), at most 16 owners and 80 admins (64 promoted, plus up to 16 former owners after a transfer or claim). Every member holds the same roster at every epoch, and it is bound into the key schedule. It is decoded strictly (the documented exception to ignoring unknown fields, since it is security-critical); a later version is read for keys 1-3 only, its group takes commits only from its lead, and the apps ask the user to update. Every roster change is a GroupContextExtensions proposal carrying both the roster and `RequiredCapabilities([0xFA01])`, because such a proposal replaces every extension.
- **Capability.** Every leaf (new groups and key packages) advertises `0xFA01`: the authenticated, group-wide sign that a member enforces roster rules. Once a roster is installed, openmls refuses a key package without it, and so does the inviter (`NeedsUpdate`): old clients can't join a group with admins.
- **Groups without a roster** (all groups made before this, and any group until its owner first changes a role) work as before: the creator (or, for a joiner, whoever made its Welcome) is the only owner and commits alone.
- **Checks.** Every member checks every commit against the roster before it. The coordinator's commits may reference proposals only if their proposers may make them (adds by owners and admins; removes by owners of anyone but the coordinator, by admins of members, and by anyone of themselves; updates of the proposer's own leaf; roster changes by owners, or an admin stepping down); the term stays the same unless the coordinator changes, when it goes up by one. An owner device that isn't the coordinator may commit only to take over (it becomes the coordinator, the term goes up by one, and it references no proposals). A successor device may commit only its claim (below). The roster after must name only members. Anyone else's commit is refused, and its attached proposals are taken back out of the store. Pins count only from owners and admins, on every member.
- **Privacy.** The roster shows members which devices are the owner's (and, when the coordinator can tell, an admin's), which non-contacts couldn't tell before. It exists only after the owner's first role change, and the apps' "Make admin" confirm says so. Personas can't link devices, so a persona owner reveals nothing more.

### The coordinator

The coordinator commits its own changes directly, and everyone else's as soon as their proposal reaches it. A commit carries, in key 7, the proposals it references, so every member can store them and check their senders before merging. Attachments on anything but a commit are ignored, never stored. The coordinator keeps at most 32 proposals an epoch, 8 from one proposer; past that it drops them. When it commits, it leaves out a stored proposal its proposer may no longer make; if the commit still fails, it keeps the stored proposals that go in together, oldest first, and drops the rest, whose proposers propose them again after its next commit. An owner device taking over leaves its own pending handover or coordinator change for its next commit, as the coordinator. A proposal for an older epoch (held or mailboxed across a commit) or sent to a device that isn't the coordinator is dropped quietly, not reported as an error. Commits are always built with an explicit choice of whether to reference stored proposals; in a group without a roster they never do, since older members don't store proposals. When a commit removes a device named in the roster, the coordinator also drops it from every role in the same commit; if a stored roster change is pending, its own change carries both. Each commit is checked by the coordinator as every member will, before it is merged.

Welcomes are made by the coordinator only. For an add someone else proposed, the coordinator sends the Welcome to the proposer (`PassWelcome`), who passes it on; the proposer accepts it only from an owner device, and only for a member it proposed or that is now in the group. The joiner takes a Welcome only if its MLS sender is the lead in the Welcome's roster, and only from someone it gave a key package to who is that lead, an owner or an admin. In a group without a roster, the Welcome's sender must be whoever passed it on, as before, and becomes the owner.

### Proposals and intents

Admins and owner devices that aren't the coordinator send their changes as MLS proposals (Add, Remove, GroupContextExtensions) to the coordinator only. A change of ours is kept as an *intent* until a commit includes it. After every commit, intents that are now true are dropped, and the rest are proposed again at the new epoch (an add reuses its key package); after 8 attempts the change is given up and the apps say so (`GroupChangeFailed`). Two admins' changes at the same epoch therefore never conflict: one lands, the other is proposed again. While the coordinator is offline, changes wait (held, mailboxed or forwarded like any message) and the apps show how many.

openmls refuses to send an application message while proposals are pending, so that a client doesn't keep talking in an epoch about to change. Here a proposal may wait days for an offline coordinator, so members set their pending proposals aside while they encrypt a message and put them back after: they keep chatting, reacting and pinning. Until the coordinator commits a remove, its target is still a member and reads what is sent, just as before an owner got around to removing someone.

Messages for a later epoch than ours (a new member's first message before the commit that added it, a commit before its predecessor) wait, with their attachments, until the commit that gets us there. Anyone who knows a group's id can send one, so: none more than 32 epochs ahead, at most 32 from one sender, at most 64 per group and 2 MiB (past that, those from non-members are dropped first, then the oldest), and 8 MiB across all groups (past that, non-members' go first, then nothing more is kept).

### Upgrading existing groups

A group without a roster gets one when its owner first changes a role, and only once every leaf advertises the capability:

1. If the coordinator's own leaf predates roles, it first commits an update of its own leaf. Older members take it: it's an owner commit without proposals.
2. Members running this version whose leaf predates roles then propose updating it, to the coordinator only, once per epoch (and on restart, or when the coordinator connects).
3. The coordinator keeps those proposals, and once every outdated member has proposed this epoch, commits them all together. By then no member is outdated.
4. Meanwhile the role change waits, and the apps say who must update (`GroupUpgradePending`). Once every leaf supports roles, the coordinator installs the roster (its own devices in the group as owners) with the change in the same commit.

Nothing is sent to older members that they can't handle, and they are never forked. Once a group has a roster, downgrading breaks it on the downgraded device.

### Takeover, transfer and succession

- **Changing coordinator.** An owner device asks the coordinator to make it the coordinator (a roster change; the term goes up by one). An owner device can also take over directly, committing without the coordinator, along with its own pending changes; the apps offer it behind the "waiting for…" banner, and to remove a lost coordinating device.
- **Transfer.** The owner hands the group to another account: its devices in the group become the owners, the first the coordinator, and the old owners become admins.
- **Leaving.** In a group with a roster, a leaving member sends the coordinator its own Remove proposal, so every member sees it left rather than was removed. If the coordinator hasn't yet merged a commit the leaver has, the leave waits for it. An owner device or admin that gets a leave meant for a coordinator it no longer is proposes the removal itself (members then see that device removed it). In a group without a roster, leaving is a bare request as before. When the leaver's own proposal can't go in (a group without a roster, or a proposal for an epoch gone by), the coordinator removes it in its own commit and says so in the commit's MLS authenticated data: the ASCII bytes `threnody/left`, then the leaf index (4 bytes, big endian) of each member it removes because that member asked to leave. Members show such a removal as a leave. The marker only counts for the committer's own Remove proposals in that commit, which it may make anyway, so it can only change how a removal is worded. Members that predate it ignore it (the authenticated data is otherwise empty). It travels outside the encryption, as all MLS authenticated data does, but says no more than that a commit's removal was a leave, by leaf index. A coordinator running 0.19.0 or earlier doesn't add it, so a leave it commits still reads as its removal; nothing in its commit tells the two apart. The coordinator first hands over to another owner device; the last owner account must transfer the group or delete it, unless nobody else is in it. The apps' "Choose a new owner first" hands over and leaves in one step, and refuses, changing nothing, while the group still waits for members to update (a handover can't go in then) or the handover hasn't gone through.
- **Claim.** A successor device may commit, by itself, a roster in which the successor's devices own the group, it coordinates, everyone else who owned or administered stays an admin, there is no successor, and the term goes up by one. The apps offer it only after the owner has been silent in the group for 14 days (`group-owner-seen`, stamped whenever an owner device sends something, commits anything (a role change too) or connects, and set afresh when we become successor; debug builds can lower it). That rule is client policy, so a modified client could claim early; this is bounded: only the owner's chosen successor can, the old owners stay as admins and every member is told, and the apps' confirm says it can't be undone.

### Forks and repair

Only a takeover or claim racing the old coordinator, or two of them racing each other, can fork a group. Branches are ranked by `(term, lead)`: the higher term wins, and on equal terms the branch whose lead has the lower identity bytes, so two takeovers at once still converge.

Every message in a group with a roster carries the sender's sync hint (key 8): its epoch, term, and the first 8 bytes of its epoch authenticator (RFC 9420 §8.7) and lead. A member that gets a message from another member whose hint outranks its own roster, from an epoch it has passed or from its own epoch with different secrets, is on the losing side (a later epoch waits, as reordering looks alike, but if nothing explains it within 2 minutes, by the next time a peer connects, it is treated the same way; hints from non-members are ignored): once per epoch and term it sends that member a `Resync` with its own hint and shows that it fell out of step. A member receiving it passes it to its coordinator, naming the stale device. The coordinator re-adds the stale device (a remove and an add of the same device in one commit, as openmls's fork resolution does) only if its reported rank is below its own, and once per term, so a forged or replayed `Resync` changes nothing; a key package from a member it didn't agree to repair this term changes nothing either. The stale device accepts a key-package request for this without asking only because it asked to be brought back, and only from an owner or successor device of its roster. It accepts a Welcome to a group it is already in only from an owner or successor device of the roster it has, into a branch that outranks it, or from its own coordinator into a later epoch; anyone else could swap the group for a look-alike under the same id. The hint is unauthenticated: it only ever prompts a request, and every change of state still needs a Welcome checked as above.

### Joining

1. An owner or admin sends a `KeyPackageRequest` to the invitee.
2. The invitee consents by returning a key package. Requests from mutually approved contacts are accepted automatically; anyone else needs `/group accept`. Group traffic doesn't go through message requests, so the invitee keeps at most 3 waiting invitations per inviter and 32 in all, and none from a blocked contact. The oldest go first, those from strangers before those from accepted contacts (or, for a group one of our own devices is in, from its owner), since a stranger can make any number of identities. After a stranger's invitation notifies, others from strangers in the next 10 minutes are listed without notifying. Group names lose control characters and the invisible ones that reorder or hide text. As for message requests (Appendix M), the apps' notification of an invitation from someone not accepted says only "New group invitation": the group's name is the inviter's to choose. The invitee keeps the key package's private keys (in its group state) only until a Welcome uses them. The inviter may ask more than once while the add is pending and completes it with whichever answer arrives first, so another request for the same group from the same peer is answered with the same key package (a fresh one only once it has expired or been deleted). At most 16 are kept, the oldest deleted first.
3. The inviter completes adds only for key packages it asked for, and only when the key package comes from the identity it invited. Pending invitations survive a restart.
4. The coordinator commits the add, sends the Welcome to the new member (or to the admin who proposed it, who passes it on), and sends the commit to the existing members.

- **Joining checks.** The joiner takes a Welcome only from someone whose invitation to that group it accepted (gave a key package to), before it uses up the key package, and only if that group's coordinator made the Welcome (signed its group info), as above. It then checks the group id, the ciphersuite, that every member credential is valid, and that the inviter and the joiner are both members. Several people can invite us to the same group id. Accepting one of them sends a key package to that inviter only, and declining one keeps the others. The apps answer the invitation they show.
- **Linked devices.** When a device is linked into an account (or first connects to a sibling after this was introduced), each sibling has it join every group the sibling is in, once per group: it invites the device itself if it is an owner or admin, and otherwise sends the coordinator `AddDevice`. The coordinator (or an owner device or admin receiving it) invites the device only if the sender is a member and both are devices of the same account, according to the sender's account chain. The sibling also sends `AddDevice` to the device itself, naming the coordinator. The device then accepts that invitation automatically, as it does an invitation from its own sibling. Anyone else's invitation to the group still needs consent. An `AddDevice` from an older sibling doesn't name the owner, so that invitation needs consent too. In a group with a roster, a device the owner's account adds becomes an owner, and one an admin adds becomes an admin if the coordinator can tell it is the admin's own (the admin is its contact); otherwise it joins as a member, since members can't check accounts. When an admin device steps down, the coordinator steps down the account's other devices it knows. Leaving is per device, and a device that left stays out: a sibling that sees one of its account's devices leave (or be removed from) a group never adds it back, and a device ignores `AddDevice` for a group it left or declined, and asks the user again before accepting any invitation to one.
- **Removal.** On removal, the coordinator's commit goes to every member, including the one removed. The removed member's group becomes inactive, and later epochs are unreadable to it. The removed member then deletes the group and its secrets, as on leaving, so it can be invited again. When a device is removed from its account (Appendix J), it loses its role and its membership in every group, always (there is no setting): the coordinator removes it at once; an owner device or admin that may proposes it; if it was the account's coordinating device, the account's remaining owner device with the lowest identity takes over and removes it in the same commit. Every member stops delivering to it (no session, mailbox, forward or holding), including the removal commit.
- **Past epochs.** Application messages from up to five past epochs are still accepted, to tolerate reordering around commits.

## Persistence

After every change, the full openmls key-value store and the group metadata are exported to `<home>/groups.state`. The file is written mode 0600 and encrypted:

```
file = nonce (12) || ChaCha20-Poly1305(KDF("state encryption key", identity_seed), nonce, ad = "groups", export)
```

The export (`STATE_VERSION` 1) holds openmls's store (key 1, which also keeps the roster, being in the GroupContext, and every stored proposal), each group's name, coordinator and roles as older versions read them (key 2), the key packages given out (key 3), and since roles: each group's intents, the coordinator's attachable proposals, the future-epoch buffer, the epoch of our last upgrade proposal and our outstanding resync (key 4), pending invitations (key 5) and fork repairs done or under way (key 6). Older versions skip keys 4-6. When each group's owner was last seen is kept beside it, encrypted the same way (`group-owner-seen`).

The key is derived from the identity seed, so group secrets are only as accessible as the identity: if the identity is passphrase-sealed, so is everything else. State is saved *before* any resulting message leaves, so a crash can't leave us behind an epoch our peers have already moved to. If it can't be saved (a full disk, say), nothing leaves: the change fails, and the groups go back to their last saved state, where the peers still are. An invitation that would have been accepted automatically then waits for consent instead.

## CLI

```
/group new <name>                 /group invite <group> <peer>
/group accept [n]                 /group decline [n]
/group remove <group> <peer>      /group leave <group>
/group delete <group>             /group admin <group> <peer>
/group unadmin <group> <peer>     /group owner <group> <peer>
/group successor <group> <peer|none>
/group claim <group>              /group coordinate <group>
/groups
/g <group> <text>                 /gfile <group> [-s] <path>... [| caption]
```

`/groups` shows each member's role, the coordinating device and our changes still waiting for it. `<peer>` may name any member, contact or not. `/group unadmin` with our own name steps down.

Membership changes are worded from where they're seen: `you removed carol from G` on our devices, `carol was removed from G by bob` elsewhere, `carol left G` when she asked to leave (whoever committed it), and `you are now an admin in G` for our own role. The apps show the same, as a notice while the group is open, except for a change made on that device.

## Not yet done

- **Secure deletion.** Each save rewrites the whole state file atomically. Old epoch secrets are gone from the file system's view, but not necessarily from the storage medium (spec §16 secure deletion).
- **Member key updates.** Only the coordinator commits, so only its leaf key changes regularly, with each commit. Members update their leaves only to upgrade a group for roles, so a member's leaf key otherwise stays the same for as long as it is in the group. Until they do, groups give members forward secrecy but not post-compromise security (spec §3.3): someone who once copied a member's group state and identity seed can read every later epoch until that member is removed. In a group with a roster, members could now send update proposals for the coordinator to commit; they don't yet.
- **Coordinator on an admin's device.** Admin changes wait while every owner device is offline. The coordinator could one day be a trusted admin's device too.
- **Ciphersuite version.** openmls's X-Wing HPKE implements X-Wing draft-06, while the 1:1 protocol uses draft-11. Both are hybrid; they will converge once the MLS PQ ciphersuite draft settles.
