//! Two WebRTC sessions whose only path to each other is our TURN servers,
//! joined here in memory where a call would carry the media: audio sent
//! as a tone on one side arrives on the other.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::PeerConnectionState;
use threnody_media::rtc::{AudioIn, MediaSession, Wires};
use tokio::sync::mpsc;

const RATE: u32 = 48_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_crosses_only_through_the_call() {
    // What each side sends the other: (signal?, bytes).
    let (a_out, mut a_rx) = mpsc::unbounded_channel::<(bool, Vec<u8>)>();
    let (b_out, mut b_rx) = mpsc::unbounded_channel::<(bool, Vec<u8>)>();
    let wires = |out: mpsc::UnboundedSender<(bool, Vec<u8>)>| {
        let o = out.clone();
        Wires {
            signal: Box::new(move |s| {
                let _ = o.send((true, s));
            }),
            media: Box::new(move |m| {
                let _ = out.send((false, m));
            }),
        }
    };
    let relayed = Arc::new(Mutex::new(0usize));

    let tone = NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 100);
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let (track_tx, mut track_rx) = mpsc::unbounded_channel();
    let caller = MediaSession::start(
        true,
        AudioIn::Source(tone.clone()),
        wires(a_out),
        |_| {},
        move |s| {
            let _ = state_tx.send(s);
        },
    )
    .await
    .unwrap();
    let callee = MediaSession::start(
        false,
        AudioIn::None,
        wires(b_out),
        move |t| {
            let _ = track_tx.send(t.track);
        },
        |_| {},
    )
    .await
    .unwrap();
    let (caller, callee) = (Arc::new(caller), Arc::new(callee));

    // The "call": everything one side sends goes to the other.
    let pump = |mut rx: mpsc::UnboundedReceiver<(bool, Vec<u8>)>, to: Arc<MediaSession>| {
        let relayed = relayed.clone();
        tokio::spawn(async move {
            while let Some((signal, b)) = rx.recv().await {
                if signal {
                    to.on_signal(&b).await.unwrap();
                } else {
                    *relayed.lock().unwrap() += 1;
                    to.on_media(b);
                }
            }
        })
    };
    pump(a_rx_take(&mut a_rx), callee.clone());
    pump(a_rx_take(&mut b_rx), caller.clone());

    tokio::time::timeout(Duration::from_secs(20), async {
        while state_rx.recv().await != Some(PeerConnectionState::Connected) {}
    })
    .await
    .expect("connected through the relays");

    // A 440 Hz tone, 10 ms frames.
    tokio::spawn(async move {
        let per = (RATE / 100) as usize;
        for n in 0.. {
            let data: Vec<i16> = (0..per)
                .map(|i| {
                    let t = (n * per + i) as f32 / RATE as f32;
                    ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16
                })
                .collect();
            let frame = AudioFrame {
                data: Cow::Owned(data),
                sample_rate: RATE,
                num_channels: 1,
                samples_per_channel: per as u32,
            };
            if tone.capture_frame(&frame).await.is_err() {
                return;
            }
        }
    });

    // Sessions carry a video track too (off here): the audio one.
    let track = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(MediaStreamTrack::Audio(t)) = track_rx.recv().await {
                return t;
            }
        }
    })
    .await
    .expect("a remote audio track");
    let mut stream = NativeAudioStream::new(track, RATE as i32, 1);
    let loud = tokio::time::timeout(Duration::from_secs(10), async {
        let mut loud = 0;
        while let Some(f) = stream.next().await {
            let energy = f.data.iter().map(|s| i64::from(*s).abs()).sum::<i64>();
            if energy / f.data.len().max(1) as i64 > 1000 {
                loud += 1;
                if loud >= 50 {
                    return loud;
                }
            }
        }
        loud
    })
    .await
    .expect("audio arrived");
    assert!(loud >= 50, "half a second of the tone came through");
    assert!(
        *relayed.lock().unwrap() > 50,
        "all of it through our relays"
    );
}

fn a_rx_take(
    rx: &mut mpsc::UnboundedReceiver<(bool, Vec<u8>)>,
) -> mpsc::UnboundedReceiver<(bool, Vec<u8>)> {
    let (_, empty) = mpsc::unbounded_channel();
    std::mem::replace(rx, empty)
}
