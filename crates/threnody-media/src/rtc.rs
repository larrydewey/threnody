//! One call's WebRTC peer connection, reaching the peer only through our
//! loopback TURN server ([`crate::turn`]).
//!
//! The application wires two things to the call: [`Wires::signal`]
//! carries session descriptions and ICE candidates to the peer (as
//! `CallMsg::Signal`), and [`Wires::media`] carries what WebRTC sends the
//! peer's relayed address (as sealed call media). What arrives from the
//! peer goes back in through [`MediaSession::on_signal`] and
//! [`MediaSession::on_media`].
//!
//! DTLS-SRTP inside authenticates against fingerprints that only travel
//! in the end-to-end encrypted signalling; the call's own media keys
//! around it (post-quantum hybrid, from the ratchet) protect it in
//! transit.

use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use libwebrtc::audio_source::native::NativeAudioSource;
use libwebrtc::ice_candidate::IceCandidate;
use libwebrtc::media_stream_track::MediaStreamTrack;
use libwebrtc::peer_connection::{OfferOptions, PeerConnection};
pub use libwebrtc::peer_connection::{PeerConnectionState, TrackEvent};
use libwebrtc::peer_connection_factory::native::PeerConnectionFactoryExt;
use libwebrtc::peer_connection_factory::{
    IceServer, IceTransportsType, PeerConnectionFactory, RtcConfiguration,
};
use libwebrtc::session_description::{SdpType, SessionDescription};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::turn::{Output, Turn};

/// One factory per process: it owns WebRTC's threads and audio device.
pub fn factory() -> &'static PeerConnectionFactory {
    static F: OnceLock<(PeerConnectionFactory, Option<PeerConnection>)> = OnceLock::new();
    &F.get_or_init(|| {
        let f = PeerConnectionFactory::default();
        // WebRTC terminates its media engine, and with it the audio device,
        // when the last peer connection closes, and initialises it again with
        // the next. LiveKit's audio device proxy doesn't survive that on
        // Android (the second call crashes registering its audio callback), so
        // an idle connection, which never gathers or sends anything, keeps
        // the engine up for the life of the process.
        let mut cfg = RtcConfiguration::default();
        cfg.ice_transport_type = IceTransportsType::Relay;
        let keep = f.create_peer_connection(cfg).ok();
        (f, keep)
    })
    .0
}

/// Signalling between the two media sessions.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Signal {
    Offer(String),
    Answer(String),
    Candidate {
        mid: String,
        index: i32,
        sdp: String,
    },
}

impl Signal {
    /// `o` / `a` then the SDP; `c` then mid, index and candidate, one per
    /// line.
    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Offer(s) => format!("o{s}"),
            Self::Answer(s) => format!("a{s}"),
            Self::Candidate { mid, index, sdp } => format!("c{mid}\n{index}\n{sdp}"),
        }
        .into_bytes()
    }

    fn decode(b: &[u8]) -> Result<Self> {
        let s = std::str::from_utf8(b).context("signal text")?;
        let (tag, rest) = s
            .split_at_checked(1)
            .ok_or_else(|| anyhow!("empty signal"))?;
        Ok(match tag {
            "o" => Self::Offer(rest.to_owned()),
            "a" => Self::Answer(rest.to_owned()),
            "c" => {
                let mut it = rest.splitn(3, '\n');
                let (Some(mid), Some(index), Some(sdp)) = (it.next(), it.next(), it.next()) else {
                    bail!("candidate signal");
                };
                Self::Candidate {
                    mid: mid.to_owned(),
                    index: index.parse()?,
                    sdp: sdp.to_owned(),
                }
            }
            _ => bail!("unknown signal"),
        })
    }
}

/// Where the call's outgoing signalling and media go.
pub struct Wires {
    pub signal: Box<dyn Fn(Vec<u8>) + Send + Sync>,
    pub media: Box<dyn Fn(Vec<u8>) + Send + Sync>,
}

