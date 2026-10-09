//! Runs a node's call media for an application: starts it when a call is
//! answered, hands it the peer's signalling (holding what arrives before
//! the media exists), and ends it with the call.
//!
//! The application forwards three things: that a call started
//! ([`Driver::start`]: the callee right after answering, the caller on
//! `Event::CallStarted`), each `Event::CallSignal` ([`Driver::signal`]),
//! and `Event::CallEnded` ([`Driver::end`]). Camera frames go in through
//! [`Driver::send_video`]; the peer's come out of [`Driver::next_frame`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::StreamExt;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::{BoxVideoFrame, I420Buffer, VideoBuffer, VideoRotation};
use libwebrtc::video_stream::native::NativeVideoStream;
use threnody_net::Node;
use tokio::sync::{Notify, mpsc};

use crate::CallMedia;
use crate::rtc::{AudioIn, PeerConnectionState};

enum Cmd {
    Start(u64, bool),
    Signal(u64, Vec<u8>),
    End,
}

/// How the call's media is doing, for the user interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaState {
    /// Audio flows both ways.
    Connected,
    /// It stopped flowing; WebRTC is trying to recover.
    Interrupted,
    /// It couldn't start, or couldn't recover: the call was hung up.
    Failed,
}

pub type OnState = Arc<dyn Fn(u64, MediaState) + Send + Sync>;

/// A frame of the peer's video, ready to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Degrees clockwise to turn it to be upright.
    pub rotation: u32,
    /// `width × height` pixels, 4 bytes each: red, green, blue, alpha.
    pub rgba: Vec<u8>,
}

/// The newest remote frame, and a wake-up for whoever waits for it. Older
/// ones are dropped: a late frame isn't worth drawing. Kept as decoded and
/// turned into pixels only when asked for ([`Driver::next_frame`]), so a
/// screen that isn't drawing (an app in the background) costs nothing,
/// and the conversion happens on the app's thread, not the call's.
#[derive(Default)]
struct Latest {
    frame: Mutex<Option<BoxVideoFrame>>,
    ready: Notify,
}

type Current = Arc<Mutex<Option<(u64, Arc<CallMedia>)>>>;

pub struct Driver {
    tx: mpsc::UnboundedSender<Cmd>,
    current: Current,
    latest: Arc<Latest>,
    /// Calls use the microphone and speaker (else silence, played nowhere).
    devices: Arc<std::sync::atomic::AtomicBool>,
}

