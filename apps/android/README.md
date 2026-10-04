# Threnody for Android (sample)

A minimal app on [`threnody-ffi`](../../crates/threnody-ffi). It runs a node in the app's private storage and listens on port 7450. It shows the device fingerprint, account fingerprint and an invite link built from the phone's Wi-Fi address, and lets you connect, chat and approve the current peer. Its log shows every event.

```sh
export JAVA_HOME=/path/to/jdk-21        # JDK 17 or 21
apps/android/build.sh --install         # cross-compile, generate Kotlin bindings, build, adb install
```

You also need the Android SDK and NDK (`ANDROID_HOME`, default `~/Android/Sdk`) and the Rust target `aarch64-linux-android`. Gradle comes from the wrapper, and the build covers arm64-v8a only.

Tested on a Pixel 8a (Android API 37) over Wi-Fi with the desktop CLI: `threnody run -c threnody://…@<phone-ip>:7450`. Messages went both ways and mutual approval completed.

If Bluetooth permissions are granted, the app listens, advertises a private beacon, and connects to approved contacts it hears, all on its own. Otherwise tap **Start Bluetooth** to grant them and start. It listens on an LE L2CAP channel and advertises it (Appendix K). To connect by hand to a device that isn't a contact yet, a Linux machine can `/ble scan` and `/ble connect`. The other way round, run `threnody run --ble` on Linux, tap **Scan Bluetooth**, type `ble 1` (the number the scan printed) and tap **Connect**. Both directions were tested with a Pixel 8a.

The node is a process-wide singleton, so rotating the screen or recreating the Activity doesn't drop sessions. A foreground service (type `remoteMessaging`, with an ongoing notification) keeps the process alive in the background. Messages that arrive while the app isn't visible show up as notifications.
