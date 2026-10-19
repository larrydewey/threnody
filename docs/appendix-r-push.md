# Appendix R: Waking a Sleeping Device with UnifiedPush (v1)

Status: specified here. The protocol (`AppMessage::Push`, feature bit 16384), the endpoint store, the paced wake-ups and the wake-up sync are implemented in `threnody-core::push` and `threnody-net::push`, with FFI calls in `threnody-ffi`. The Android app has it as an opt-in setting. Not yet tested between two phones.

## The problem

Threnody has no server, so nothing can tell a phone that a message is waiting. The Android app stays reachable the only way it can: a foreground service keeps the node running with a permanent notification, and its sessions send cover traffic at a constant rate (spec §9 layer 1). That works, but it costs battery and data, Android asks the user to accept the permanent notification, and some vendors kill the service anyway.

Phones are woken by push services instead. Google's push service (FCM) would mean a Google dependency and a Google-held record of every wake-up, which Threnody won't take. [UnifiedPush](https://unifiedpush.org) is an open standard. The user picks a *distributor* app (ntfy, NextPush, Sunup and others), and that app holds the phone's one push connection to a server the user chose or runs. Any app on the phone can get a Web Push endpoint URL from the distributor. A POST to that URL wakes the app.

This appendix lets a device sleep (no foreground service, no node running) and be woken by its contacts when something is waiting for it. It is **off unless the user turns it on**, because it tells a third party things the always-on connection does not (see *What it reveals*).

## Goals

- **Wake-ups carry nothing.** No content, no sender, no conversation, not even whether a message exists: cover wake-ups look exactly like real ones.
- **No new party learns who talks to whom.** The push server learns *that* the device is woken and *by which IP addresses*. It does not learn identities, and the sender of a message never contacts it unless the sender is the one holding the message.
- **Only mutually approved contacts can wake the device.** The endpoint goes nowhere else.
- **Bounded cost.** A contact can't drain the device's battery: wake-ups are paced by the contact, and the device can withdraw the endpoint.
- **No change for devices that don't use it.** Linux and the CLI keep running and need no push.

## Who wakes whom

The party that knows something is waiting is the one that holds it. Two parties can hold a message for a sleeping device B:

1. **A mailbox (Appendix H).** A mutually approved contact R that accepted a sealed message for B. R holds it until B connects.
2. **The sender itself.** A message to B that no session, relay or mailbox could take waits in the sender's delivery queue until a session with B comes up (Appendix N, *Fallback*). With two people and no third contact online, this is the only holder.

Either one wakes B by POSTing to B's endpoint. B wakes, starts its node, connects to its contacts, receives what they held (mailbox deliveries and queued messages flow as on any new session), and sleeps again.

Volunteer relays (Appendix P) don't hold mailboxes today, so they never wake anyone. If they hold mailboxes later, they would get the endpoint the same way, from the device over an encrypted session, and follow the same rules.

## Handing out the endpoint

```
Push    = { 0 => 30, 2 => bstr .cbor PushReg }      ; AppMessage kind 30, Hello feature bit 16384
PushReg = { ? 1 => tstr,                             ; endpoint URL (≤ 1024 bytes)
            ? 2 => bstr .size 65,                    ; p256dh: the device's P-256 key, uncompressed (RFC 8291)
            ? 3 => bstr .size 16,                    ; auth secret (RFC 8291)
            ? 4 => uint,                             ; cover wake-ups per day the device asks for (0..24)
            ? 5 => uint }                            ; expires_ms: when the contact should forget it
```

An empty map withdraws the endpoint. Keys 2 and 3 come together or not at all, and only with key 1.

- **Who gets it.** A device sends its `Push` once per session, inside the ratchet, and only to a mutually approved peer whose `Hello` has bit 16384. That includes its own other devices, which are mailboxes too. When it turns push on or changes its endpoint, it sends the new one to every such peer connected at the time; the rest get it at their next session. After it turns push off, it sends the withdrawal (an empty `PushReg`) at the start of every session, so contacts that still have the endpoint learn to forget it.
- **What the contact checks.** It accepts a `Push` only from a mutually approved peer. The URL must be `https://` with a host and no user info, or `http://` to a loopback address (tests, or a distributor on the same machine). Anything else is dropped.
- **How long it is kept.** Until `expires_ms`, capped at 30 days from receipt. The device sets 30 days from sending, and since it resends at every session, the endpoint stays fresh while the two talk. A contact also forgets it the moment approval stops being mutual, and when the push server answers 404 or 410 (the endpoint was unregistered).
- **Storage.** In the node's encrypted state, keyed by the device's identity key, like mailbox contents.
- **Anonymous identities never take part.** A persona node refuses to set an endpoint and ignores `Push` from its contacts. One distributor serving both the main identity and a persona would tie them together: the push server sees one phone with two endpoints, each woken by the other's contacts. A persona's messages arrive whenever the main identity's node wakes (the app opens every identity's node when it starts) or when the app is opened.

