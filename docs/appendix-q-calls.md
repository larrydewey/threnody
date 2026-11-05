# Appendix Q: Voice and Video Calls (v1)

Status: signalling, media keys and transport implemented in `threnody-core::call` and `threnody-net::call`. Media (WebRTC) is in `threnody-media`; the FFI, the Linux app and the Android app use it. Voice and video between two people, and group calls of up to eight devices (`threnody-net::groupcall`, signalled through `threnody-groups`).

## Overview

A call runs between two devices that have a live session. Signalling goes inside the ratchet. The media is WebRTC's (codecs, echo cancellation, jitter buffers, congestion control). WebRTC never touches the network itself: its only ICE candidates are relay candidates from a TURN server inside our own process. That server hands every packet to the call, which seals it with the call's own keys and carries it over the session.

```
WebRTC ──TURN (loopback)──▶ threnody-media::turn ──▶ Node::send_media ──▶ QUIC datagram / session stream
                                                                                  │
WebRTC ◀──TURN (loopback)── threnody-media::turn ◀── Node::call_media ◀───────────┘ (peer)
```

## Signalling

`AppMessage::Call` (kind 25), sent only to peers whose `Hello` has `FEATURE_CALLS` (1024):

```
CallMsg = { 0: op, 1: call uint, ? 2: secret bstr(32), ? 3: flags uint,
            ? 4: reason uint, ? 5: data bstr, ? 6: emoji tstr }
op: 1 offer (secret, flags), 2 ringing, 3 answer (secret, flags),
    4 hangup (reason), 5 update (flags), 6 signal (data ≤ 64 KiB),
    7 reaction (emoji)
flags: 1 = video
reason: 0 ended, 1 declined, 2 busy, 3 unanswered, 4 failed,
        5 answered elsewhere, 6 declined elsewhere (unknown = ended)
```

- **Offer.** The caller picks a random non-zero 64-bit call id and a fresh 32-byte secret.
- **Every device rings.** The caller sends the same offer to every device of the callee's account that it has a session with and whose `Hello` has `FEATURE_CALLS`, and to any such device whose session comes up while the call still rings. The first device to answer takes the call: the caller sends the others `hangup` with reason 5 (`answered elsewhere`), and from then on only the answering device counts. A decline from any device ends the call, and the caller sends the others reason 6 (`declined elsewhere`). Any other `hangup` from one ringing device (busy, say) only drops that device, while the rest ring on. A device of our own account is called alone. Apps don't log a call answered or declined on another device as missed.
- **Ringing.** The callee says it is ringing. Offers from someone not accepted (a message request) never ring. They get no answer at all, so a stranger can't tell whether we're there.
- **Busy.** An offer while a call is in progress gets `busy`.
- **Both calling at once.** When two people call each other at the same moment, each receives the other's offer while its own still rings. Rather than both answering `busy`, both sides keep the call with the higher call id: its caller ignores the other offer, and the other side ends its own call (sending `hangup`) and rings with the offer it received.
- **Answer.** The callee answers with its own fresh secret.
- **Signal.** This carries the media layer's session descriptions and ICE candidates. It is tracked, so it survives a replaced session. Apps never see it, and neither do their logs.
- **Reaction.** One emoji, at most 32 bytes with no control characters or whitespace, that both apps float up over the call's video for a few seconds. It's only accepted in an answered call, and reactions arriving less than 150 ms after the last one are dropped, so a peer can't flood the screen. It isn't stored or shown anywhere else. A peer that predates it rejects it as an unknown op, and nothing floats there.
- **Timeouts.** An unanswered call ends after 60 s. An answered call whose media stops arriving for 30 s ends as `failed`.
- **Failover.** A call outlives its session. If the session ends mid-call (Wi-Fi went, the Bluetooth link dropped), each side reaches the peer again at once by every path a new call would use: the last address, the DHT rendezvous, and a relay circuit. Whichever comes up first carries the call, and a relayed one moves to a direct path later. If media stops for 4 s on a session that still looks up (its path may have died before the lease notices), the side punches and dials where the peer may be, as the side that moved does, at most every 15 s. A new session replaces the old one, and the media continues. The phone also keeps its standby path warm during a call (Appendix N, Fast recovery), so losing Wi-Fi costs about a second.
- **Pacing.** Call messages and stream-carried media go out immediately, even under constant-rate cover traffic. A call's media reveals the call anyway, and setup can't wait for 2-second ticks.

