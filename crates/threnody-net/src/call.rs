//! Voice and video calls (Appendix Q): the call state machine and moving
//! sealed media between the peers.
//!
//! One call at a time, with one device of the peer: an offer while we're
//! in a call gets `Busy`. Offers from people we haven't accepted (message
//! requests) never ring; they are declined silently, as a request's
//! messages wait silently. Media rides QUIC datagrams when the session is
//! over QUIC, and `AppMessage::Media` in the session stream otherwise
//! (TCP, Bluetooth, relays). Either way it is sealed with the call's own
//! keys ([`MediaKeys`]), so a call carries on through a new session when
//! the old one is replaced.
//!
//! The media layer above (codecs, jitter buffer, echo cancellation) gets
//! decrypted payloads from [`Node::call_media`] and sends with
//! [`Node::send_media`]; it chooses the stream ids.

use std::time::Duration;

use threnody_core::call::{self, CallMsg, HangupReason, MediaKeys};
use threnody_core::crypto::random_bytes;
use threnody_core::message::FEATURE_CALLS;
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// How long a call may ring before it counts as unanswered.
pub const RING_TIMEOUT: Duration = Duration::from_secs(60);
/// An active call with no media from the peer for this long has failed.
pub const MEDIA_TIMEOUT: Duration = Duration::from_secs(30);
/// Received media payloads waiting for the media layer; more are dropped
/// (late media is useless).
const MEDIA_QUEUE: usize = 512;
const WATCH_EVERY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// We offered; the peer hasn't said it's ringing yet.
    Calling,
    /// We offered and the peer is ringing.
    Ringing,
    /// The peer offered; we haven't answered.
    Incoming,
    Active,
}

/// The current call, as the user interface sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallInfo {
    pub call: u64,
    pub peer: PublicIdentity,
    pub outgoing: bool,
    pub phase: Phase,
    /// Whether we send video, and whether the peer says it does.
    pub video: bool,
    pub peer_video: bool,
    pub started_ms: u64,
    /// When it was answered (0 = not yet).
    pub answered_ms: u64,
}

struct Call {
    info: CallInfo,
    /// The secret we contributed (offer or answer), and the offer's when
    /// we're the callee.
    secret: Zeroizing<[u8; 32]>,
    offer_secret: Option<Zeroizing<[u8; 32]>>,
    keys: Option<MediaKeys>,
    last_rx_ms: u64,
    media_tx: Option<mpsc::Sender<(u8, Vec<u8>)>>,
    media_rx: Option<mpsc::Receiver<(u8, Vec<u8>)>>,
}

#[derive(Default)]
pub(crate) struct CallState {
    current: Option<Call>,
}

impl Node {
    /// The current call, if any.
    pub fn call(&self) -> Option<CallInfo> {
        lock(&self.shared.calls)
            .current
            .as_ref()
            .map(|c| c.info.clone())
    }

    /// Calls `peer`, which must be connected and support calls. Returns
    /// the call id; [`Event::CallRinging`], [`Event::CallStarted`] or
    /// [`Event::CallEnded`] follow.
    pub fn start_call(&self, peer: &PublicIdentity, video: bool) -> Result<u64> {
        if !self.sessions().iter().any(|s| s.peer == *peer) {
            return Err(NetError::NoRoute("calls need a connection to them".into()));
        }
        if !self.supports(peer, FEATURE_CALLS) {
            return Err(NetError::NoRoute("their app can't take calls".into()));
        }
        let mut calls = lock(&self.shared.calls);
        if calls.current.is_some() {
            return Err(NetError::NoRoute("already in a call".into()));
        }
        let id = loop {
            let id = u64::from_be_bytes(random_bytes());
            if id != 0 {
                break id;
            }
        };
        let secret = Zeroizing::new(random_bytes::<32>());
        let offer = CallMsg::Offer {
            call: id,
            secret: *secret,
            video,
        };
        self.send(peer, AppMessage::Call(offer.encode()?))?;
        calls.current = Some(Call {
            info: CallInfo {
                call: id,
                peer: *peer,
                outgoing: true,
                phase: Phase::Calling,
                video,
                peer_video: false,
                started_ms: now_ms(),
                answered_ms: 0,
            },
            secret,
            offer_secret: None,
            keys: None,
            last_rx_ms: 0,
            media_tx: None,
            media_rx: None,
        });
        drop(calls);
        self.watch_call(id);
        Ok(id)
    }

