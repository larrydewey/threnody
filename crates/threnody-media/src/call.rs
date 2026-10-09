//! A Threnody call's media: a [`MediaSession`] wired to the node's call.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use libwebrtc::peer_connection::{PeerConnectionState, TrackEvent};
use threnody_net::Node;

use crate::rtc::{AudioIn, MediaSession, Wires};

/// The stream id WebRTC's packets use in the call's media.
const WEBRTC: u8 = 0;

pub struct CallMedia {
    session: Arc<MediaSession>,
}

impl CallMedia {
    /// Starts media for the active call `call` on `node`. The caller
    /// starts it on `Event::CallStarted`, the callee right after
    /// answering; the callee's must exist before the caller's offer is
    /// handed to [`CallMedia::on_signal`].
    pub async fn start(
        node: &Node,
        call: u64,
        audio: AudioIn,
        on_track: impl FnMut(TrackEvent) + Send + Sync + 'static,
        on_state: impl FnMut(PeerConnectionState) + Send + Sync + 'static,
    ) -> Result<Self> {
        let info = node
            .call()
            .filter(|c| c.call == call)
            .ok_or_else(|| anyhow!("no such call"))?;
        let mut from_peer = node
            .call_media(call)
            .ok_or_else(|| anyhow!("the call's media is already taken"))?;
        let (n1, n2) = (node.clone(), node.clone());
        let wires = Wires {
            signal: Box::new(move |s| {
                let _ = n1.send_call_signal(call, s);
            }),
            media: Box::new(move |m| {
                let _ = n2.send_media(call, WEBRTC, &m);
            }),
        };
        let session =
            Arc::new(MediaSession::start(info.outgoing, audio, wires, on_track, on_state).await?);
        let s = session.clone();
        tokio::spawn(async move {
            while let Some((stream, payload)) = from_peer.recv().await {
                if stream == WEBRTC {
                    s.on_media(payload);
                }
            }
        });
        Ok(Self { session })
    }

    /// Signalling from `Event::CallSignal`.
    pub async fn on_signal(&self, data: &[u8]) -> Result<()> {
        self.session.on_signal(data).await
    }

    pub fn set_muted(&self, muted: bool) {
        self.session.set_muted(muted);
    }

    /// Starts or stops sending our video (the peer hears about it through
    /// `Node::set_call_video`).
    pub fn set_video(&self, on: bool) {
        self.session.set_video(on);
    }

    /// One camera frame, turned `rotation` degrees clockwise to be upright.
    pub fn send_video(&self, frame: &libwebrtc::video_frame::I420Buffer, rotation: u32) {
        self.session.send_video(frame, rotation);
    }

    pub fn state(&self) -> PeerConnectionState {
        self.session.state()
    }
}
