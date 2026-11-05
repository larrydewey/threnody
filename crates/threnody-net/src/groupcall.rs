//! Group calls (Appendix Q, *Group calls*): the roster, a leg of sealed
//! media between every two participants, and the hub that passes legs on
//! for pairs without a session of their own.
//!
//! The group layer above owns the signalling: it reads and writes
//! [`threnody_core::call::GroupCallMsg`] in group messages, takes the
//! call's base secret from the group's MLS exporter, and tells the node
//! what happened ([`Node::group_call_rang`], [`Node::group_call_joined`],
//! …). The media layer gets each leg's payloads from
//! [`Node::group_leg_media`] and sends with [`Node::send_leg_media`]; its
//! leg signalling (session descriptions) also goes through the group
//! layer, so the hub never sees it and can't stand in the middle.
//!
//! The hub only forwards packets between two current participants, sealed
//! with a leg's keys; DTLS-SRTP inside each leg keeps the media from it.

use std::collections::HashMap;
use std::time::Duration;

use threnody_core::call::{self, MAX_GROUP_CALL, MediaKeys};
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use crate::error::{NetError, Result};
use crate::node::{Event, Node, lock};

/// How long a group call rings here before it stops (the call goes on).
pub const GROUP_RING_TIMEOUT: Duration = Duration::from_secs(60);
/// A leg with no media from its peer this long counts as down: the peer
/// is dropped from our view of the call (it may come back by joining).
pub const LEG_TIMEOUT: Duration = Duration::from_secs(30);
/// Received payloads per leg waiting for the media layer.
const MEDIA_QUEUE: usize = 512;
const WATCH_EVERY: Duration = Duration::from_secs(1);

/// A group call, as the user interface sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupCallInfo {
    pub call: u64,
    pub group: [u8; 16],
    /// The device that relays for pairs without a session.
    pub hub: PublicIdentity,
    /// Devices in the call other than us, with whether each sends video.
    pub participants: Vec<(PublicIdentity, bool)>,
    /// We're in it (else it rings here).
    pub joined: bool,
    pub video: bool,
    pub started_ms: u64,
}

struct Leg {
    keys: MediaKeys,
    media_tx: mpsc::Sender<(u8, Vec<u8>)>,
    media_rx: Option<mpsc::Receiver<(u8, Vec<u8>)>>,
    last_rx_ms: u64,
}

struct GroupCall {
    info: GroupCallInfo,
    base: Zeroizing<[u8; 32]>,
    legs: HashMap<PublicIdentity, Leg>,
    /// When it started ringing here.
    rang_ms: u64,
}

#[derive(Default)]
pub(crate) struct GroupCallState {
    current: Option<GroupCall>,
}

impl GroupCall {
    fn is_participant(&self, d: &PublicIdentity) -> bool {
        self.info.participants.iter().any(|(p, _)| p == d)
    }
}

impl Node {
    /// The group call we're in or that rings here, if any.
    pub fn group_call(&self) -> Option<GroupCallInfo> {
        lock(&self.shared.group_calls)
            .current
            .as_ref()
            .map(|c| c.info.clone())
    }

    /// Whether any call, one-to-one or group, rings or runs here.
    pub(crate) fn in_group_call(&self) -> bool {
        lock(&self.shared.group_calls)
            .current
            .as_ref()
            .is_some_and(|c| c.info.joined)
    }

    /// We start group call `call` in `group`, relaying for everyone, with
    /// `base` from the group's exporter. The group layer then sends
    /// `GroupCallMsg::Start`.
    pub fn start_group_call(
        &self,
        group: [u8; 16],
        call: u64,
        base: [u8; 32],
        video: bool,
    ) -> Result<()> {
        if self.call().is_some() {
            return Err(NetError::NotAllowed("already in a call".into()));
        }
        let mut gc = lock(&self.shared.group_calls);
        if gc.current.as_ref().is_some_and(|c| c.info.joined) {
            return Err(NetError::NotAllowed("already in a call".into()));
        }
        gc.current = Some(GroupCall {
            info: GroupCallInfo {
                call,
                group,
                hub: self.identity(),
                participants: Vec::new(),
                joined: true,
                video,
                started_ms: now_ms(),
            },
            base: Zeroizing::new(base),
            legs: HashMap::new(),
            rang_ms: 0,
        });
        drop(gc);
        self.watch_group_call(call);
        Ok(())
    }