impl Driver {
    /// Starts the driver's task on the current Tokio runtime.
    pub fn new(node: Node, on_state: OnState) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let current: Current = Arc::default();
        let latest: Arc<Latest> = Arc::default();
        let devices = Arc::new(std::sync::atomic::AtomicBool::new(true));
        tokio::spawn(run(
            node,
            rx,
            on_state,
            current.clone(),
            latest.clone(),
            devices.clone(),
        ));
        Self {
            tx,
            current,
            latest,
            devices,
        }
    }

    /// Whether calls from now on use the microphone and speaker (on by
    /// default). Off, they send silence and play nothing: for test peers
    /// and bots, whose speaker would otherwise echo into their microphone.
    pub fn set_devices(&self, on: bool) {
        self.devices.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    /// Starts `call`'s media, sending video from the start if `video`.
    pub fn start(&self, call: u64, video: bool) {
        let _ = self.tx.send(Cmd::Start(call, video));
    }

    pub fn signal(&self, call: u64, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Signal(call, data));
    }

    pub fn end(&self) {
        let _ = self.tx.send(Cmd::End);
    }

    fn media(&self) -> Option<Arc<CallMedia>> {
        lock(&self.current).as_ref().map(|(_, m)| m.clone())
    }

    pub fn set_muted(&self, muted: bool) {
        if let Some(m) = self.media() {
            m.set_muted(muted);
        }
    }

    pub fn set_video(&self, on: bool) {
        if let Some(m) = self.media() {
            m.set_video(on);
        }
    }

    /// One camera frame: I420 planes, `width × height` luma then each
    /// chroma plane at half size, turned `rotation` degrees clockwise to be
    /// upright. Returns false when there's no call or the size is wrong.
    pub fn send_video(&self, width: u32, height: u32, rotation: u32, i420: &[u8]) -> bool {
        let Some(m) = self.media() else { return false };
        let (cw, ch) = (width.div_ceil(2) as usize, height.div_ceil(2) as usize);
        let luma = width as usize * height as usize;
        if width == 0 || height == 0 || i420.len() < luma + 2 * cw * ch {
            return false;
        }
        let mut b = I420Buffer::new(width, height);
        let (sy, su, sv) = b.strides();
        let (y, u, v) = b.data_mut();
        copy_plane(
            &i420[..luma],
            width as usize,
            y,
            sy as usize,
            height as usize,
        );
        let (cu, cv) = i420[luma..].split_at(cw * ch);
        copy_plane(cu, cw, u, su as usize, ch);
        copy_plane(&cv[..cw * ch], cw, v, sv as usize, ch);
        m.send_video(&b, rotation);
        true
    }

    /// One line on how the current call's media is doing, for the
    /// diagnostics log: delays, rates and freezes, never what's in it.
    pub async fn report(&self) -> Option<String> {
        self.media()?.report().await
    }

    /// The newest frame of the peer's video, waiting up to `timeout` for
    /// one; each frame is returned once.
    pub async fn next_frame(&self, timeout: Duration) -> Option<Frame> {
        if lock(&self.latest.frame).is_none() {
            let _ = tokio::time::timeout(timeout, self.latest.ready.notified()).await;
        }
        let f = lock(&self.latest.frame).take()?;
        Some(pixels(&f))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn copy_plane(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, rows: usize) {
    for r in 0..rows {
        let n = src_stride.min(dst_stride);
        dst[r * dst_stride..r * dst_stride + n]
            .copy_from_slice(&src[r * src_stride..r * src_stride + n]);
    }
}

/// A decoded frame as RGBA.
fn pixels(f: &BoxVideoFrame) -> Frame {
    let b = f.buffer.to_i420();
    let (w, h) = (b.width(), b.height());
    let (sy, su, sv) = b.strides();
    let (y, u, v) = b.data();
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    // libyuv's "ABGR" is R, G, B, A in memory.
    yuv_helper::i420_to_abgr(y, sy, u, su, v, sv, &mut rgba, w * 4, w as i32, h as i32);
    let rotation = match f.rotation {
        VideoRotation::VideoRotation90 => 90,
        VideoRotation::VideoRotation180 => 180,
        VideoRotation::VideoRotation270 => 270,
        VideoRotation::VideoRotation0 => 0,
    };
    Frame {
        width: w,
        height: h,
        rotation,
        rgba,
    }
}

/// Keeps the peer's newest frame in `latest` until the track ends.
async fn watch_video(track: libwebrtc::video_track::RtcVideoTrack, latest: Arc<Latest>) {
    let mut stream = NativeVideoStream::new(track);
    while let Some(f) = stream.next().await {
        *lock(&latest.frame) = Some(f);
        latest.ready.notify_waiters();
    }
}

async fn run(
    node: Node,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    on_state: OnState,
    current: Current,
    latest: Arc<Latest>,
    devices: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut early: Vec<(u64, Vec<u8>)> = Vec::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Start(call, video) => {
                let report = on_state.clone();
                let failed = node.clone();
                let frames = latest.clone();
                // `on_track` runs on a WebRTC thread, outside the runtime.
                let rt = tokio::runtime::Handle::current();
                let started = CallMedia::start(
                    &node,
                    call,
                    if devices.load(std::sync::atomic::Ordering::Relaxed) {
                        AudioIn::Device
                    } else {
                        AudioIn::None
                    },
                    move |t| {
                        if let MediaStreamTrack::Video(v) = t.track {
                            rt.spawn(watch_video(v, frames.clone()));
                        }
                    },
                    move |s| match s {
                        PeerConnectionState::Connected => report(call, MediaState::Connected),
                        PeerConnectionState::Disconnected => report(call, MediaState::Interrupted),
                        PeerConnectionState::Failed => {
                            report(call, MediaState::Failed);
                            failed.fail_call(call);
                        }
                        _ => {}
                    },
                )
                .await;
                match started {
                    Ok(m) => {
                        m.set_video(video);
                        for (_, d) in early.drain(..).filter(|(c, _)| *c == call) {
                            let _ = m.on_signal(&d).await;
                        }
                        *lock(&current) = Some((call, Arc::new(m)));
                    }
                    Err(_) => {
                        on_state(call, MediaState::Failed);
                        node.fail_call(call);
                    }
                }
            }
            Cmd::Signal(call, d) => {
                let media = lock(&current)
                    .as_ref()
                    .filter(|(c, _)| *c == call)
                    .map(|(_, m)| m.clone());
                match media {
                    Some(m) => {
                        let _ = m.on_signal(&d).await;
                    }
                    None => early.push((call, d)),
                }
            }
            Cmd::End => {
                *lock(&current) = None;
                *lock(&latest.frame) = None;
                early.clear();
            }
        }
    }
}