## Media keys

```
base      = KDF("call media secret", offer_secret, answer_secret, call_id)
k_to_callee = KDF("call media direction key", base, "to callee")
k_to_caller = KDF("call media direction key", base, "to caller")
```

- **Strength of the keys.** Both secrets travel only inside the ratchet. The keys are therefore as strong as the session (post-quantum hybrid), need both sides' randomness, and are new for every call. They are erased when the call ends.
- **Independent of the session.** The keys don't come from the session. A call keeps going when its session is replaced, for example after a network change.

## Media packets

```
packet = call (8) || seq (8) || ChaCha20-Poly1305(k_dir, nonce = 0^4 || seq,
                                                  ad = call || seq,
                                                  stream (1) || len (2) || payload || 0^pad)
```

- **Sequence numbers.** `seq` counts from 0 in each direction, so nonces never repeat.
- **Replays.** Receivers keep a 1024-packet replay window. Anything already seen, or older than the window, is dropped. Reordering within the window is fine.
- **Padding.** Plaintext is padded to a multiple of 32 bytes. Media layers should also use constant-bitrate audio, because variable-bitrate speech leaks content through packet sizes.
- **Size limit.** The largest payload is 1400 bytes.
- **QUIC paths.** Over a QUIC session, packets go as QUIC datagrams (RFC 9221). Quinn's send buffer is small (64 KiB), so under congestion old media is dropped, not queued. A packet too large for the path's current datagram size goes in the stream instead.
- **Other links.** On links without datagrams (TCP, Bluetooth, relays), packets go as `AppMessage::Media` (kind 26) in the session stream: delivered in order and late rather than lost.
- **Moving to QUIC.** When a call starts on a direct TCP session, the side that dialed it also dials the peer's QUIC endpoint at the same address (nodes listen on both). The new session replaces the TCP one, and the call carries on, since its keys aren't the session's. Only the dialer does this, so the two sides never race. Over TCP on a phone's Wi-Fi, video stalled for seconds behind single late packets. Over QUIC it ran at a steady 25 to 30 frames a second.

## Video

Every call negotiates an audio and a video track from the start, on both sides. The video track stays disabled until that side turns its camera on (`CallMsg::Update`), so a camera never needs a new offer, and the two sides can't collide by renegotiating at once.

- **Apps send frames.** They give the call I420 frames, 640×480 (WebRTC scales them and adapts to the bandwidth), with how far to turn each to be upright. The Android app reads the front camera (Camera2) and gives the sensor's orientation. The Linux app reads the camera through GStreamer (`v4l2src`).
- **Apps receive frames.** They get the peer's newest frame as RGBA, with its rotation, through `next_video_frame`. Older frames are dropped, since a late frame isn't worth drawing.
- **Previews.** Each app shows its own picture from the camera directly, mirrored. It never goes through the call.
- **Camera lifetime.** The camera runs only while the call wants video. On Android it also needs the call screen open. It stops when the call ends.

## The loopback TURN server

`threnody-media::turn` is a TURN server (RFC 8656) for one client and one peer.

- **Credentials.** It uses long-term credentials, random for each call, and serves only clients that authenticate with them. Other local programs can neither use it nor read from it.
- **Relayed addresses.** Each side's relayed addresses are fixed TEST-NET-1 addresses, `192.0.2.1` for the caller and `192.0.2.2` for the callee, so the two sides' relay candidates pair up. Ports are high because WebRTC ignores remote candidates on most ports below 1024.
- **One allocation per network.** WebRTC allocates once for each network it sees: on a phone, one for Wi-Fi and one for mobile data. Allocation *i* is relayed at port 40000 + *i* (up to 16; past that, the one quiet longest gives way). The call media's stream id says which allocations a packet is between: the sender's in its high four bits, the receiver's in its low four. WebRTC therefore checks a path over each network and moves to mobile data the moment Wi-Fi goes, without a new offer. When only the first allocations are involved, the stream id is 0, as it was when there was one allocation, so older peers still connect. Before this, the server served only the first client, so a call whose media ran over Wi-Fi died with Wi-Fi even when the session moved to mobile data at once.
- **Permissions.** Permissions and channels are granted only for the peer's relayed addresses.
- **No address leaks.** WebRTC is configured with `IceTransportsType::Relay`. With the defaults it would have offered the device's LAN IPv4 and global IPv6 addresses as host candidates. Here it offers none.

