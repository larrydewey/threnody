# Appendix Q: Voice and Video Calls (v1)

Status: signalling, media keys and transport implemented in `threnody-core::call` and `threnody-net::call`. Media (WebRTC) is in `threnody-media`; the FFI, the Linux app and the Android app use it. Voice and video between two people; group calls are not done yet.

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
            ? 4: reason uint, ? 5: data bstr }
op: 1 offer (secret, flags), 2 ringing, 3 answer (secret, flags),
    4 hangup (reason), 5 update (flags), 6 signal (data ≤ 64 KiB)
flags: 1 = video
reason: 0 ended, 1 declined, 2 busy, 3 unanswered, 4 failed,
        5 answered elsewhere, 6 declined elsewhere (unknown = ended)
```

- **Offer.** The caller picks a random non-zero 64-bit call id and a fresh 32-byte secret.
- **Ringing.** The callee says it is ringing. Offers from someone not accepted (a message request) never ring. They get no answer at all, so a stranger can't tell whether we're there.
- **Busy.** An offer while a call is in progress gets `busy`.
- **Answer.** The callee answers with its own fresh secret.
- **Signal.** This carries the media layer's session descriptions and ICE candidates. It is tracked, so it survives a replaced session. Apps never see it, and neither do their logs.
- **Timeouts.** An unanswered call ends after 60 s. An answered call whose media stops arriving for 30 s ends as `failed`.
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

- **Credentials.** It uses long-term credentials, random for each call, and answers only the first client that authenticates. Other local programs can neither use it nor read from it.
- **Relayed addresses.** Each side's relayed address is a fixed TEST-NET-1 address, `192.0.2.1:40000` for the caller and `192.0.2.2:40000` for the callee, so the two sides' relay candidates pair up. The port is high because WebRTC ignores remote candidates on most ports below 1024.
- **Permissions.** Permissions and channels are granted only for the peer's relayed address.
- **No address leaks.** WebRTC is configured with `IceTransportsType::Relay`. With the defaults it would have offered the device's LAN IPv4 and global IPv6 addresses as host candidates. Here it offers none.

Inside, WebRTC still runs DTLS-SRTP. Its certificate fingerprints travel only in the end-to-end signalling, so they are authenticated.

## Platforms

`threnody-media` uses LiveKit's `libwebrtc` crate (Apache-2.0), which ships prebuilt WebRTC for Linux, macOS, Windows, iOS and Android.

- **Linux.** Done: audio through the platform audio device module (PulseAudio/PipeWire), with WebRTC's echo cancellation, noise suppression and gain control.
- **Android.** Done, tested on a Pixel 8a against a laptop, calling in both directions. Details:
  - *JavaVM, without our own `unsafe`.* WebRTC needs the `JavaVM`. Its prebuilt archive has its own `JNI_OnLoad`. `threnody-ffi/build.rs` passes the linker `android-jni.ld`, an `EXTERN(...)` list of `JNI_OnLoad` and every `Java_livekit_org_webrtc_*` native, so they are kept. It also passes `android-jni.map`, so they are exported. The app calls `System.loadLibrary("threnody_ffi")` before the node opens, since uniffi's JNA loading doesn't run `JNI_OnLoad`. Then it calls `livekit.org.webrtc.ContextUtils.initialize`. `build.sh` copies LiveKit's `libwebrtc.jar` into the app. Regenerate `android-jni.ld` when upgrading `libwebrtc`.
  - *The audio device across calls.* WebRTC terminates its media engine when the last peer connection closes, and initialises it again for the next one. LiveKit's audio-device proxy terminates the Android audio device then, but never re-initialises it, so the second call crashed. An idle peer connection, held for the life of the process, keeps the engine up (`rtc::factory`).
  - *Ringing.* An incoming call posts a `CallStyle` notification with a full-screen intent, insistent, on a channel that plays the phone's ringtone as a ringtone. Silent mode and Do Not Disturb therefore apply, and Answer and Decline work from the lock screen.
  - *Audio routing.* While a call runs, the app uses `MODE_IN_COMMUNICATION`, routes to the earpiece, and switches to the speaker on request.
  - *Size.* The APK grows by about 20 MB (WebRTC).
- **macOS, iOS, Windows.** These need their apps, which reach calls through the FFI (`start_call`, `answer_call`, `hangup_call`, `set_call_muted`, `current_call` and the `Call*` events).

## Not yet

- Video: switching cameras, screen sharing, hardware-texture rendering (frames are copied as RGBA), and a frame size limited to the path's QUIC datagrams before path MTU discovery.
- Android: a calls foreground-service type (the microphone in the background), and Telecom integration (`ConnectionService`: Bluetooth headsets, car audio, other calls).
- Ringing every device of an account, with `answered elsewhere` and `declined elsewhere`.
- Group calls.
- Media over onion circuits.
- A Tamarin model of the call key derivation.
- Fuzzing `CallMsg`.
