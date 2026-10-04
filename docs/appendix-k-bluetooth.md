# Appendix K: Bluetooth LE Transport (v1)

Status: implemented on Linux (`threnody-cli::ble`, using BlueZ through `bluer`) and Android (`apps/android`, using platform L2CAP sockets bridged through `threnody-ffi`'s `ByteLink`). Fulfils spec §7.2's Bluetooth Low Energy transport.

## Advertising

A listening device opens an **LE L2CAP connection-oriented channel** and advertises:

```
service data UUID 7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01  →  beacon (146 bytes)
beacon = Appendix E beacon with port := PSM, and exactly 8 tag slots
```

The beacon is the private LAN beacon of Appendix E, with two changes:

- **Port field.** It carries the L2CAP PSM, which the stack assigns dynamically.
- **Tag count.** It always has 8 tag slots, so it is 16 + 2 + 8 × 16 = 146 bytes and fits one extended advertising PDU. With more than 8 approved peers, each beacon carries a fresh random subset of 8 of them, so every peer's tag still turns up regularly.

A beacon does not fit a 31-byte legacy advert, so both platforms use **extended advertising**: BlueZ with `SecondaryChannel = 1M`, and Android with `AdvertisingSet` in non-legacy mode. Scanners must accept non-legacy adverts. On Android that means `ScanSettings.setLegacy(false)`.

The advert carries no name and no identity. Nodes replace it at least once a minute, so the nonce, the tags and (on Android) the resolvable private address all change.

## Connecting

### Automatic

Nodes with Bluetooth on scan continuously. When a beacon matches a mutually approved contact, they apply the dialing rule of Appendix E:

- The side with the smaller identity key dials, pinning the peer's fingerprint.
- Neither side dials a peer it is already connected to.
- A node waits 30 s before redialing the same peer.

Bluetooth reception is often one-sided. For example, some controllers stop advertising while an old link lingers. So there is a fallback: if a node has heard a peer for **45 s** without the two being connected, it dials, whichever key is smaller. Not hearing the peer for 2 minutes restarts that clock.

### Manual

A scan lists every Threnody advert with the PSM read from the beacon's port field, which needs no key. A user can then connect to one of them, for example to meet a new contact. The scanner connects to the advertised PSM, and the two ends run the ordinary Threnody handshake and session over the channel's byte stream (Appendices A and B). The session is authenticated exactly as over TCP: by fingerprint pinning, trust on first use, or approval policy. It shows up as transport `ble`.

The Bluetooth channel itself is **insecure** at the Bluetooth layer (no pairing, no link-layer encryption). That is deliberate, because the Threnody handshake provides post-quantum, mutually authenticated encryption end to end, and Bluetooth pairing would add neither security nor convenience.

### Implementation notes

- **Linux.** A non-blocking LE L2CAP `connect` can report success before the channel exists, after which writes fail with `ENOTCONN`. The client waits until the channel's send MTU is readable (about 0.8 s on the test hardware) before starting the handshake.
- **Linux scanning.** Scans list only devices actually heard during the scan, because BlueZ also returns cached entries with stale PSMs.
- **Linux listening.** `threnody run --ble` binds an LE L2CAP listener on a dynamic PSM and registers the advert with BlueZ's `LEAdvertisingManager1`. While an old LE link to a peer lingers (the peer's Bluetooth stack can hold it after the app has gone), some controllers stop sending the connectable advert. The next session works once that link is dropped.
- **Android.** `listenUsingInsecureL2capChannel()` (API 29+) and `BluetoothLeAdvertiser`, with the `BLUETOOTH_CONNECT` and `BLUETOOTH_ADVERTISE` runtime permissions. Each accepted socket is bridged into the node through `ByteLink` / `LinkHandle`.
- **Linux scanning for contacts.** BlueZ discovery with duplicate data on reports each advert. Every device is checked at most every 5 s.
- **Android dialing.** A `BluetoothLeScanner` scan filtered on the service-data UUID collects adverts (`BLUETOOTH_SCAN` with `neverForLocation`), and `createInsecureL2capChannel(psm)` opens the channel. Without `BLUETOOTH_SCAN` the app still listens and advertises, so contacts can reach it. LE connection setup sometimes fails with HCI status 0x3e ("connection failed to be established"), so the app makes up to three attempts.
- **SDU size.** Android hands an L2CAP SDU (Service Data Unit) to the app only once the whole SDU has arrived, and stalls on large ones. On a Pixel 8a a 10 KB SDU arrived and a 33 KB one never did. Both ends therefore cut writes to **4096 bytes**. Linux also offers a 4096-byte receive MTU, up from the default 672.
- **Android writes.** `BluetoothSocket.write` on an L2CAP channel silently truncates a write larger than one packet, so the app writes packet-sized pieces.
- **Lingering links.** When a peer's app dies, its LE link can stay up. While it does, the laptop's controller stops advertising. Every 10 s, Linux therefore disconnects LE links to devices it has run Threnody channels with that no longer carry a session, after a 30 s grace period for handshakes. Other Bluetooth devices are never touched. In testing, a link left by force-stopping the app was gone within 8 s, and the app reconnected 9 s after it restarted.
- **Android background.** A foreground service of type `remoteMessaging` keeps the process, and so the node and its Bluetooth sessions, alive while the app is in the background. Messages that arrive while no Activity is visible raise a notification.

## Tested

A Pixel 8a (Android API 37) and a Linux laptop (BlueZ 5.87), in both directions:

- The laptop found the phone's advert, opened the channel and completed the handshake, and messages went both ways.
- The phone found the laptop's advert (`run --ble`), dialed it, and messages went both ways.
- With the app in the background, and again after three minutes with the screen off, a message from the laptop arrived over the existing Bluetooth session and raised a notification.
- **Automatic connection.** With the laptop on `run --ble` and the app open, the phone recognised the laptop's beacon and connected without any taps.
- **Reconnection.** After the laptop node restarted, the session came back within 15 s, and a message sent with the app in the background raised a notification.
- **Fallback.** With the phone's scan permission revoked, the laptop recognised the phone's beacon and dialed it through the fallback rule. The first attempt timed out and the redial connected, about 2.5 minutes after start.

## Privacy

To anyone without a discovery key, the advert shows only that *a* Threnody device is nearby, plus its PSM. Beacons are unlinkable to each other, and the phone's Bluetooth address rotates. Only mutually approved contacts can recognise the device. Its presence is still visible: hiding that would mean dropping the fixed service UUID, so that scanners have to test every extended advert they hear.

## Not yet done

- Android listening that survives the Activity being destroyed by the user (the listener is process-wide, but starting it needs the Activity).