    /// Answers the incoming call `id`, sending video or not.
    pub fn answer_call(&self, id: u64, video: bool) -> Result<()> {
        let mut calls = lock(&self.shared.calls);
        let c = calls
            .current
            .as_mut()
            .filter(|c| c.info.call == id && c.info.phase == Phase::Incoming)
            .ok_or_else(|| NetError::NoRoute("no such call ringing".into()))?;
        let offer = c.offer_secret.take().ok_or(NetError::Closed)?;
        let answer = CallMsg::Answer {
            call: id,
            secret: *c.secret,
            video,
        };
        self.send(&c.info.peer, AppMessage::Call(answer.encode()?))?;
        c.keys = Some(MediaKeys::new(id, &offer, &c.secret, false));
        c.info.video = video;
        let (tx, rx) = mpsc::channel(MEDIA_QUEUE);
        (c.media_tx, c.media_rx) = (Some(tx), Some(rx));
        c.info.phase = Phase::Active;
        c.info.answered_ms = now_ms();
        c.last_rx_ms = now_ms();
        let (peer, peer_video) = (c.info.peer, c.info.peer_video);
        drop(calls);
        self.upgrade_for_media(peer);
        self.emit(Event::CallStarted {
            peer,
            call: id,
            peer_video,
        });
        Ok(())
    }

    /// Ends call `id`: hangs up, declines it if it's ringing here, or
    /// cancels it if it's ringing there.
    pub fn hangup_call(&self, id: u64) {
        let reason = match self.call() {
            Some(c) if c.call == id && c.phase == Phase::Incoming => HangupReason::Declined,
            _ => HangupReason::Ended,
        };
        self.end_call(id, reason, true);
    }

    /// Ends call `id` because its media failed (the media layer's verdict).
    pub fn fail_call(&self, id: u64) {
        self.end_call(id, HangupReason::Failed, true);
    }

    /// Tells the peer we turned our video on or off.
    pub fn set_call_video(&self, id: u64, on: bool) -> Result<()> {
        let mut calls = lock(&self.shared.calls);
        let c = calls
            .current
            .as_mut()
            .filter(|c| c.info.call == id)
            .ok_or_else(|| NetError::NoRoute("no such call".into()))?;
        c.info.video = on;
        let peer = c.info.peer;
        drop(calls);
        let m = CallMsg::Update {
            call: id,
            video: on,
        };
        self.send(&peer, AppMessage::Call(m.encode()?))
    }

    /// Sends the media layer's signalling (at most
    /// [`threnody_core::call::MAX_SIGNAL`] bytes) to the peer of call `id`;
    /// it arrives as [`Event::CallSignal`]. Tracked: it survives the session
    /// being replaced.
    pub fn send_call_signal(&self, id: u64, data: Vec<u8>) -> Result<()> {
        let peer = self
            .call()
            .filter(|c| c.call == id)
            .map(|c| c.peer)
            .ok_or_else(|| NetError::NoRoute("no such call".into()))?;
        let m = CallMsg::Signal { call: id, data }.encode()?;
        // Validate as the receiver will.
        CallMsg::decode(&m)?;
        self.send_tracked(&peer, AppMessage::Call(m))
    }

    /// Takes the stream of received media for the active call `id`:
    /// `(stream, payload)` in arrival order (not necessarily send order).
    /// Only the first caller gets it.
    pub fn call_media(&self, id: u64) -> Option<mpsc::Receiver<(u8, Vec<u8>)>> {
        lock(&self.shared.calls)
            .current
            .as_mut()
            .filter(|c| c.info.call == id)?
            .media_rx
            .take()
    }