### Rotation

The endpoint is an identifier: every contact that has it can ask the push server about it, or recognise it. The Android app gets a new one (unregisters and registers again) every 30 days, and whenever the setting is turned off and on. Contacts get the new one at their next session, and the old one stops working at once (the push server answers 404 or 410 and contacts forget it).

Contacts could compare endpoints to learn that they share a contact, but they already know that: the endpoint goes only to contacts that have the device's identity key anyway.

## The wake-up

A wake-up is one HTTP POST to the endpoint:

```
POST <endpoint>
TTL: 86400
Urgency: high
Content-Type: application/octet-stream
Content-Encoding: aes128gcm                        ; only when the device gave keys
<body>
```

- **The body.** 32 random bytes. When the device gave Web Push keys, they are encrypted to it as one RFC 8188 `aes128gcm` record, as RFC 8291 specifies: a fresh ephemeral P-256 key per wake-up, salt, record size 4096, then the ciphertext with the `0x02` delimiter and no padding. Every wake-up is 135 bytes then (32 bytes without keys). The distributor's decryption and the app see random bytes, and the app ignores them.
- **Nothing else.** No user agent, no cookies, no VAPID. VAPID would sign every request with a key of the sender's, which would let the push server link wake-ups from the same contact to each other and across devices. Push servers that insist on VAPID (some gateways onto FCM do) won't deliver these wake-ups; ntfy, NextPush and Sunup accept them.
- **TTL.** The push server keeps a wake-up for up to a day if the phone is unreachable (switched off, no signal), so the phone still syncs when it comes back.
- **Answers.** 2xx: sent. 404 or 410: the endpoint is gone; forget it. 400, 401, 403 or 413: the server will never take these; forget it too. Anything else (429, 5xx, no connection): don't wake this device again for 10 minutes.

### Pacing

Each contact paces wake-ups per device, whatever their cause:

- **Jitter.** A wake-up goes after a random wait of 2 to 20 seconds, so its time says less about the moment the message arrived. Every reason to wake the device in that time shares the one wake-up.
- **Spacing.** At least 60 seconds between two wake-ups of one device. A reason that comes sooner is put off until the gap has passed, not dropped.
- **Cap.** At most 20 wake-ups of one device per hour, cover included. Past that, reasons are dropped: what is held stays held and arrives at the next wake-up or session.
- **Pointless ones are skipped.** At send time, a contact that has a session with the device again, or is no longer mutually approved, sends nothing.

Together these bound what a misbehaving or compromised contact can do to the device's battery: 20 wake-ups an hour, until the user withdraws the endpoint or blocks the contact.

### Cover wake-ups

A contact also sends cover wake-ups to each device that gave it an endpoint, at random times with exponentially distributed gaps (a Poisson process) averaging the rate the device asked for in key 4. The apps ask for 4 a day. They are paced and sent exactly like real ones: same budget, same jitter, same size, same headers. The device can't tell them apart either, and doesn't try: it syncs on every wake-up.

They exist so that a wake-up does not prove a message. Without them, the push server would learn the device's message pattern from wake-up times. With them it learns an upper bound. Contacts only send them while their own node runs, which for a phone that sleeps is rarely, so most cover comes from contacts that are always on. That is the same set of contacts that hold most mailboxes.

