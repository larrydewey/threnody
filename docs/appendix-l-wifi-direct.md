# Appendix L: Wi-Fi Direct Link Upgrade (v1)

Status: implemented in `threnody-net::direct` (messages), `apps/android` (hosting and joining groups) and `threnody-radio::wifidirect` (joining through NetworkManager, for the CLI and the desktop app). Fulfils spec §7.2's Wi-Fi Direct transport.

## Model

Wi-Fi Direct is used as an **upgrade** of a session that already exists, usually over Bluetooth LE (Appendix K):

- Bluetooth finds the peer and carries the first session.
- Wi-Fi Direct then gives the bandwidth for files and calls.

The steps are:

1. **One side hosts.** It creates a Wi-Fi Direct group with an explicit network name (`DIRECT-th-xxxx`) and a random 20-character passphrase.
2. **It sends an offer.** The offer travels over the existing session and carries the group credentials and the address it listens on inside the group.
3. **The other side joins.** It joins the group with those credentials, then dials the address. The dial pins the peer's fingerprint.
4. **The new session replaces the old one.** Sessions are keyed by peer, so the TCP session over the group takes over. Replacing a session isn't reported as a disconnection.

The credentials travel inside the end-to-end ratchet, so neither device shows a Wi-Fi Protected Setup (WPS) prompt. The group's WPA2 layer adds nothing to security beyond keeping strangers off the radio link, because the Threnody session over it has its own encryption.

## Messages

These are carried as `AppMessage::Direct` (Appendix C, kind 12):

```
DirectMsg = { 0: op, ? 1: ssid tstr, ? 2: passphrase tstr, ? 3: addr tstr }
op: 1 offer (ssid 1–32 bytes without control characters, passphrase 8–63
             printable ASCII characters, addr = ip:port literal)
    2 request (asks the peer to host and offer)
```

Both messages are only sent to, and only honoured from, **mutually approved peers on a session over a nearby link**: Bluetooth (any stream transport), or TCP or QUIC whose peer address is loopback, private (RFC 1918, IPv6 unique local) or link-local. A peer reached through relays isn't nearby, and nor is one dialed or hole-punched across the internet; neither is trusted with link credentials, or with moving our radio, just because it is reachable. Each peer gets at most one request and one offer honoured every 30 seconds; the rest are dropped, since each makes a phone host a new group or a laptop leave its network. Malformed offers are refused on both ends, including an SSID with control characters or a passphrase that isn't printable ASCII (WPA2 requires printable ASCII anyway, so no sender is affected; it also keeps a passphrase from breaking the line-based files a joiner hands to NetworkManager). An anonymous identity's node (Appendix M) neither sends nor honours them: a group would show a nearby contact the phone, and the main identity listening on it. Older peers send the same messages and see no change unless they are far away or repeat themselves.

## Platforms

- **Android hosts or joins.**
  - Hosting uses `WifiP2pManager.createGroup` with a `WifiP2pConfig` that names the network and passphrase.
  - Joining uses `WifiP2pManager.connect` with the same kind of config.
  - Both are API 29+ and need `NEARBY_WIFI_DEVICES` (`neverForLocation`).
  - An incoming request is answered by hosting a group and offering it.
- **Linux joins.**
  - `threnody run --wifi-direct` joins offered groups as an ordinary Wi-Fi client through NetworkManager. It joins at once only a group it asked that peer for in the last two minutes (`/wifi-direct request`); an offer nobody asked for waits for `/wifi-direct join`.
  - The desktop app does the same once Preferences → Nearby → *Join Wi-Fi Direct links* is on (off by default): *Faster link (Wi-Fi Direct)* in a contact's chat menu asks the contact to host, and the group offered back within two minutes is joined; an unasked offer is kept, and *Faster link* takes it. Turning the switch off, *Leave Wi-Fi Direct* in Diagnostics, or quitting (bounded to 10 s) leaves the group, or the one being joined. Quitting with nothing joined or joining doesn't run `nmcli` (the CLI as well).
  - Joiners only join networks whose SSID starts with `DIRECT-` (Wi-Fi P2P groups always do), so a contact can't steer the computer onto an ordinary network. This is local policy, not a message rule.
  - One group at a time: joining a new group leaves any other first, and a repeated offer for the group already joined is ignored.
  - A group is left 60 s after the join once no session to that peer runs over the group owner's IP address, and at once if the pinned dial fails.
  - Profiles are named `threnody-<tag>-<ssid>`, where the tag is 8 hex digits derived from the joining identity (not its fingerprint: every local user can list profile names). A process only deletes its own identity's profiles, so the app and a CLI with another home never cut each other's links. Its profiles a crash left behind are deleted on leave and before the first join after a start; so are inactive ones from before the tag (`threnody-DIRECT-*`).
  - Leaving while a join is still running deletes its profile, which cuts the activation short; a join that finishes anyway is left at once.
  - The profile is temporary: no autoconnect, no default route, no DNS.
  - The passphrase goes through a 0600 `passwd-file` rather than the command line.
  - The Wi-Fi interface leaves its usual network while joined. `/wifi-direct leave` (or the app's *Leave Wi-Fi Direct*), or exiting, deletes the profile, and NetworkManager then reconnects the usual network.
  - Hosting a group from Linux needs root access to wpa_supplicant, so Linux answers a request by saying it can't host.
- **CLI.** `/wifi-direct request` asks the current peer to host.
- **Desktop.** *Faster link (Wi-Fi Direct)* in a contact's chat menu asks that contact to host; never in an anonymous identity's chats.

## Tested

A Pixel 8a and a Linux laptop (NetworkManager 1.58), already in a session over Bluetooth:

1. The laptop sent `/wifi-direct request`.
2. The phone created `DIRECT-th-ebjp` at 192.168.49.1 and sent the offer.
3. The laptop joined and dialed it. About 15 s after the request, the session was running over TCP inside the group.
4. A message and a 4 MB file then went from laptop to phone. The file arrived in under 4 s, measured by polling the phone's screen.
5. After `/wifi-direct leave`, the laptop returned to its usual network and the session fell back to Bluetooth within 10 s.

For comparison, 400 kB over Bluetooth LE took under 50 s.

## Not yet done

- **Automatic upgrade.** For example, request Wi-Fi Direct when a large file is queued for a Bluetooth peer.
- **Releasing the group** when the upgraded session ends.
- **Phone-to-phone test.** Both phones would use the same Android code, but only one phone was available.