/// What we send as audio.
pub enum AudioIn {
    /// The platform's microphone (and its playout for what we receive),
    /// with WebRTC's echo cancellation, noise suppression and gain control.
    Device,
    /// Frames the application pushes (tests, files).
    Source(NativeAudioSource),
    None,
}

pub struct MediaSession {
    pc: PeerConnection,
    from_peer: mpsc::UnboundedSender<Vec<u8>>,
    /// Candidates that came before the remote description.
    early: Mutex<Vec<IceCandidate>>,
    signal: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
    /// We hold the platform audio device (see [`AudioIn::Device`]).
    device: bool,
    /// What we send, to mute.
    audio: Option<MediaStreamTrack>,
}

impl MediaSession {
    /// Starts the media for one call. The caller makes the offer; the
    /// callee waits for it. `on_track` gets the peer's tracks.
    pub async fn start(
        caller: bool,
        audio: AudioIn,
        wires: Wires,
        on_track: impl FnMut(TrackEvent) + Send + Sync + 'static,
        on_state: impl FnMut(PeerConnectionState) + Send + Sync + 'static,
    ) -> Result<Self> {
        let rand_hex = |n: usize| {
            threnody_core::random_bytes_vec(n)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        let (user, pass) = (rand_hex(8), rand_hex(16));
        let turn = Turn::new(&user, &pass, caller);
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        let port = socket.local_addr()?.port();
        let (from_peer, rx) = mpsc::unbounded_channel();
        tokio::spawn(serve(turn, socket, rx, wires.media));

        let mut cfg = RtcConfiguration::default();
        // Only our relay: no host or server-reflexive candidates, so
        // WebRTC never shows or uses this device's addresses.
        cfg.ice_transport_type = IceTransportsType::Relay;
        cfg.ice_servers = vec![IceServer {
            urls: vec![format!("turn:127.0.0.1:{port}?transport=udp")],
            username: user,
            password: pass,
        }];
        let pc = factory()
            .create_peer_connection(cfg)
            .map_err(|e| anyhow!("peer connection: {e:?}"))?;
        let signal: Arc<dyn Fn(Vec<u8>) + Send + Sync> = Arc::from(wires.signal);
        let s = signal.clone();
        pc.on_ice_candidate(Some(Box::new(move |c: IceCandidate| {
            s(Signal::Candidate {
                mid: c.sdp_mid(),
                index: c.sdp_mline_index(),
                sdp: c.candidate(),
            }
            .encode())
        })));
        pc.on_track(Some(Box::new(on_track)));
        pc.on_connection_state_change(Some(Box::new(on_state)));

        let mut session = Self {
            pc,
            from_peer,
            early: Mutex::new(Vec::new()),
            signal,
            device: false,
            audio: None,
        };
        if matches!(audio, AudioIn::Device) {
            // Microphone and speaker for as long as this call lasts
            // (released on drop, even if what follows fails).
            let f = factory();
            if !f.acquire_platform_adm() {
                bail!("no audio device");
            }
            session.device = true;
            f.set_adm_recording_enabled(true);
            f.set_adm_playout_enabled(true);
        }
        let track = match audio {
            AudioIn::Device => Some(factory().create_device_audio_track("audio")),
            AudioIn::Source(src) => Some(factory().create_audio_track("audio", src)),
            AudioIn::None => None,
        };
        if let Some(t) = track {
            let t = MediaStreamTrack::Audio(t);
            session
                .pc
                .add_track(t.clone(), &["call"])
                .map_err(|e| anyhow!("audio track: {e:?}"))?;
            session.audio = Some(t);
        }
        if caller {
            let offer = session
                .pc
                .create_offer(OfferOptions {
                    offer_to_receive_audio: true,
                    ..Default::default()
                })
                .await
                .map_err(|e| anyhow!("offer: {e:?}"))?;
            let sdp = offer.to_string();
            session
                .pc
                .set_local_description(offer)
                .await
                .map_err(|e| anyhow!("local offer: {e:?}"))?;
            (session.signal)(Signal::Offer(sdp).encode());
        }
        Ok(session)
    }

    /// Signalling from the peer's media session.
    pub async fn on_signal(&self, data: &[u8]) -> Result<()> {
        match Signal::decode(data)? {
            Signal::Offer(sdp) => {
                let offer = SessionDescription::parse(&sdp, SdpType::Offer)
                    .map_err(|e| anyhow!("offer: {e:?}"))?;
                self.pc
                    .set_remote_description(offer)
                    .await
                    .map_err(|e| anyhow!("remote offer: {e:?}"))?;
                self.add_early().await;
                let answer = self
                    .pc
                    .create_answer(Default::default())
                    .await
                    .map_err(|e| anyhow!("answer: {e:?}"))?;
                let sdp = answer.to_string();
                self.pc
                    .set_local_description(answer)
                    .await
                    .map_err(|e| anyhow!("local answer: {e:?}"))?;
                (self.signal)(Signal::Answer(sdp).encode());
            }
            Signal::Answer(sdp) => {
                let answer = SessionDescription::parse(&sdp, SdpType::Answer)
                    .map_err(|e| anyhow!("answer: {e:?}"))?;
                self.pc
                    .set_remote_description(answer)
                    .await
                    .map_err(|e| anyhow!("remote answer: {e:?}"))?;
                self.add_early().await;
            }
            Signal::Candidate { mid, index, sdp } => {
                let c = IceCandidate::parse(&mid, index, &sdp)
                    .map_err(|e| anyhow!("candidate: {e:?}"))?;
                if self.pc.current_remote_description().is_none() {
                    self.early.lock().expect("early lock").push(c);
                } else {
                    self.pc
                        .add_ice_candidate(c)
                        .await
                        .map_err(|e| anyhow!("candidate: {e:?}"))?;
                }
            }
        }
        Ok(())
    }

    async fn add_early(&self) {
        let early = std::mem::take(&mut *self.early.lock().expect("early lock"));
        for c in early {
            let _ = self.pc.add_ice_candidate(c).await;
        }
    }

    /// Media from the peer (opened call media).
    pub fn on_media(&self, payload: Vec<u8>) {
        let _ = self.from_peer.send(payload);
    }

    /// Stops (or resumes) sending our audio; the peer hears silence.
    pub fn set_muted(&self, muted: bool) {
        if let Some(t) = &self.audio {
            t.set_enabled(!muted);
        }
    }

    pub fn state(&self) -> PeerConnectionState {
        self.pc.connection_state()
    }

    pub fn close(&self) {
        self.pc.close();
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.pc.close();
        if self.device {
            factory().release_platform_adm();
        }
    }
}

/// Runs the TURN server until the session's sender is dropped.
async fn serve(
    mut turn: Turn,
    socket: UdpSocket,
    mut from_peer: mpsc::UnboundedReceiver<Vec<u8>>,
    to_peer: Box<dyn Fn(Vec<u8>) + Send + Sync>,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = r else { return };
                match turn.handle(from, &buf[..n]) {
                    Some(Output::ToClient(to, b)) => {
                        let _ = socket.send_to(&b, to).await;
                    }
                    Some(Output::ToPeer(p)) => to_peer(p),
                    None => {}
                }
            }
            p = from_peer.recv() => {
                let Some(p) = p else { return };
                if let Some((to, b)) = turn.from_peer(&p) {
                    let _ = socket.send_to(&b, to).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_round_trip() {
        for s in [
            Signal::Offer("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\n".into()),
            Signal::Answer("v=0".into()),
            Signal::Candidate {
                mid: "0".into(),
                index: 0,
                sdp: "candidate:1 1 udp 2 192.0.2.1 9 typ relay".into(),
            },
        ] {
            assert_eq!(Signal::decode(&s.encode()).unwrap(), s);
        }
        assert!(Signal::decode(b"").is_err() && Signal::decode(b"x").is_err());
        assert!(Signal::decode(b"c0\nnot a number\ncand").is_err());
    }
}
