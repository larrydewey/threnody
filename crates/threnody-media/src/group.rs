//! A group call's media: one WebRTC session per leg (one per other
//! participant), sharing our microphone and one camera source.
//!
//! The side of a leg with the smaller device key makes the offer, as it
//! counts as the leg's caller for its keys. Each leg's signalling leaves
//! through `signal` (the group layer sends it over a session or in a
//! group message), and comes back through [`GroupMedia::signal`]. What
//! arrives before its leg exists is held.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use futures_lite::StreamExt;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::PeerConnectionState;
use libwebrtc::video_frame::{BoxVideoFrame, I420Buffer};
use libwebrtc::video_source::native::NativeVideoSource;
use libwebrtc::video_stream::native::NativeVideoStream;
use threnody_core::PublicIdentity;
use threnody_net::Node;
use tokio::sync::Notify;

use crate::rtc::{self, AudioIn, MediaSession, VIDEO_MAX_BITRATE, VideoOut, Wires};

/// The least a leg's video gets, however many share the uplink.
pub const MIN_LEG_BITRATE: u64 = 150_000;

/// What each leg's video may send with `legs` legs sharing the uplink.
pub fn leg_bitrate(legs: usize) -> u64 {
    (VIDEO_MAX_BITRATE / legs.max(1) as u64).max(MIN_LEG_BITRATE)
}

pub type SignalOut = Arc<dyn Fn(PublicIdentity, Vec<u8>) + Send + Sync>;

/// Frames from each participant: the newest one each, taken once.
#[derive(Default)]
struct Frames {
    latest: Mutex<HashMap<PublicIdentity, BoxVideoFrame>>,
    ready: Notify,
}

pub struct GroupMedia {
    node: Node,
    call: u64,
    devices: bool,
    signal_out: SignalOut,
    camera: NativeVideoSource,
    legs: Mutex<HashMap<PublicIdentity, Arc<MediaSession>>>,
    early: Mutex<Vec<(PublicIdentity, Vec<u8>)>>,
    /// Held while signalling is applied: WebRTC must not read a
    /// connection's descriptions while another thread sets them.
    signalling: tokio::sync::Mutex<()>,
    frames: Arc<Frames>,
    muted: std::sync::atomic::AtomicBool,
    video: std::sync::atomic::AtomicBool,
    on_connected: Arc<dyn Fn() + Send + Sync>,
}