## On the device

The device registers with its distributor, hands out the endpoint, and stops its always-on connection while the app is in the background. On a wake-up it:

1. starts its node (the Android app runs a short foreground service, allowed for a few seconds after a push),
2. dials every mutually approved contact (`wake_sync`),
3. waits until every contact is connected and things have been quiet for three cover intervals (at least 3 seconds), or until some are connected and nothing new has connected for 15 seconds, or at most 60 seconds,
4. shows notifications for whatever arrived, and sleeps again.

While it sleeps it is nobody's mailbox, and its own contacts can only reach it by waking it.

## What it reveals

Compared with the always-on connection, push trades metadata for battery. The trade, stated plainly:

| Who | Learns with push on | Does not learn |
|---|---|---|
| The push server (and whoever can see its logs) | That this phone (by its endpoint) is woken, when, and from which IP addresses. Over time, the set of IP addresses that wake it: those of the contacts that hold mailboxes for it or write to it. | Identities, contents, senders, whether a given wake-up is real. |
| The distributor app on the phone | That Threnody is installed and gets pushes, and when. | Everything else (the body is random). |
| A network observer near the phone | That the phone talks to a push server and when it wakes, instead of seeing constant cover traffic. Sessions begin after wake-ups, so it sees when the phone is reachable. | The same things it can't see today. |
| Each mutually approved contact | The endpoint URL, so the push server's address, and so roughly which push provider the user picked. | Nothing more about other contacts. |
| A contact that misbehaves | It can wake the phone up to 20 times an hour. | — |

Weighing it:

- **The push server can map contacts' IP addresses.** This is the main cost. A contact's IP address is not its identity, but the two are often linkable (a home connection, a fixed server). Users who care should run their own push server (ntfy is a single binary) or use one they trust, and keep push off when that's not possible. Routing wake-ups through onion circuits would hide the contacts' addresses, but Threnody circuits end at Threnody nodes, not at web servers. That is listed under *Not yet done*.
- **Timing correlation.** Someone who sees both a sender's traffic and the push server's logs can match a message to a wake-up, within the jitter. Cover wake-ups and jitter make this a statistical attack over many messages, not a certainty for one. Constant-rate cover traffic, the default, defeats it completely, which is why push is opt-in.
- **Reachability.** A sleeping phone depends on its contacts being up when it wakes. A message whose holder goes offline before the phone wakes waits until both are up again. The always-on connection has no such gap.

The project rule holds: the always-on connection, with cover traffic, stays the default. Push is a setting the user turns on, explained where it is turned on.

## Android

- The setting *Wake up with UnifiedPush (no permanent notification)* under Privacy & Security is off by default. Turning it on explains the trade above, then asks the UnifiedPush connector library (`org.unifiedpush.android:connector`) for the user's default distributor. With none installed, it says so and stays off.
- With an endpoint, the app stops its foreground service a minute after it goes to the background (not during a call), then shuts its nodes down and ends its process. Nothing runs until the next wake-up or until the user opens the app.
- On a wake-up, the connector's `PushService` starts the foreground service in wake mode (a brief "Checking for messages" notification). That brings the nodes up, runs `wake_sync`, and sleeps again if the app is still in the background.
- Turning the setting off unregisters the endpoint, withdraws it from contacts, and brings back the always-on service.

## Linux and the CLI

Nothing changes. Desktop and server nodes run all the time and are the contacts that hold mailboxes for sleeping phones and wake them. They need the endpoint store and the wake-up sender, which are in `threnody-net`, not a push client of their own.

## Not yet done

- **Wake-ups through a proxy or circuit**, to hide the contacts' IP addresses from the push server. That needs an exit to the web, which Threnody circuits don't have.
- **One endpoint per contact.** UnifiedPush allows several registrations per app. Giving each contact its own would let the device tell which contact woke it, and revoke one without rotating for all. But the push server would see more endpoints on one phone, which is no worse, and the app would have more registrations to manage.
- **iOS.** Apple's push service has no open equivalent. An iOS app would need APNs and its own relay.
- **Field tests** between two phones, and with push servers other than ntfy.
