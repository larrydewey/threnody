# Threnody for Android (sample)

An app on [`threnody-ffi`](../../crates/threnody-ffi), built on the platform alone (no AndroidX). It runs a node in the app's private storage and listens on port 7450.

- **Conversations.** A list of contacts and groups with their last message, and whether each contact is connected. A contact's devices share one conversation. Pending group invitations come first.
- **Chat.** Message bubbles from the encrypted history, sending (live, through relays, or sealed for mailboxes when the contact is offline), and files both ways. Received files go to *Downloads/Threnody*. Files are kept in history, so they stay in the chat and open from it.
- **Groups.** *New group* under **+** creates an MLS group that you own. You can invite contacts, see members, and remove them as the owner. Members' messages show who sent them. Invitations from mutually approved contacts are accepted automatically. Others appear in the list with **Join** and **Decline**.
- **Trust.** A banner asks you to approve new contacts, then to compare safety numbers. The contact menu has rename, approve or revoke, the safety number, disappearing messages and Wi-Fi Direct.
- **Invites.** *My invite* shows a QR code and link to share or copy. The app opens `threnody://` invites and `threnody-link://` device codes, so scanning a QR code with the camera opens the app. The overflow menu links a new device or joins another device's account.
- **Diagnostics.** The node's event log, plus manual Bluetooth scan and dial for testing transports.

The layout is edge to edge and pads for the system bars and keyboard, whether the phone uses gesture navigation or three-button navigation (which sits on the side in landscape). It follows the system light or dark theme.

```sh
export JAVA_HOME=/path/to/jdk               # JDK 17 or newer
apps/android/build.sh --install             # cross-compile, generate Kotlin bindings, build, adb install
apps/android/build.sh --emulator --install  # also build x86_64 for the emulator
```

You also need the Android SDK and NDK (`ANDROID_HOME`, default `~/Android/Sdk`) and the Rust targets `aarch64-linux-android` (and `x86_64-linux-android` for `--emulator`). Gradle comes from the wrapper.

To try it on an emulator against the desktop CLI, run `threnody run --listen 127.0.0.1:7460` on the host and give the app the invite for `10.0.2.2:7460` (the host as the emulator sees it). For example, `adb shell am start -a android.intent.action.VIEW -d 'threnody://…@10.0.2.2:7460'`.

Tested on a Pixel 8a (Android API 37) over Wi-Fi with the desktop CLI: `threnody run -c threnody://…@<phone-ip>:7450`. Messages went both ways and mutual approval completed.

If Bluetooth permissions are granted, the app listens, advertises a private beacon, and connects to approved contacts it hears, all on its own. Otherwise tap **Start Bluetooth** under *Diagnostics* to grant them and start. It listens on an LE L2CAP channel and advertises it (Appendix K). To connect by hand to a device that isn't a contact yet, a Linux machine can `/ble scan` and `/ble connect`. The other way round, run `threnody run --ble` on Linux, tap **Scan Bluetooth** under *Diagnostics*, type `ble 1` (the number the scan printed) and tap **Dial**. Both directions were tested with a Pixel 8a.

**Faster link (Wi-Fi Direct)** in a contact chat's menu creates a Wi-Fi Direct group and offers it to that contact over the existing session (Appendix L). The contact joins, and the session moves to the faster link. A request from a peer does the same automatically. **Leave Wi-Fi Direct** under *Diagnostics* closes the group. These need the nearby-devices permission.

The node is a process-wide singleton, so rotating the screen or recreating the Activity doesn't drop sessions. On start, and whenever a network becomes available, it redials mutually approved contacts. A foreground service (type `remoteMessaging`, with an ongoing notification) keeps the process alive in the background. Messages that arrive while their chat isn't on screen show up as notifications, one per conversation. Tapping one opens the chat, and Back returns to the list.

The UI flows were tested on an API 35 emulator against the desktop CLI: invites by deep link, chat both ways, approval, safety numbers, files both ways (and still listed after a restart), notifications, and redialing after a restart. For groups, the tests covered creating one, inviting an approved contact, joining an unapproved contact's group or declining it, chatting both ways and removing a member. They also covered light and dark themes, and portrait and landscape with three-button navigation.

A group member who has no session with another member reaches them through an approved relay (Appendix G). Without one, the message isn't delivered. Group invitations waiting for consent are kept in memory only, so after a restart the owner invites again.