    /// `from` started (or, `joined`, is in) group call `call` in `group`:
    /// it rings here unless we're in a call. Returns whether it rings.
    pub fn group_call_rang(
        &self,
        group: [u8; 16],
        call: u64,
        from: PublicIdentity,
        hub: PublicIdentity,
        base: [u8; 32],
        video: bool,
    ) -> bool {
        if self.call().is_some() {
            return false;
        }
        let mut gc = lock(&self.shared.group_calls);
        match &gc.current {
            Some(c) if c.info.call == call => return false,
            Some(c) if c.info.joined => return false,
            _ => {}
        }
        gc.current = Some(GroupCall {
            info: GroupCallInfo {
                call,
                group,
                hub,
                participants: vec![(from, video)],
                joined: false,
                video: false,
                started_ms: now_ms(),
            },
            base: Zeroizing::new(base),
            legs: HashMap::new(),
            rang_ms: now_ms(),
        });
        drop(gc);
        self.emit(Event::GroupCallRinging { group, call, from });
        self.watch_group_call(call);
        true
    }

    /// We join the group call that rings here; the group layer then sends
    /// `GroupCallMsg::Join`, and the media layer opens a leg to every
    /// participant ([`Node::group_leg_media`]).
    pub fn join_group_call(&self, call: u64, video: bool) -> Result<()> {
        if self.call().is_some() {
            return Err(NetError::NotAllowed("already in a call".into()));
        }
        let me = self.identity();
        let mut gc = lock(&self.shared.group_calls);
        let c = gc
            .current
            .as_mut()
            .filter(|c| c.info.call == call && !c.info.joined)
            .ok_or_else(|| NetError::NotAllowed("no such call ringing".into()))?;
        if c.info.participants.len() + 1 > MAX_GROUP_CALL {
            return Err(NetError::NotAllowed("the call is full".into()));
        }
        c.info.joined = true;
        c.info.video = video;
        let peers: Vec<_> = c.info.participants.iter().map(|(p, _)| *p).collect();
        for p in peers {
            add_leg(c, &me, p);
        }
        Ok(())
    }

