# Threnody for iOS

A SwiftUI app (iOS 17+) around the same Rust node as the Android app, through the UniFFI bindings in [`crates/threnody-ffi`](../../crates/threnody-ffi). It is a beta that does what the Android app does except calls, tunnels, Bluetooth and Wi-Fi Direct links, and push wake-ups (see *Missing compared with Android*).

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

CI (`.github/workflows/ci.yml`, job `ios`) runs the same steps, builds for the simulator without signing, and runs the unit tests (`ThrenodyTests`, XCTest) in a simulator. Run them locally with *Product → Test* or `xcodebuild test -scheme Threnody -destination 'platform=iOS Simulator,name=iPhone 16'`.

## What works

- **Identity.** Created on first launch and sealed with a random passphrase (Argon2id, as on Android). The passphrase is encrypted to a P-256 key in the Secure Enclave, or kept as a Keychain item where there is no Secure Enclave (the simulator). Both are `AfterFirstUnlockThisDeviceOnly`: bound to this device and out of backups, and the node's directory is excluded from backups too. A restored backup on another phone can't open the identity, by design. Received and sent files are kept privately, out of backups, readable only while the phone is unlocked, and deleted when their messages are deleted or disappear.
- **Chats.** One list of contacts, groups, *Note to self*, group invitations and message requests, with previews, unread counts (an "@" when one mentions you), who is connected and who is typing. Folders above the list (rules, picked chats, notification overrides, quiet hours with a let-through); swipe to pin (at most ten) or archive; *Archived* at the bottom. Drafts are kept per chat.
- **Messages.** Touch and hold a message to react (six quick reactions, or any emoji), reply, copy (never to other Apple devices), edit your own, or delete it for you or, for your own messages to a contact, for everyone. Replies show what they quote; tap it to jump there. Reactions show under the message; tap one to add or take back yours. Mentions show by name; polls show with their counts (vote from another device).
- **Photos and files.** The paperclip sends photos (up to ten at once, as an album) or any file, with a caption and a *Sensitive* switch. HEIC, AVIF, TIFF and BMP photos are converted to JPEG first, so no location, camera or time details leave the phone while *Remove photo metadata* is on (the node strips JPEG, PNG and WebP itself). Received photos are decoded small and refused over 100 megapixels; sensitive ones stay covered until tapped. Tap a file to open it in QuickLook, which plays voice and video messages and offers Share and Save. Files are sent only while a session is up, as on Android.
- **Groups.** Create one and invite contacts, accept or decline invitations, see members and their roles, and (depending on your role) invite, remove, make admin, hand over ownership or choose a successor. Per-group notifications (all, mentions, off), disappearing timer, clear, leave, or delete for everyone.
- **Search.** The search field above the chat list searches every chat (conversation names need every word); the magnifier in a chat searches it, marks the matches and steps through them.
- **Devices.** *Settings* → *Devices* lists this account's devices; rename or remove them. *Link a new device* shows a one-time code; *Join another device's account* (also on the first screen: *I already use Threnody: link this iPhone*) takes one by QR code or paste, so an iPhone can join an Android or Linux account. A safety alert says when a verified contact adds a device you haven't verified, and this device says when it was removed.
- **Handles.** *Settings* → *Your handle* makes a `name-XXXX-XXXX` handle people can type into *Add contact* to find you (as a message request); change its code, rename or remove it. *Add contact* takes handles too, with *Reach contacts over the internet* on.
- **Lists.** Shared checklists (*+* → *Lists*): tick, edit, reorder, indent, headings, paste and copy markdown, share with a contact or group. Links in items are shown as plain text.
- **Anonymous identities.** *Settings* → *Anonymous identities*: each has its own keys, contacts and chats (tagged 🎭 in the list), and burns itself when you choose. On iPhone one is reachable only while Threnody is open and on the same network as whoever has its invite: iOS suspends the app, and anonymous identities never use the DHT.
- **Receipts and typing.** ✓ sent, ✓✓ delivered, 👁️ read. Read receipts and typing indicators go out unless turned off in *Settings*.
- **Contacts.** Add by pasting an invite link or handle, scanning its QR code (AVFoundation), or tapping an Android phone showing its invite (Core NFC; iPhones can't be tapped, as iOS doesn't let apps emulate a card); show your own invite as a QR code (Core Image) or share the link, and reset invite links so ones shared so far stop finding this device across the internet. Accept or block message requests.
- **Trust.** Approve and revoke, compare safety numbers device by device and mark them verified, with the same banners as Android (approval, unverified, new unverified device). Per-chat disappearing timers (30 seconds to 4 weeks) and notifications, pin, archive and clear.
- **Privacy settings.** As on Android, every protection is on unless you turn it off: cover traffic (every 2 s, every 10 s on expensive or Low Data networks), onion routing first, removing photo metadata, read receipts, typing indicators, reaching contacts over the internet and routing through volunteer relays; and a default disappearing timer (off unless chosen). They are applied before the node listens, so with *Reach contacts over the internet* off it never joins the DHT. These settings are kept with the identity, out of backups, and erased with it. Screen security and private notifications are kept with the app instead (in its preferences, which iOS backs up; they say only whether you turned a protection off), so they apply before the identity opens; an erase removes them too.
- **Scheduled messages.** Long-press the send button for *Send Later…* (contacts and groups). They are sent while the app is open, or during a background refresh; a local notification, without sender or text, reminds you to open the app when one is due.
- **Network changes.** `NWPathMonitor` tells the node when the path moves between Wi-Fi and cellular, and it redials approved contacts.
- **App lock.** Onboarding offers *Lock Threnody with Face ID / passcode*, on by default where the phone has a passcode; existing installs get it once after updating, with a notice and *Turn off*. *Settings* → *App lock* switches to an app PIN or passphrase (optionally also Face ID, never the passcode, which could get around it), sets when it locks after leaving and when idle, *Lock now*, a *duress PIN* that erases everything and shows onboarding again, and *Erase after 10 wrong PINs* (off unless chosen). Wrong entries wait 30 s after five, doubling to at most an hour. While it is on, notifications say only "New message" and name no chat, and the lock screen sits in a window of its own above any open sheet or alert, so nothing under it can be used or read out by VoiceOver.
- **Screen security.** A cover hides the app in the app switcher and while the screen is recorded, mirrored or AirPlayed (*Settings* → *Screen security*, on by default; always while app lock is on). iOS doesn't let apps block screenshots.
- **Erasing.** *Settings* → *Erase all Threnody data…*, after you type `erase`, erases the identity and every anonymous identity, the Secure Enclave key and Keychain item, history, files, settings and notifications, and shows onboarding (Appendix C, *Erasing*). Nothing is sent. iOS keeps the notification permission.
- **Notifications.** Local notifications for messages, files, group messages and invitations that arrive while the app runs, except for the chat on screen and chats you muted. They say only "New message", without sender or text, unless *Settings* → *Private notifications* is turned off; even then a disappearing message's text is left out, as it would outlive the message. With previews hidden in iOS's settings the lock screen shows only "New message" too.

