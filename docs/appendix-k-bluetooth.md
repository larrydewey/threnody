# Appendix K: Bluetooth LE Transport (v1)

Status: implemented on Linux (`threnody-cli::ble`, using BlueZ through `bluer`) and Android (`apps/android`, using platform L2CAP sockets bridged through `threnody-ffi`'s `ByteLink`). Fulfils spec §7.2's Bluetooth Low Energy transport.

## Advertising

A listening device opens an **LE L2CAP connection-oriented channel** and advertises:

```
service data UUID 7e9f0e1c-3b5a-4c7e-9d2a-5f1e8b6c4a01  →  PSM (u16, little-endian)
```

The advert carries no name and no identity. The PSM is assigned dynamically by the stack. Android advertises from a rotating, resolvable private address.

## Connecting

The scanner connects to the advertised PSM, and the two ends run the ordinary Threnody handshake and session over the channel's byte stream (Appendices A and B). The session is authenticated exactly as over TCP: by fingerprint pinning, trust on first use, or approval policy. It shows up as transport `ble`.

The Bluetooth channel itself is **insecure** at the Bluetooth layer (no pairing, no link-layer encryption). That is deliberate, because the Threnody handshake provides post-quantum, mutually authenticated encryption end to end, and Bluetooth pairing would add neither security nor convenience.

### Implementation notes

- **Linux.** A non-blocking LE L2CAP `connect` can report success before the channel exists, after which writes fail with `ENOTCONN`. The client waits until the channel's send MTU is readable (about 0.8 s on the test hardware) before starting the handshake.
- **Linux scanning.** Scans list only devices actually heard during the scan, because BlueZ also returns cached entries with stale PSMs.
- **Linux listening.** `threnody run --ble` binds an LE L2CAP listener on a dynamic PSM and registers the advert with BlueZ's `LEAdvertisingManager1`. While an old LE link to a peer lingers (the peer's Bluetooth stack can hold it after the app has gone), some controllers stop sending the connectable advert. The next session works once that link is dropped.
- **Android.** `listenUsingInsecureL2capChannel()` (API 29+) and `BluetoothLeAdvertiser`, with the `BLUETOOTH_CONNECT` and `BLUETOOTH_ADVERTISE` runtime permissions. Each accepted socket is bridged into the node through `ByteLink` / `LinkHandle`.
- **Android dialing.** A low-latency `BluetoothLeScanner` scan collects adverts carrying the service-data UUID (`BLUETOOTH_SCAN` with `neverForLocation`), and `createInsecureL2capChannel(psm)` opens the channel. LE connection setup sometimes fails with HCI status 0x3e ("connection failed to be established"), so the app makes up to three attempts.
- **Android background.** A foreground service of type `remoteMessaging` keeps the process, and so the node and its Bluetooth sessions, alive while the app is in the background. Messages that arrive while no Activity is visible raise a notification.

## Tested

A Pixel 8a (Android API 37) and a Linux laptop (BlueZ 5.87), in both directions:

- The laptop found the phone's advert, opened the channel and completed the handshake, and messages went both ways.
- The phone found the laptop's advert (`run --ble`), dialed it, and messages went both ways.
- With the app in the background, and again after three minutes with the screen off, a message from the laptop arrived over the existing Bluetooth session and raised a notification.

## Privacy

The advert reveals that *a* Threnody device is nearby, plus its PSM. It reveals no identity, and the phone's Bluetooth address rotates. Hiding even the presence of a device would need the private beacon of Appendix E carried in an extended advertisement, which is future work.

## Not yet done

- Automatic reconnection, and dialing approved contacts as soon as they are heard.
- Private beacons in extended adverts (see Privacy).
- Android listening that survives the Activity being destroyed by the user (the listener is process-wide, but starting it needs the Activity).