    /// Seals and sends one media payload (at most
    /// [`threnody_core::call::MAX_PAYLOAD`] bytes) on `stream`. Dropped
    /// rather than queued when the link is congested: returns false then.
    pub fn send_media(&self, id: u64, stream: u8, payload: &[u8]) -> Result<bool> {
        let (peer, packet) = {
            let mut calls = lock(&self.shared.calls);
            let c = calls
                .current
                .as_mut()
                .filter(|c| c.info.call == id)
                .ok_or_else(|| NetError::NoRoute("no such call".into()))?;
            let keys = c.keys.as_mut().ok_or(NetError::Closed)?;
            (c.info.peer, keys.seal(stream, payload)?)
        };
        if let Some(conn) = self.datagrams_to(&peer)
            && conn.max_datagram_size().is_some_and(|m| packet.len() <= m)
        {
            return Ok(conn.send_datagram(packet.into()).is_ok());
        }
        // No datagrams on this link: the stream keeps order and delivers
        // everything, late or not.
        self.send(&peer, AppMessage::Media(packet)).map(|()| true)
    }

    /// Whether media to `peer` goes as datagrams (else in the session
    /// stream, where late packets still arrive, late).
    pub fn media_datagrams(&self, peer: &PublicIdentity) -> bool {
        self.datagrams_to(peer)
            .is_some_and(|c| c.max_datagram_size().is_some())
    }