impl GroupMedia {
    /// Media for group call `call` (which we're in). `devices`: use the
    /// microphone and speaker (else silence, played nowhere).
    /// `on_connected` runs each time a leg's media starts flowing.
    pub fn new(
        node: Node,
        call: u64,
        devices: bool,
        video: bool,
        signal_out: SignalOut,
        on_connected: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            node,
            call,
            devices,
            signal_out,
            camera: rtc::video_source(),
            legs: Mutex::default(),
            early: Mutex::default(),
            signalling: tokio::sync::Mutex::new(()),
            frames: Arc::default(),
            muted: false.into(),
            video: video.into(),
            on_connected,
        }
    }

    pub fn call(&self) -> u64 {
        self.call
    }

    /// Opens our leg with `peer` (who joined, or was in the call when we
    /// joined).
    pub async fn add(&self, peer: PublicIdentity) -> Result<()> {
        if lock(&self.legs).contains_key(&peer) {
            return Ok(());
        }
        let mut from_peer = self
            .node
            .group_leg_media(self.call, &peer)
            .ok_or_else(|| anyhow::anyhow!("no leg with that participant"))?;
        let me = self.node.identity();
        let caller = me.as_bytes() < peer.as_bytes();
        let (out, n, call) = (self.signal_out.clone(), self.node.clone(), self.call);
        let wires = Wires {
            signal: Box::new(move |s| out(peer, s)),
            media: Box::new(move |stream, m| {
                let _ = n.send_leg_media(call, &peer, stream, &m);
            }),
        };
        let frames = self.frames.clone();
        let rt = tokio::runtime::Handle::current();
        let connected = self.on_connected.clone();
        let legs = lock(&self.legs).len() + 1;
        let session = Arc::new(
            MediaSession::start_with(
                caller,
                if self.devices {
                    AudioIn::Device
                } else {
                    AudioIn::None
                },
                VideoOut {
                    source: self.camera.clone(),
                    max_bitrate: leg_bitrate(legs),
                },
                wires,
                move |t| {
                    if let MediaStreamTrack::Video(v) = t.track {
                        rt.spawn(watch(peer, v, frames.clone()));
                    }
                },
                move |s| {
                    if s == PeerConnectionState::Connected {
                        connected();
                    }
                },
            )
            .await?,
        );
        session.set_muted(self.muted.load(std::sync::atomic::Ordering::Relaxed));
        session.set_video(self.video.load(std::sync::atomic::Ordering::Relaxed));
        let s = session.clone();
        tokio::spawn(async move {
            while let Some((stream, payload)) = from_peer.recv().await {
                s.on_media(stream, payload);
            }
        });
        let held: Vec<Vec<u8>> = {
            let mut early = lock(&self.early);
            let (mine, rest): (Vec<_>, Vec<_>) = early.drain(..).partition(|(p, _)| *p == peer);
            *early = rest;
            mine.into_iter().map(|(_, d)| d).collect()
        };
        let _one_at_a_time = self.signalling.lock().await;
        lock(&self.legs).insert(peer, session.clone());
        for d in held {
            let _ = session.on_signal(&d).await;
        }
        self.rebalance();
        Ok(())
    }

    /// Closes our leg with `peer` (who left).
    pub fn remove(&self, peer: &PublicIdentity) {
        lock(&self.legs).remove(peer);
        lock(&self.frames.latest).remove(peer);
        self.rebalance();
    }

    /// Signalling for our leg with `peer`.
    pub async fn signal(&self, peer: PublicIdentity, data: Vec<u8>) {
        let _one_at_a_time = self.signalling.lock().await;
        let leg = lock(&self.legs).get(&peer).cloned();
        match leg {
            Some(s) => {
                let _ = s.on_signal(&data).await;
            }
            None => lock(&self.early).push((peer, data)),
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted
            .store(muted, std::sync::atomic::Ordering::Relaxed);
        for s in lock(&self.legs).values() {
            s.set_muted(muted);
        }
    }

    pub fn set_video(&self, on: bool) {
        self.video.store(on, std::sync::atomic::Ordering::Relaxed);
        for s in lock(&self.legs).values() {
            s.set_video(on);
        }
    }

    /// One camera frame, for every leg at once.
    pub fn send_video(&self, frame: &I420Buffer, rotation: u32) {
        rtc::capture(&self.camera, frame, rotation);
    }

    /// The participants we have a leg with.
    pub fn peers(&self) -> Vec<PublicIdentity> {
        lock(&self.legs).keys().copied().collect()
    }

    /// The newest frame from some participant, waiting up to `timeout`
    /// for one. Each frame is returned once; participants take turns.
    pub async fn next_frame(
        &self,
        timeout: std::time::Duration,
    ) -> Option<(PublicIdentity, BoxVideoFrame)> {
        if lock(&self.frames.latest).is_empty() {
            let _ = tokio::time::timeout(timeout, self.frames.ready.notified()).await;
        }
        let mut latest = lock(&self.frames.latest);
        let peer = *latest.keys().min_by_key(|p| *p.as_bytes())?;
        latest.remove(&peer).map(|f| (peer, f))
    }

    /// Shares the uplink among the legs.
    fn rebalance(&self) {
        let legs = lock(&self.legs);
        let max = leg_bitrate(legs.len());
        for s in legs.values() {
            s.set_video_bitrate(max);
        }
    }
}

async fn watch(
    peer: PublicIdentity,
    track: libwebrtc::video_track::RtcVideoTrack,
    frames: Arc<Frames>,
) {
    let mut stream = NativeVideoStream::new(track);
    while let Some(f) = stream.next().await {
        lock(&frames.latest).insert(peer, f);
        frames.ready.notify_waiters();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legs_share_the_uplink_down_to_a_floor() {
        assert_eq!(leg_bitrate(1), VIDEO_MAX_BITRATE);
        assert_eq!(leg_bitrate(0), VIDEO_MAX_BITRATE);
        assert_eq!(leg_bitrate(2), VIDEO_MAX_BITRATE / 2);
        assert_eq!(leg_bitrate(7), MIN_LEG_BITRATE);
    }
}
