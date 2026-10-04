# threnody-ffi

[UniFFI](https://mozilla.github.io/uniffi-rs/) bindings for embedding a Threnody node in apps: Kotlin (Android), Swift (iOS) and Python.

```sh
crates/threnody-ffi/bindings.sh            # build, generate bindings into target/bindings, run the Python test
```

The API is small and **blocking**. Each node owns a Tokio runtime, so call it from a background thread or coroutine.

| Call | Purpose |
|---|---|
| `ThrenodyNode.open(home, passphrase?)` | Opens a node, creating it on first use. The passphrase seals a new identity. |
| `listen(addr)`, `invite_link(addr)`, `connect(link)` | Reach peers. |
| `send_text(peer, text)` | Sends to every device of the peer's account. Uses live sessions where possible, otherwise sealed for mailboxes. |
| `set_approval`, `set_name`, `contacts()`, `safety_number(peer)` | Manage trust. |
| `create_link_code(addr)`, `link_with(code)` | Add a device to the account. |
| `next_event(timeout_ms)` | Drains `NodeEvent`s: `Connected`, `Message`, `File`, `ApprovalChanged`, `AccountChanged`, … |
| `shutdown()` | Stops the node. |

## Android

Build the library for each ABI with [cargo-ndk](https://github.com/bbqsrc/cargo-ndk), for example `cargo ndk -t arm64-v8a -o app/src/main/jniLibs build -p threnody-ffi --release`. Then add `target/bindings/kotlin/uniffi/threnody_ffi/threnody_ffi.kt` to the app, along with the [JNA](https://github.com/java-native-access/jna) dependency that UniFFI's Kotlin bindings use.

## iOS

Build `libthrenody_ffi.a` for `aarch64-apple-ios` and `aarch64-apple-ios-sim`, then combine them into an XCFramework with `xcodebuild -create-xcframework`. Add `threnody_ffi.swift`, `threnody_ffiFFI.h` and `threnody_ffiFFI.modulemap` from `target/bindings/swift`.

The Kotlin and Swift bindings are generated from the same library as the Python ones, which are exercised by `tests/python/test_chat.py`. They have not been compiled into an app here.
