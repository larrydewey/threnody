# Threnody for iOS

A SwiftUI app (iOS 17+) around the same Rust node as the Android app, through the UniFFI bindings in [`crates/threnody-ffi`](../../crates/threnody-ffi). It is a first beta: the core chat flows, not yet everything the Android app does.

## Build

Needs macOS with Xcode 16 or later, Rust (rustup), and [XcodeGen](https://github.com/yonaskolb/XcodeGen).

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
brew install xcodegen
apps/ios/build-xcframework.sh     # Rust for devices and simulators, Swift bindings, XCFramework
cd apps/ios && xcodegen generate  # writes Threnody.xcodeproj from project.yml
open Threnody.xcodeproj
```

`build-xcframework.sh` builds `libthrenody_ffi.a` for `aarch64-apple-ios` (devices) and for `aarch64-apple-ios-sim` and `x86_64-apple-ios` (simulators, combined with `lipo`), generates the Swift bindings from a host build with the same features, and packages `Generated/ThrenodyFFI.xcframework`. The bindings land in `Threnody/Generated/threnody_ffi.swift`. Everything it makes is ignored by git, as is the Xcode project: edit `project.yml`, not the project. Rerun the script after changing the Rust API (`--debug` builds faster).

CI (`.github/workflows/ci.yml`, job `ios`) runs the same steps and builds for the simulator without signing.

## What works

- **Identity.** Created on first launch and sealed with a random passphrase (Argon2id, as on Android). The passphrase is encrypted to a P-256 key in the Secure Enclave, or kept as a Keychain item where there is no Secure Enclave (the simulator). Both are `AfterFirstUnlockThisDeviceOnly`: bound to this device and out of backups, and the node's directory is excluded from backups too. A restored backup on another phone can't open the identity, by design.
- **Chats.** The chat list (with previews, unread counts and who is connected), 1:1 text chat, drafts, and received files and photos (saved privately; tap to share).
- **Receipts and typing.** ✓ sent, ✓✓ delivered, 👁️ read. Read receipts go out for messages shown on screen; typing indicators both ways.
- **Contacts.** Add by pasting an invite link or scanning its QR code (AVFoundation); show your own invite as a QR code (Core Image) or share the link. Accept or block message requests.
- **Trust.** Approve and revoke, compare safety numbers device by device and mark them verified, with the same banners as Android (approval, unverified, new unverified device). Per-chat disappearing timers.
- **Network changes.** `NWPathMonitor` tells the node when the path moves between Wi-Fi and cellular, and it redials approved contacts.
- **Notifications.** Local notifications for messages that arrive while the app runs, except for the chat on screen.

## Receiving in the background

iOS suspends an app a few seconds after it leaves the screen, and Threnody has no push server to wake it. So:

- While the app is in the foreground, messages arrive live.
- When it goes to the background it asks iOS for a little time to finish sending, then sleeps. Messages sent to you meanwhile are held by your mutually approved contacts (mailboxes) or queued by the sender, and arrive the next time you open the app.
- A background refresh task (`org.threnody.app.refresh`) reconnects for about 20 seconds when iOS chooses to run it, which may be rarely or never. Don't rely on it.

To be reachable while the phone sleeps, use an always-on relay (a mutually approved relay node on a server, [`docs/relay-node.md`](../../docs/relay-node.md)) once the app can choose one, and expect notifications only when the app is opened until push-style wake-ups exist.

## Missing compared with Android

- Voice and video calls: the `calls` feature (WebRTC) isn't built for iOS. The bindings still contain the call API; nothing calls it.
- Tunnels (`boringtun` feature): would need a Network Extension.
- Groups, notes to self, lists, polls, reactions, replies, edits and deletes, mentions, search, folders, pinned and archived chats.
- Sending files and photos, voice and video messages, link previews.
- Anonymous identities (personas), profiles and credentials, directories.
- Linking more devices to the account, backups and restores.
- Bluetooth and Wi-Fi Direct links; local discovery by multicast needs Apple's `com.apple.developer.networking.multicast` entitlement, which must be requested.
- Privacy settings: everything stays at the node's defaults (cover traffic, onion-first, read receipts and typing indicators on). No settings to turn them off yet, nor screen-capture protection.
- Always-on relay choice, standby path, the diagnostics log.

## TestFlight

What the maintainer has to do once:

1. **Apple Developer Program** membership (paid) for the account that will publish.
2. **Bundle ID.** In Certificates, Identifiers & Profiles, register an App ID. The project uses `org.threnody.app`; if that is taken, change `PRODUCT_BUNDLE_IDENTIFIER` in `project.yml`. No extra capabilities are needed (background fetch is an Info.plist mode, not a capability).
3. **App Store Connect.** Create the app record with that bundle ID.
4. **Signing.** Set `DEVELOPMENT_TEAM` in `project.yml` to your team ID (or choose the team under Signing & Capabilities in Xcode); automatic signing makes the certificates and profiles.
5. **Version.** Bump `CURRENT_PROJECT_VERSION` in `project.yml` for every upload; `MARKETING_VERSION` follows the release.
6. **Archive and upload.** Run `build-xcframework.sh` (release) and `xcodegen generate`, then in Xcode choose *Any iOS Device*, *Product → Archive*, and *Distribute App → TestFlight & App Store*. From the command line: `xcodebuild archive -scheme Threnody -destination 'generic/platform=iOS' -archivePath build/Threnody.xcarchive DEVELOPMENT_TEAM=…` then `xcodebuild -exportArchive` with an export options plist (`method` = `app-store-connect`).
7. **Export compliance.** Threnody uses encryption beyond Apple's exemptions for HTTPS. App Store Connect asks about it on each build until `ITSAppUsesNonExemptEncryption` is set in `project.yml`; answer the questionnaire (and file the yearly self-classification report with the US BIS if it applies), then set the key accordingly.
8. **Testers.** Internal testers can install right away. External testers need a short Beta App Review; describe how to test (two devices, exchange invites by QR code), and note that local network access is used to reach contacts directly.

CI builds for the simulator only. Uploading from CI would need the signing certificate and an App Store Connect API key as secrets; that isn't set up.