    /// A call over a TCP session we dialed: also dial the peer's QUIC
    /// endpoint (the same address; nodes listen on both), so media can go
    /// as datagrams. The new session replaces the TCP one; the call, whose
    /// keys aren't the session's, carries on. Only the dialer does this, so
    /// the two sides never race.
    fn upgrade_for_media(&self, peer: PublicIdentity) {
        let Some(s) = self
            .sessions()
            .into_iter()
            .find(|s| s.peer == peer && s.via.is_none() && s.outbound && s.transport == "tcp")
        else {
            return;
        };
        if self.media_datagrams(&peer) || self.quic().is_none() {
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let _ = node.connect_quic(s.addr, Some(peer.fingerprint())).await;
        });
    }

    pub(crate) fn on_call(&self, peer: PublicIdentity, payload: &[u8]) {
        let Ok(m) = CallMsg::decode(payload) else {
            return;
        };
        let id = m.call();
        match m {
            CallMsg::Offer { secret, video, .. } => self.on_offer(peer, id, secret, video),
            CallMsg::Ringing { .. } => {
                let mut calls = lock(&self.shared.calls);
                if let Some(c) = calls.current.as_mut().filter(|c| {
                    c.info.call == id && c.info.peer == peer && c.info.phase == Phase::Calling
                }) {
                    c.info.phase = Phase::Ringing;
                    drop(calls);
                    self.emit(Event::CallRinging { peer, call: id });
                }
            }
            CallMsg::Answer { secret, video, .. } => {
                let mut calls = lock(&self.shared.calls);
                let Some(c) = calls.current.as_mut().filter(|c| {
                    c.info.call == id
                        && c.info.peer == peer
                        && matches!(c.info.phase, Phase::Calling | Phase::Ringing)
                }) else {
                    return;
                };
                c.keys = Some(MediaKeys::new(id, &c.secret, &secret, true));
                let (tx, rx) = mpsc::channel(MEDIA_QUEUE);
                (c.media_tx, c.media_rx) = (Some(tx), Some(rx));
                c.info.phase = Phase::Active;
                c.info.peer_video = video;
                c.info.answered_ms = now_ms();
                c.last_rx_ms = now_ms();
                drop(calls);
                self.upgrade_for_media(peer);
                self.emit(Event::CallStarted {
                    peer,
                    call: id,
                    peer_video: video,
                });
            }
            CallMsg::Hangup { reason, .. } => {
                if self.call().is_some_and(|c| c.call == id && c.peer == peer) {
                    self.end_call(id, reason, false);
                }
            }
            CallMsg::Signal { data, .. } => {
                if self
                    .call()
                    .is_some_and(|c| c.call == id && c.peer == peer && c.phase == Phase::Active)
                {
                    self.emit(Event::CallSignal {
                        peer,
                        call: id,
                        data,
                    });
                }
            }
            CallMsg::Update { video, .. } => {
                let mut calls = lock(&self.shared.calls);
                if let Some(c) = calls
                    .current
                    .as_mut()
                    .filter(|c| c.info.call == id && c.info.peer == peer)
                {
                    c.info.peer_video = video;
                    drop(calls);
                    self.emit(Event::CallVideo {
                        peer,
                        call: id,
                        video,
                    });
                }
            }
        }
    }

    fn on_offer(&self, peer: PublicIdentity, id: u64, offer: [u8; 32], video: bool) {
        let reply = |reason| {
            if let Ok(m) = (CallMsg::Hangup { call: id, reason }).encode() {
                let _ = self.send(&peer, AppMessage::Call(m));
            }
        };
        if !self.is_accepted(&peer) {
            // A message request: don't ring, and don't say whether we're here.
            return;
        }
        let mut calls = lock(&self.shared.calls);
        match &calls.current {
            // The same offer again (resent over a new session): ring on.
            Some(c) if c.info.call == id => return,
            Some(_) => {
                drop(calls);
                return reply(HangupReason::Busy);
            }
            None => {}
        }
        calls.current = Some(Call {
            info: CallInfo {
                call: id,
                peer,
                outgoing: false,
                phase: Phase::Incoming,
                video: false,
                peer_video: video,
                started_ms: now_ms(),
                answered_ms: 0,
            },
            secret: Zeroizing::new(random_bytes::<32>()),
            offer_secret: Some(Zeroizing::new(offer)),
            keys: None,
            last_rx_ms: 0,
            media_tx: None,
            media_rx: None,
        });
        drop(calls);
        if let Ok(m) = (CallMsg::Ringing { call: id }).encode() {
            let _ = self.send(&peer, AppMessage::Call(m));
        }
        self.emit(Event::CallIncoming {
            peer,
            call: id,
            video,
        });
        self.watch_call(id);
    }

    /// A sealed media packet from `peer`, by datagram or in the stream.
    pub(crate) fn on_media(&self, peer: PublicIdentity, packet: &[u8]) {
        let mut calls = lock(&self.shared.calls);
        let Some(c) = calls
            .current
            .as_mut()
            .filter(|c| c.info.peer == peer && call::call_of(packet) == Some(c.info.call))
        else {
            return;
        };
        let (Some(keys), Some(tx)) = (c.keys.as_mut(), c.media_tx.as_ref()) else {
            return;
        };
        if let Ok(m) = keys.open(packet) {
            c.last_rx_ms = now_ms();
            // Full: the media layer is behind; dropping is what it'd do.
            let _ = tx.try_send(m);
        }
    }

    /// Ends call `id` with `reason`, telling the peer when `tell`.
    fn end_call(&self, id: u64, reason: HangupReason, tell: bool) {
        let ended = {
            let mut calls = lock(&self.shared.calls);
            if calls.current.as_ref().is_some_and(|c| c.info.call == id) {
                calls.current.take()
            } else {
                None
            }
        };
        let Some(c) = ended else { return };
        // Tracked: if the session drops now, the next one still says so.
        if tell && let Ok(m) = (CallMsg::Hangup { call: id, reason }).encode() {
            let _ = self.send_tracked(&c.info.peer, AppMessage::Call(m));
        }
        self.emit(Event::CallEnded {
            peer: c.info.peer,
            call: id,
            reason,
            by_us: tell,
        });
    }

    /// Ends call `id` when it rings unanswered too long, or when its media
    /// stops arriving.
    fn watch_call(&self, id: u64) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_EVERY).await;
                let (phase, started, last_rx) = {
                    let calls = lock(&node.shared.calls);
                    match calls.current.as_ref().filter(|c| c.info.call == id) {
                        Some(c) => (c.info.phase, c.info.started_ms, c.last_rx_ms),
                        None => return,
                    }
                };
                let now = now_ms();
                let reason = match phase {
                    Phase::Active
                        if now.saturating_sub(last_rx) > MEDIA_TIMEOUT.as_millis() as u64 =>
                    {
                        HangupReason::Failed
                    }
                    Phase::Calling | Phase::Ringing | Phase::Incoming
                        if now.saturating_sub(started) > RING_TIMEOUT.as_millis() as u64 =>
                    {
                        HangupReason::Unanswered
                    }
                    _ => continue,
                };
                // The callee stops ringing quietly; the caller tells it.
                let tell = phase != Phase::Incoming;
                node.end_call(id, reason, tell);
                return;
            }
        });
    }
}