    /// `device` joined call `call` (or is in it, as the starter is).
    pub fn group_call_joined(&self, call: u64, device: PublicIdentity, video: bool) {
        let me = self.identity();
        if device == me {
            return;
        }
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return;
        };
        if c.is_participant(&device) {
            return;
        }
        if c.info.participants.len() + usize::from(c.info.joined) >= MAX_GROUP_CALL {
            return;
        }
        c.info.participants.push((device, video));
        let joined = c.info.joined;
        if joined {
            add_leg(c, &me, device);
        }
        let group = c.info.group;
        drop(gc);
        self.emit(Event::GroupCallMember {
            group,
            call,
            device,
            joined: true,
        });
    }

    /// `device` left call `call`. Returns the new hub when the hub left
    /// and we take its place (the group layer then sends
    /// `GroupCallMsg::Hub`).
    pub fn group_call_left(&self, call: u64, device: PublicIdentity) -> Option<PublicIdentity> {
        let me = self.identity();
        let mut gc = lock(&self.shared.group_calls);
        let c = gc.current.as_mut().filter(|c| c.info.call == call)?;
        if !c.is_participant(&device) {
            return None;
        }
        c.info.participants.retain(|(p, _)| *p != device);
        c.legs.remove(&device);
        let group = c.info.group;
        // Nobody left to ring with: it stops ringing here.
        let over = !c.info.joined && c.info.participants.is_empty();
        let mut new_hub = None;
        if c.info.hub == device {
            // The remaining participant with the smallest key takes over;
            // everyone picks the same one.
            let next = c
                .info
                .participants
                .iter()
                .map(|(p, _)| *p)
                .chain(c.info.joined.then_some(me))
                .min_by_key(|p| *p.as_bytes());
            if let Some(h) = next {
                c.info.hub = h;
                if h == me {
                    new_hub = Some(h);
                }
            }
        }
        if over {
            gc.current = None;
        }
        drop(gc);
        self.emit(Event::GroupCallMember {
            group,
            call,
            device,
            joined: false,
        });
        if over {
            self.emit(Event::GroupCallEnded { group, call });
        }
        new_hub
    }

    /// The group layer heard `GroupCallMsg::Hub`.
    pub fn set_group_call_hub(&self, call: u64, hub: PublicIdentity) {
        if let Some(c) = lock(&self.shared.group_calls)
            .current
            .as_mut()
            .filter(|c| c.info.call == call)
        {
            c.info.hub = hub;
        }
    }

    /// `device` turned its video on or off.
    pub fn group_call_video(&self, call: u64, device: PublicIdentity, video: bool) {
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return;
        };
        let Some(p) = c.info.participants.iter_mut().find(|(p, _)| *p == device) else {
            return;
        };
        p.1 = video;
        let group = c.info.group;
        drop(gc);
        self.emit(Event::GroupCallVideo {
            group,
            call,
            device,
            video,
        });
    }

    /// We turn our video on or off (the group layer sends the update).
    pub fn set_group_call_video_flag(&self, call: u64, on: bool) {
        if let Some(c) = lock(&self.shared.group_calls)
            .current
            .as_mut()
            .filter(|c| c.info.call == call)
        {
            c.info.video = on;
        }
    }

    /// We leave call `call` (or stop it ringing here); the group layer
    /// sends `GroupCallMsg::Leave` when we were in it.
    pub fn leave_group_call(&self, call: u64) {
        let ended = {
            let mut gc = lock(&self.shared.group_calls);
            match gc.current.as_ref() {
                Some(c) if c.info.call == call => gc.current.take(),
                _ => None,
            }
        };
        if let Some(c) = ended {
            self.emit(Event::GroupCallEnded {
                group: c.info.group,
                call,
            });
        }
    }

    /// Takes the stream of received media on our leg with `peer` in call
    /// `call`: `(stream, payload)`. Only the first caller gets it.
    pub fn group_leg_media(
        &self,
        call: u64,
        peer: &PublicIdentity,
    ) -> Option<mpsc::Receiver<(u8, Vec<u8>)>> {
        lock(&self.shared.group_calls)
            .current
            .as_mut()
            .filter(|c| c.info.call == call && c.info.joined)?
            .legs
            .get_mut(peer)?
            .media_rx
            .take()
    }

    /// Seals and sends one media payload on our leg with `peer`: over our
    /// own session with it, else through the hub. False when it couldn't
    /// go (dropped, not queued).
    pub fn send_leg_media(
        &self,
        call: u64,
        peer: &PublicIdentity,
        stream: u8,
        payload: &[u8],
    ) -> Result<bool> {
        let (packet, hub) = {
            let mut gc = lock(&self.shared.group_calls);
            let c = gc
                .current
                .as_mut()
                .filter(|c| c.info.call == call && c.info.joined)
                .ok_or_else(|| NetError::NotAllowed("no such call".into()))?;
            let hub = c.info.hub;
            let leg = c
                .legs
                .get_mut(peer)
                .ok_or_else(|| NetError::NotAllowed("not in the call".into()))?;
            (leg.keys.seal(stream, payload)?, hub)
        };
        if self.connected(peer) {
            return Ok(self.send_packet(peer, packet));
        }
        if hub != self.identity() && hub != *peer && self.connected(&hub) {
            let wrapped = call::wrap_relayed(peer.as_bytes(), &packet);
            return Ok(self.send_packet(&hub, wrapped));
        }
        Ok(false)
    }

    /// A packet to `to`: as a datagram when the link has them and it
    /// fits, else in the session stream.
    fn send_packet(&self, to: &PublicIdentity, packet: Vec<u8>) -> bool {
        if let Some(conn) = self.datagrams_to(to)
            && conn.max_datagram_size().is_some_and(|m| packet.len() <= m)
        {
            return conn.send_datagram(packet.into()).is_ok();
        }
        self.send(to, AppMessage::Media(packet)).is_ok()
    }

    /// A media packet from `from` that may be a group call's: relayed
    /// (we pass it on as the hub, or the hub passes it to us) or one of
    /// our legs. Returns false when it isn't, for the one-to-one call.
    pub(crate) fn on_group_media(&self, from: PublicIdentity, packet: &[u8]) -> bool {
        let me = self.identity();
        if let Some((device, inner)) = call::unwrap_relayed(packet) {
            let other = PublicIdentity::from_bytes(&device);
            let mut gc = lock(&self.shared.group_calls);
            let Some(c) = gc.current.as_mut().filter(|c| c.info.joined) else {
                return true;
            };
            let Ok(other) = other else { return true };
            if c.info.hub == me {
                // Pass it on, only between two participants, and only a
                // packet of the leg between them.
                if c.is_participant(&from)
                    && c.is_participant(&other)
                    && call::call_of(inner)
                        == Some(call::leg_id(c.info.call, from.as_bytes(), other.as_bytes()))
                {
                    drop(gc);
                    self.send_packet(&other, call::wrap_relayed(from.as_bytes(), inner));
                }
                return true;
            }
            if from == c.info.hub {
                open_leg(c, &other, inner);
            }
            return true;
        }
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.joined) else {
            return false;
        };
        let Some(leg) = c.legs.get(&from) else {
            return false;
        };
        if call::call_of(packet) != Some(leg.keys.call()) {
            return false;
        }
        open_leg(c, &from, packet);
        true
    }

    /// Stops a group call ringing after a minute, and drops participants
    /// whose legs went silent.
    fn watch_group_call(&self, call: u64) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_EVERY).await;
                let now = now_ms();
                let (ring_over, silent) = {
                    let gc = lock(&node.shared.group_calls);
                    let Some(c) = gc.current.as_ref().filter(|c| c.info.call == call) else {
                        return;
                    };
                    let ring_over = !c.info.joined
                        && now.saturating_sub(c.rang_ms) > GROUP_RING_TIMEOUT.as_millis() as u64;
                    let silent: Vec<_> = c
                        .legs
                        .iter()
                        .filter(|(_, l)| {
                            now.saturating_sub(l.last_rx_ms) > LEG_TIMEOUT.as_millis() as u64
                        })
                        .map(|(p, _)| *p)
                        .collect();
                    (ring_over, silent)
                };
                if ring_over {
                    node.leave_group_call(call);
                    return;
                }
                for p in silent {
                    node.group_call_left(call, p);
                }
            }
        });
    }
}

fn add_leg(c: &mut GroupCall, me: &PublicIdentity, peer: PublicIdentity) {
    let keys = MediaKeys::for_leg(&c.base, c.info.call, me.as_bytes(), peer.as_bytes());
    let (tx, rx) = mpsc::channel(MEDIA_QUEUE);
    c.legs.insert(
        peer,
        Leg {
            keys,
            media_tx: tx,
            media_rx: Some(rx),
            last_rx_ms: now_ms(),
        },
    );
}

fn open_leg(c: &mut GroupCall, peer: &PublicIdentity, packet: &[u8]) {
    let Some(leg) = c.legs.get_mut(peer) else {
        return;
    };
    if let Ok(m) = leg.keys.open(packet) {
        leg.last_rx_ms = now_ms();
        let _ = leg.media_tx.try_send(m);
    }
}