Inside, WebRTC still runs DTLS-SRTP. Its certificate fingerprints travel only in the end-to-end signalling, so they are authenticated.

## Group calls

A group call is a call among up to eight devices (`MAX_GROUP_CALL`) that belong to one MLS group (Appendix F). Every two participants share a **leg**: a WebRTC session of its own, sealed and carried exactly like a two-person call's media. A pair without a session of its own sends its leg through a participant that has sessions with both.

### Signalling

Group call messages travel as group content kind 9 (Appendix F), inside the group's MLS encryption, so they are authenticated as coming from that member:

```
GroupCallMsg = { 0: op, 1: call uint, ? 2: device bstr(32), ? 3: flags uint,
                 ? 4: data bstr, ? 5: secret bstr(32), ? 6: links [* bstr(32)] }
op: 1 start (flags, secret), 2 join (flags, links), 3 leave,
    4 leg (device = to, data ≤ 64 KiB), 6 update (flags, links)
flags: 1 = video
```

- **Start.** The starter picks a random non-zero call id and a fresh 32-byte call secret. It rings every other member's devices, except the starter's own other devices. The call secret goes only inside the group's encryption and isn't tied to the group's epoch, so members at different epochs still agree on it.
- **Join and leave.** These keep everyone's roster. A member who sees a join from someone new, while in the call itself, answers with its own `join` to that member alone. A joiner therefore learns everyone already in the call, even when someone's join reached it before the call's start did. A call rings for 60 s, then stops ringing (the call goes on). With no session to a member, call messages go through another member that has one (Appendix F forwarding), but they are never held or sealed for later, since a call can't wait.
- **Links.** `join` and `update` carry the sender's **links**: the participants it has a live session with. A participant sends an `update` whenever its links change. Links say who can carry a leg for whom.
- **Leg signalling.** Session descriptions and ICE candidates for the leg between A and B go over A and B's own ratchet when they have a session (`CallMsg::Signal` under the leg's id). Otherwise they go in a group message addressed to B alone (op 4). Either way they never pass through a relay as anything it could change, so no relay can stand in the middle of a leg's DTLS.
- **The hub.** The participant linked to the most others (ties: the smaller key) is shown as the call's hub. It carries the most legs, but nothing depends on agreeing who it is.
- **Busy.** One-to-one and group calls exclude each other: an offer during a group call gets `busy`, and a group call doesn't ring during a one-to-one call.

### Legs

```
leg id   = first 8 bytes of KDF("group call leg id", call id, min(key_A, key_B), max(key_A, key_B)), never 0
leg keys = MediaKeys(leg id, s, s, caller = A < B),
           s = KDF("group call leg", call secret, call id, min(key_A, key_B), max(key_A, key_B))
```

The device with the smaller key makes the WebRTC offer, and counts as the leg's caller for its direction keys. Packets use the format above, with the leg id where a call id goes.

A pair without a session sends its packets to a participant R that it reaches and that is linked to the other end (the hub first), wrapped as `0^8 || device (32) || packet`. Call id 0 is never a call's, so older peers drop it. R passes a wrapped packet on, rewrapped with the sender's key, only when both ends are current participants and the packet's id is the id of the leg between them. The receiver opens a wrapped packet only when its id is the id of its own leg with the device named. Every participant can derive every leg's transport keys. What keeps the media of other legs from them, relays included, is the DTLS-SRTP inside each leg, whose fingerprints travel only in authenticated signalling.