## Receiving in the background

iOS suspends an app a few seconds after it leaves the screen, and Threnody has no push server to wake it. So:

- While the app is in the foreground, messages arrive live.
- When it goes to the background it asks iOS for a little time to finish sending, then sleeps. Messages sent to you meanwhile are held by your mutually approved contacts (mailboxes) or queued by the sender, and arrive the next time you open the app.
- A background refresh task (`org.threnody.app.refresh`) reconnects for about 20 seconds when iOS chooses to run it, which may be rarely or never. Don't rely on it.

To be reachable while the phone sleeps, use an always-on relay (a mutually approved relay node on a server, [`docs/relay-node.md`](../../docs/relay-node.md)) once the app can choose one, and expect notifications only when the app is opened until push-style wake-ups exist.

## Why no push on iOS

Android can wake Threnody through UnifiedPush, a distributor the user chooses ([Appendix R](../../docs/appendix-r-push.md)). iOS has nothing like it: only Apple's push service can wake an app, and only for whoever holds the app's push key, which for the published app is its publisher. A wake-up gateway would be a server run by the publisher that sees every wake-up and every waking contact's IP address, the same Google-held record that Appendix R rejects FCM for, with Apple instead. It wouldn't even deliver messages: silent pushes are throttled and dropped after a force-quit, and a notification extension has too little memory to unlock the identity (Argon2id needs 64 MiB). So the iOS app doesn't use push. Instead, mutually approved contacts hold messages (mailboxes) and senders queue them until the app is opened, and an always-on relay contact makes this device reachable whenever the app runs.

## Missing compared with Android

- Voice and video calls: the `calls` feature (WebRTC) isn't built for iOS. The bindings still contain the call API; nothing calls it.
- Tunnels (`boringtun` feature): would need a Network Extension.
- Push wake-ups (see above).
- Recording voice and video messages (received ones play in QuickLook), composing mentions, voting in polls, forwarding, pinned messages, link previews and GIFs.
- Getting files from your other devices: a photo synced from another device shows as "on another device".
- Profiles and credentials, revealing an anonymous identity, directories.
- Backups and restores.
- Bluetooth and Wi-Fi Direct links; local discovery by multicast needs Apple's `com.apple.developer.networking.multicast` entitlement, which must be requested.
- Always-on relay choice, standby path, the diagnostics log.

## How to test (two devices)

With an iPhone and an Android phone or Linux desktop:

- **Privacy:** toggle each setting and check the footer; with Low Data Mode on cellular, cover traffic slows to every 10 s. Screen recording and the app switcher show the cover.
- **Photos and files:** send a HEIC with a location from Photos (on Android, `exiftool` shows no GPS); three photos as an album with a caption; a sensitive photo (it arrives covered); a 20000×20000 PNG from Linux (a label, not a crash).
- **Message actions:** reply, react both ways, edit (Android shows "edited"), delete for everyone (Android deletes it).
- **Groups:** create one on the iPhone and invite Android; messages, files and roles both ways; leave; accept an invitation from someone who isn't a mutual contact.
- **Devices:** link the iPhone into an Android account from the first screen; history syncs; remove it from Android, and the iPhone says it was removed.
- **Handles:** make one on the iPhone and add it from Android (a message request); change the code, and the old one stops within the hour.
- **Folders, lists, search:** folder rules and quiet hours keep notifications quiet; a list shared from Linux is edited on the iPhone and back; search jumps to an old message.
- **Disappearing messages:** a photo with a 30 s timer is gone from `media/` within about ten minutes after it disappears (download the container from Xcode).
- **Anonymous identities:** create one, scan its invite from Android on the same Wi-Fi, chat, burn it, and check that `media-p-<id>` is gone.

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
