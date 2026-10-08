//! A real call between two nodes: signalling through the ratchet, WebRTC
//! media through the call (QUIC datagrams, or the session stream), audio
//! arriving at the other end.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::StreamExt;
use libwebrtc::audio_frame::AudioFrame;
use libwebrtc::audio_source::AudioSourceOptions;
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::audio_stream::native::NativeAudioStream;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::PeerConnectionState;
use threnody_core::Identity;
use threnody_core::message::FEATURE_CALLS;
use threnody_core::store::Home;
use threnody_media::CallMedia;
use threnody_media::rtc::AudioIn;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::{OnceCell, mpsc};
use tokio::time::timeout;

const RATE: u32 = 48_000;

fn node(
    dir: &tempfile::TempDir,
    name: &str,
    cover: bool,
) -> (Node, mpsc::UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity: Identity = home.create_identity(None).unwrap();
    Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        // The apps' default: a frame every 2 s, cover traffic otherwise.
        constant_rate: cover.then_some(Duration::from_secs(2)),
        tunnel_port: None,
    })
    .unwrap()
}

/// Hands the node's call signalling to its media once that exists (it
/// may arrive first), and reports the call's start.
fn route(
    mut rx: mpsc::UnboundedReceiver<Event>,
    media: Arc<OnceCell<CallMedia>>,
) -> mpsc::UnboundedReceiver<Event> {
    let (tx, out) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut early = Vec::new();
        while let Some(e) = rx.recv().await {
            match e {
                Event::CallSignal { data, .. } => early.push(data),
                e => {
                    let _ = tx.send(e);
                }
            }
            if let Some(m) = media.get() {
                for d in early.drain(..) {
                    m.on_signal(&d).await.unwrap();
                }
            }
        }
    });
    out
}

async fn wait(rx: &mut mpsc::UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(10), async {
        loop {
            let e = rx.recv().await.expect("events");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
    .expect("event")
}

async fn talk(quic: bool, cover: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (alice, arx) = node(&dir, "alice", cover);
    let (bob, brx) = node(&dir, "bob", cover);
    let fp = Some(bob.identity().fingerprint());
    let b_id = if quic {
        alice.listen_quic("127.0.0.1:0").await.unwrap();
        let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();
        alice.connect_quic(addr, fp).await.unwrap()
    } else {
        let addr = bob.listen("127.0.0.1:0").await.unwrap();
        alice.connect(&addr.to_string(), fp).await.unwrap()
    };
    bob.accept_contact(&alice.identity());
    timeout(Duration::from_secs(10), async {
        while !(alice.supports(&b_id, FEATURE_CALLS)
            && bob.supports(&alice.identity(), FEATURE_CALLS))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(alice.media_datagrams(&b_id), quic);

    let (a_media, b_media) = (Arc::new(OnceCell::new()), Arc::new(OnceCell::new()));
    let mut arx = route(arx, a_media.clone());
    let mut brx = route(brx, b_media.clone());

    let id = alice.start_call(&b_id, false).unwrap();
    wait(&mut brx, |e| matches!(e, Event::CallIncoming { .. })).await;
    bob.answer_call(id, false).unwrap();
    let (track_tx, mut track_rx) = mpsc::unbounded_channel();
    let (state_tx, mut state_rx) = mpsc::unbounded_channel();
    let m = CallMedia::start(
        &bob,
        id,
        AudioIn::None,
        move |t| {
            let _ = track_tx.send(t.track);
        },
        move |s| {
            let _ = state_tx.send(s);
        },
    )
    .await
    .unwrap();
    let _ = b_media.set(m);

    wait(&mut arx, |e| matches!(e, Event::CallStarted { .. })).await;
    let tone = NativeAudioSource::new(AudioSourceOptions::default(), RATE, 1, 100);
    let m = CallMedia::start(&alice, id, AudioIn::Source(tone.clone()), |_| {}, |_| {})
        .await
        .unwrap();
    let _ = a_media.set(m);

    timeout(Duration::from_secs(20), async {
        while state_rx.recv().await != Some(PeerConnectionState::Connected) {}
    })
    .await
    .expect("media connected through the call");

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
    let MediaStreamTrack::Audio(track) = timeout(Duration::from_secs(10), track_rx.recv())
        .await
        .expect("a remote track")
        .unwrap()
    else {
        panic!("expected audio")
    };
    let mut stream = NativeAudioStream::new(track, RATE as i32, 1);
    let loud = timeout(Duration::from_secs(10), async {
        let mut loud = 0;
        while let Some(f) = stream.next().await {
            let energy = f.data.iter().map(|s| i64::from(*s).abs()).sum::<i64>();
            if energy / f.data.len().max(1) as i64 > 1000 {
                loud += 1;
                if loud >= 50 {
                    break;
                }
            }
        }
        loud
    })
    .await
    .expect("audio arrived");
    assert!(loud >= 50);

    alice.hangup_call(id);
    wait(&mut brx, |e| matches!(e, Event::CallEnded { .. })).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_call_over_quic_datagrams() {
    talk(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_call_over_tcp() {
    talk(false, false).await;
}

/// Constant-rate cover traffic doesn't hold back a call's signalling or
/// (on a link without datagrams) its media.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn audio_call_under_cover_traffic() {
    talk(false, true).await;
}
