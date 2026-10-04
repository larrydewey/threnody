# Threnody for Android (sample)

A minimal app on [`threnody-ffi`](../../crates/threnody-ffi). It runs a node in the app's private storage and listens on port 7450. It shows the device fingerprint, account fingerprint and an invite link built from the phone's Wi-Fi address, and lets you connect, chat and approve the current peer. Its log shows every event.

```sh
export JAVA_HOME=/path/to/jdk-21        # JDK 17 or 21
apps/android/build.sh --install         # cross-compile, generate Kotlin bindings, build, adb install
```

You also need the Android SDK and NDK (`ANDROID_HOME`, default `~/Android/Sdk`) and the Rust target `aarch64-linux-android`. Gradle comes from the wrapper, and the build covers arm64-v8a only.

Tested on a Pixel 8a (Android API 37) over Wi-Fi with the desktop CLI: `threnody run -c threnody://…@<phone-ip>:7450`. Messages went both ways and mutual approval completed.

Tap **Start Bluetooth** to listen on an LE L2CAP channel and advertise it (Appendix K). A Linux machine can then `/ble scan` and `/ble connect` to chat over the radio. Tested with a Pixel 8a.

The node is a process-wide singleton, so rotating the screen or recreating the Activity doesn't drop sessions. There is no background service yet: Android may stop the app when it isn't in the foreground.