### Media

- A leg over a direct TCP session that we dialed moves to QUIC, as a two-person call does (*Moving to QUIC* above), so its media goes as datagrams.
- One WebRTC session runs per leg. They share the microphone (the platform audio device is reference-counted) and one camera source, so each camera frame feeds every leg.
- Each participant uploads one stream per other participant, so the video uplink is divided among the legs: 700 kbit/s for one leg, down to 150 kbit/s each.
- A leg with no media for 30 s drops that participant from our view of the call. They come back by joining again.
- Apps get each participant's newest frame (`next_group_video_frame`) and show a tile per participant. Reactions are only for calls with one contact.

## Platforms

`threnody-media` uses LiveKit's `libwebrtc` crate (Apache-2.0), which ships prebuilt WebRTC for Linux, macOS, Windows, iOS and Android.

- **Linux.** Done: audio through the platform audio device module (PulseAudio/PipeWire), with WebRTC's echo cancellation, noise suppression and gain control.
- **Android.** Done, tested on a Pixel 8a against a laptop, calling in both directions. Details:
  - *JavaVM, without our own `unsafe`.* WebRTC needs the `JavaVM`. Its prebuilt archive has its own `JNI_OnLoad`. `threnody-ffi/build.rs` passes the linker `android-jni.ld`, an `EXTERN(...)` list of `JNI_OnLoad` and every `Java_livekit_org_webrtc_*` native, so they are kept. It also passes `android-jni.map`, so they are exported. The app calls `System.loadLibrary("threnody_ffi")` before the node opens, since uniffi's JNA loading doesn't run `JNI_OnLoad`. Then it calls `livekit.org.webrtc.ContextUtils.initialize`. `build.sh` copies LiveKit's `libwebrtc.jar` into the app. Regenerate `android-jni.ld` when upgrading `libwebrtc`.
  - *The audio device across calls.* WebRTC terminates its media engine when the last peer connection closes, and initialises it again for the next one. LiveKit's audio-device proxy terminates the Android audio device then, but never re-initialises it, so the second call crashed. An idle peer connection, held for the life of the process, keeps the engine up (`rtc::factory`).
  - *Ringing.* An incoming call posts a `CallStyle` notification with a full-screen intent, insistent, on a channel that plays the phone's ringtone as a ringtone. Silent mode and Do Not Disturb therefore apply, and Answer and Decline work from the lock screen.
  - *Audio routing.* While a call runs, the app uses `MODE_IN_COMMUNICATION`. Audio goes to the speaker if chosen, else to a connected headset (LE Audio, Bluetooth SCO or a hearing aid), else to the speaker for video or to the earpiece (or a wired headset). The app watches audio devices during the call: when a headset drops, audio moves to the earpiece (the speaker for video) at once, and back when it reconnects. On Android 10 and 11 it starts Bluetooth SCO itself.
  - *Telecom.* Each call is also a self-managed `ConnectionService` call, so car audio and headset buttons answer and hang up, the audio follows Bluetooth hands-free, and a phone call that comes in puts ours on hold (our microphone goes quiet). Telecom then sets the audio mode and routes the audio, and the app's speaker switch goes through it. The system learns nothing about who the call is with: the address is a fixed `threnody:call`, the name shown is "Threnody call", and self-managed calls stay out of the system call log. Where Telecom refuses a call, it rings and routes as before.
  - *Size.* The APK grows by about 20 MB (WebRTC).
- **macOS, iOS, Windows.** These need their apps, which reach calls through the FFI (`start_call`, `answer_call`, `hangup_call`, `set_call_muted`, `current_call` and the `Call*` events).

## Not yet

- Video: screen sharing, hardware-texture rendering (frames are copied as RGBA), and a frame size limited to the path's QUIC datagrams before path MTU discovery.
- Group calls: mixing or forwarding one upload for everyone (an SFU with frame encryption), which needs RTP forwarding below WebRTC; and calls bigger than eight. Group calls on iOS, which has no calls yet.
- Media over onion circuits.
- A Tamarin model of the call key derivation.
- Fuzzing `CallMsg`.
