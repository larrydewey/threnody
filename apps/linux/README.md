# Threnody for Linux

A desktop messenger on [`threnody-ffi`](../../crates/threnody-ffi), the API the [Android app](../android) uses, written with GTK 4 and libadwaita. It follows the Android app's design and wording, laid out for a desktop: conversations on the left and the open chat on the right. Narrow windows show one pane at a time.

```sh
apps/linux/install.sh               # build, then install for this user (~/.local); quits and restarts a running copy
cargo run -p threnody-desktop       # or run it from the source tree
```

It needs GTK 4.14 or newer and libadwaita 1.5 or newer, plus their development headers to build. On Debian or Ubuntu that's `libgtk-4-dev libadwaita-1-dev`, on Fedora `gtk4-devel libadwaita-devel`, and on Arch `gtk4 libadwaita`. The keyring is used through the Secret Service, so `libdbus-1-dev` is needed too. Video calls read the camera through GStreamer: `libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev` (Debian, Ubuntu), `gstreamer1-devel gstreamer1-plugins-base-devel` (Fedora) or `gstreamer gst-plugins-base` (Arch), with the `v4l2src` element from the good plugins (`gstreamer1.0-plugins-good`, `gstreamer1-plugins-good`, `gst-plugins-good`).

**One identity with the CLI.** The app uses the CLI's data directory (`$THRENODY_HOME`, else `~/.local/share/threnody`) and the same keyring entry. An identity created with `threnody init` opens without a prompt, and one created here works with the CLI. Don't run both on the same directory at once: two nodes with one identity fight over sessions. Only one copy of the app runs per data directory. If port 7450 is taken, the app picks another and keeps it for later runs, so invites stay valid.

**Key protection, on by default.** A new identity is sealed (Argon2id) with a random key kept in the system keyring (GNOME Keyring, KWallet, KeePassXC). An identity stored unprotected, from an older CLI, is sealed this way when the app first opens it. Without a keyring the app asks for a passphrase, and stores the key unprotected only if you choose that explicitly. Identities sealed with a passphrase of your own ask for it at start.

What it does:

- **Conversations.** Contacts and groups with their last message, unread counts and a dot for connected contacts. Message requests and group invitations come first. Right-click a conversation to clear it or delete the contact.
- **Chat.** Encrypted history with ticks: one when sent, two once a device of theirs acknowledges, and "✓ 1/2" in groups until every member has. Enter sends and Shift+Enter starts a new line. The smiley button beside the message box inserts any emoji. Text sent to an offline contact is sealed for mailboxes. Links ask before they open.
- **Photos and files.** Use the attach button or drag files into the chat. You can add a message and mark them *sensitive*, so the recipient sees a cover until they click it. Photos sent together form an album, and photo metadata is removed before sending unless you turn that off. Formats the node can't strip (HEIC, AVIF, TIFF, BMP) are sent as JPEG, and one that can't be read isn't sent. GIFs play in the chat. Received photos stay in the data directory (`media/`). Other files go to *Downloads/Threnody*. Photos whose messages are deleted or disappear are removed within five minutes. The ⋮ menu lists a chat's photos, files and links.
- **GIFs.** The GIF button searches GIPHY, once you agree to GIPHY seeing your searches and this computer's IP address (Preferences → *GIF search with GIPHY*). The GIF you pick is downloaded here and sent like a photo, so your contact's device never contacts GIPHY. Without that, it offers GIF files of your own. A build without `GIPHY_API_KEY` set asks for a key once.
- **Reactions, editing, deleting.** Right-click (or long-press on a touchscreen) a message to react, copy, edit your own, delete it for you, or delete it for everyone.
- **Trust.** The chat shows a banner when a contact is new, unapproved, unverified, or has added a device you haven't verified. *Compare* shows the safety number, and *They match* marks it verified. The ⋮ menu also renames, approves or revokes, sets the disappearing timer and shares your profile.
- **Invites.** The QR button shows your invite as a QR code and a link. Clicking a `threnody://` link (or `threnody-link://` for linking devices) opens the app with it filled in.
- **Groups.** Create a group (you own it), invite contacts, list and remove members, leave or delete it. Invitations from contacts who haven't approved you wait for *Join* or *Decline*.
- **Anonymous identities.** **+ → Anonymous invite** makes a separate identity with its own keys, conversations, files and port. It can burn itself after a day, a week or four weeks. Its chats are marked 🎭. *Reveal who you are…* in its chats proves to that contact that it is you. These are the CLI's personas, sealed the same way.
- **Volunteer relays and directories.** Preferences → *Relay directories* subscribes to a directory by its `threnody-dir://` link (every identity, anonymous ones included, subscribes through links of its own). When your contacts can't relay for you, circuits then go through two volunteer relays, each paid with an anonymous token ([Appendix P](../../docs/appendix-p-volunteer-relays.md)). *Route through volunteer relays* is on unless switched off.
- **Credentials.** A chat's menu → *Offer a credential…* vouches for attributes of that contact. *Ask for a credential…* asks them to prove some. Offers and requests ask you first. Nothing is shown unless you tick it, and proofs can't be linked to each other ([Appendix O](../../docs/appendix-o-credentials.md)). The main menu → *Credentials* lists those you hold.
- **Devices and profile.** Link a new device, join another device's account, rename devices, remove a lost or retired one, and edit your profile details. Nothing in your profile is shared until you choose, contact by contact.
- **Metadata protection, all on unless switched off** (Preferences): cover traffic (every 2 s, or every 10 s on a metered network), onion routing first, removing photo metadata, and reaching contacts over the internet. Notifications are private by default: they say "New message", without sender or text. Disappearing messages are off unless you choose a timer.
- **Background.** Closing the window keeps the node running, so messages still arrive and notify. Launching Threnody again brings the window back, and *Quit* (Ctrl+Q) stops it. Preferences can make closing quit instead.
- **Diagnostics.** Fingerprints, the data directory, how the key is protected, the port, internet reachability, and the node's event log. The log never records message text.

Not yet here: volunteering as a relay or running a directory (the CLI does both: `run --volunteer-relay`, `run --serve-directory`), Bluetooth LE and Wi-Fi Direct (the CLI has both on Linux, with `run --ble` and `--wifi-direct`), scanning QR codes with a camera, and WireGuard tunnels.

## Testing without a display

GTK's Broadway backend renders the app in a browser, which allows headless tests. The flows above were tested this way against the CLI, with throwaway homes: message requests, chat both ways, files and photos, reactions, safety numbers matching the CLI's, mutual approval, groups, and anonymous identities.

```sh
gtk4-broadwayd :5 &
GDK_BACKEND=broadway BROADWAY_DISPLAY=:5 THRENODY_HOME=/tmp/thr-a \
    cargo run -p threnody-desktop -- --gapplication-app-id=org.threnody.Test   # open http://127.0.0.1:8085
```

`--gapplication-app-id` lets a second copy run beside the one you use.
