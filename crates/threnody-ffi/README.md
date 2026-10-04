# threnody-ffi

[UniFFI](https://mozilla.github.io/uniffi-rs/) bindings for embedding a Threnody node in apps: Kotlin (Android), Swift (iOS) and Python.

```sh
crates/threnody-ffi/bindings.sh            # build, generate bindings into target/bindings, run the Python test
```

The API is small and **blocking**. Each node owns a Tokio runtime, so call it from a background thread or coroutine.

| Call | Purpose |
|---|---|
| `ThrenodyNode.open(home, passphrase?)` | Opens a node, creating it on first use. The passphrase seals a new identity. |
| `identity_is_sealed(home)`, `change_passphrase(home, current?, new?)` | Before opening: check whether an identity is sealed, and seal, reseal or unseal it. For example, the Android app keeps a random passphrase under the Keystore. |
| `listen(addr)`, `invite_link(addr)`, `connect(link)` | Reach peers. |
| `send_text(peer, text)` | Sends to every device of the peer's account. Uses live sessions where possible, otherwise sealed for mailboxes. |
| `send_file(peer, name, data, location?)`, `record_received_file(…)` | Sends a file, reaching the peer first, and records it in history with where the app keeps it. Record received files once they are saved. |
| `history(peer, n)`, `set_disappearing(peer, secs?)` | Stored messages (text or file entries) and the disappearing-message timer. |
| `set_approval`, `set_name`, `contacts()`, `safety_number(peer)`, `mark_verified(peer)` | Manage trust. |
| `reconnect()` | Dials mutually approved contacts that aren't connected. Call it on start and when the network returns. |
| `create_group`, `groups()`, `invite_to_group`, `remove_from_group`, `send_group_text`, `group_history` | MLS groups (Appendix F). Invitations from mutually approved contacts are accepted automatically. Others arrive as `GroupInvited` and wait for `accept_group_invite` or `decline_group_invite`. |
| `qr_matrix(text)` | A QR code for an invite or link code, for the app to draw. |
| `create_link_code(addr)`, `link_with(code)` | Add a device to the account. |
| `next_event(timeout_ms)` | Drains `NodeEvent`s: `Connected`, `Message`, `File`, `ApprovalChanged`, `AccountChanged`, `GroupMessage`, `GroupInvited`, … Group traffic is processed inside this call, so keep calling it. |
| `shutdown()` | Stops the node. |

## Android

Build the library for each ABI with [cargo-ndk](https://github.com/bbqsrc/cargo-ndk), for example `cargo ndk -t arm64-v8a -o app/src/main/jniLibs build -p threnody-ffi --release`. Then add `target/bindings/kotlin/uniffi/threnody_ffi/threnody_ffi.kt` to the app, along with the [JNA](https://github.com/java-native-access/jna) dependency that UniFFI's Kotlin bindings use.

## iOS

Build `libthrenody_ffi.a` for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then combine them into an XCFramework with `xcodebuild -create-xcframework`. Add `threnody_ffi.swift`, `threnody_ffiFFI.h` and `threnody_ffiFFI.modulemap` from `target/bindings/swift`.

The Kotlin and Swift bindings are generated from the same library as the Python ones, which are exercised by `tests/python/test_chat.py`. The Kotlin bindings are used by the Android app in [`apps/android`](../../apps/android). The Swift ones have not been compiled into an app yet.
