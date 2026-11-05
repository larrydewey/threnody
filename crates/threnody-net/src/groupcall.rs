//! Group calls (Appendix Q, *Group calls*): the roster, a leg of sealed
//! media between every two participants, and relaying for pairs without a
//! session of their own.
//!
//! The group layer above owns the signalling: it reads and writes
//! [`threnody_core::call::GroupCallMsg`] in group messages, carries the
//! call's secret, and tells the node what happened
//! ([`Node::group_call_rang`], [`Node::group_call_joined`], …). The media
//! layer gets each leg's payloads from [`Node::group_leg_media`] and sends
//! with [`Node::send_leg_media`]; its leg signalling (session
//! descriptions) goes over the pair's own session or in a group message,
//! never through a relay, so no relay can stand in the middle.
//!
//! Each participant says which other participants it has sessions with
//! (its links). A pair without a session sends its leg through any
//! participant linked to both, which passes on only packets of the leg
//! between two current participants, sealed with that leg's keys;
//! DTLS-SRTP inside each leg keeps the media from it. The best-linked
//! participant is the call's hub, for the record.

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
    /// The participant linked to the most others (it relays the most).
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
    /// Who each participant says it has sessions with.
    links: HashMap<PublicIdentity, Vec<PublicIdentity>>,
    /// Our own links as we last told the others.
    told: Vec<PublicIdentity>,
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

    /// Whether `a` and `b` have a session, as either says.
    fn linked(&self, a: &PublicIdentity, b: &PublicIdentity) -> bool {
        self.links.get(a).is_some_and(|l| l.contains(b))
            || self.links.get(b).is_some_and(|l| l.contains(a))
    }

    /// The participant linked to the most others (ties: the smaller key),
    /// with `mine` being our own links.
    fn hub(&self, me: &PublicIdentity, mine: &[PublicIdentity]) -> PublicIdentity {
        let all: Vec<PublicIdentity> = self
            .info
            .participants
            .iter()
            .map(|(p, _)| *p)
            .chain(self.info.joined.then_some(*me))
            .collect();
        let score = |d: &PublicIdentity| {
            all.iter()
                .filter(|o| *o != d)
                .filter(|o| {
                    if d == me {
                        mine.contains(o)
                    } else if *o == me {
                        mine.contains(d) || self.linked(d, o)
                    } else {
                        self.linked(d, o)
                    }
                })
                .count()
        };
        all.iter()
            .copied()
            .max_by(|a, b| {
                score(a)
                    .cmp(&score(b))
                    .then_with(|| b.as_bytes().cmp(a.as_bytes()))
            })
            .unwrap_or(*me)
    }
}

impl Node {
    /// The group call we're in or that rings here, if any.
    pub fn group_call(&self) -> Option<GroupCallInfo> {
        let me = self.identity();
        let gc = lock(&self.shared.group_calls);
        let c = gc.current.as_ref()?;
        let mine = self.links_of(c);
        let mut info = c.info.clone();
        info.hub = c.hub(&me, &mine);
        Some(info)
    }

    /// The participants of `c` we have a session with.
    fn links_of(&self, c: &GroupCall) -> Vec<PublicIdentity> {
        c.info
            .participants
            .iter()
            .map(|(p, _)| *p)
            .filter(|p| self.connected(p))
            .collect()
    }

    /// Our links in call `call` (the participants we have a session
    /// with), for the group layer to send with `Join` or `Update`. Noted
    /// as told when they go to `everyone` (an answer to one newcomer
    /// leaves the others still to tell).
    pub fn group_call_links(&self, call: u64, everyone: bool) -> Vec<PublicIdentity> {
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return Vec::new();
        };
        let mine = self.links_of(c);
        if everyone {
            c.told = mine.clone();
        }
        mine
    }

    /// Whether any call, one-to-one or group, rings or runs here.
    pub(crate) fn in_group_call(&self) -> bool {
        lock(&self.shared.group_calls)
            .current
            .as_ref()
            .is_some_and(|c| c.info.joined)
    }

    /// We start group call `call` in `group`, keyed by `base` (the call's
    /// secret). The group layer then sends `GroupCallMsg::Start`.
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
            links: HashMap::new(),
            told: Vec::new(),
            rang_ms: 0,
        });
        drop(gc);
        self.watch_group_call(call);
        Ok(())
    }

    /// `from` started group call `call` in `group`, keyed by `base`: it
    /// rings here unless we're in a call. Returns whether it rings.
    pub fn group_call_rang(
        &self,
        group: [u8; 16],
        call: u64,
        from: PublicIdentity,
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
                hub: from,
                participants: vec![(from, video)],
                joined: false,
                video: false,
                started_ms: now_ms(),
            },
            base: Zeroizing::new(base),
            legs: HashMap::new(),
            links: HashMap::new(),
            told: Vec::new(),
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
        for p in &peers {
            add_leg(c, &me, *p);
        }
        drop(gc);
        // Media wants datagrams: legs over TCP sessions we dialed move to
        // QUIC, as a one-to-one call's do.
        for p in peers {
            self.upgrade_for_media(p);
        }
        Ok(())
    }

    /// `device` is in call `call` (it joined, or answered our join), with
    /// sessions to `links`. Returns whether it's new to us: then, if
    /// we're in the call, the group layer answers it with our own `Join`,
    /// so it learns of us even if our join reached it before the call did.
    pub fn group_call_joined(
        &self,
        call: u64,
        device: PublicIdentity,
        video: bool,
        links: Vec<PublicIdentity>,
    ) -> bool {
        let me = self.identity();
        if device == me {
            return false;
        }
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return false;
        };
        c.links.insert(device, links);
        if let Some(p) = c.info.participants.iter_mut().find(|(p, _)| *p == device) {
            p.1 = video;
            return false;
        }
        if c.info.participants.len() + usize::from(c.info.joined) >= MAX_GROUP_CALL {
            return false;
        }
        c.info.participants.push((device, video));
        let joined = c.info.joined;
        if joined {
            add_leg(c, &me, device);
        }
        let group = c.info.group;
        drop(gc);
        // Try for a session of our own with it, where we can, and one
        // with datagrams.
        self.seek(&device);
        if joined {
            self.upgrade_for_media(device);
        }
        self.emit(Event::GroupCallMember {
            group,
            call,
            device,
            joined: true,
        });
        true
    }

    /// `device` left call `call`.
    pub fn group_call_left(&self, call: u64, device: PublicIdentity) {
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return;
        };
        if !c.is_participant(&device) {
            return;
        }
        c.info.participants.retain(|(p, _)| *p != device);
        c.legs.remove(&device);
        c.links.remove(&device);
        let group = c.info.group;
        // Nobody left to ring with: it stops ringing here.
        let over = !c.info.joined && c.info.participants.is_empty();
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
    }

    /// `device` sent an update: its video, and its links.
    pub fn group_call_update(
        &self,
        call: u64,
        device: PublicIdentity,
        video: bool,
        links: Vec<PublicIdentity>,
    ) {
        let mut gc = lock(&self.shared.group_calls);
        let Some(c) = gc.current.as_mut().filter(|c| c.info.call == call) else {
            return;
        };
        let Some(p) = c.info.participants.iter_mut().find(|(p, _)| *p == device) else {
            return;
        };
        let changed = p.1 != video;
        p.1 = video;
        c.links.insert(device, links);
        if !changed {
            return;
        }
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

    /// Sends the media layer's signalling for our leg with `peer` over our
    /// own session with it (in the ratchet, as `CallMsg::Signal` under the
    /// leg's id). False when there's no session: the group layer sends it
    /// in a group message instead (`GroupCallMsg::Leg`), never the hub.
    pub fn send_leg_signal(&self, call: u64, peer: &PublicIdentity, data: Vec<u8>) -> bool {
        let leg = {
            let gc = lock(&self.shared.group_calls);
            match gc
                .current
                .as_ref()
                .filter(|c| c.info.call == call && c.info.joined)
                .and_then(|c| c.legs.get(peer))
            {
                Some(l) => l.keys.call(),
                None => return false,
            }
        };
        let Ok(m) = (call::CallMsg::Signal { call: leg, data }).encode() else {
            return false;
        };
        self.connected(peer) && self.send_tracked(peer, AppMessage::Call(m)).is_ok()
    }

    /// `CallMsg::Signal` from `peer` under leg id `leg`: our leg's with
    /// `peer`; reported as [`Event::GroupLegSignal`]. It may come before
    /// we've heard `peer` join (it travels in the session, the join in the
    /// group), so it counts by the leg's id; the media layer holds it
    /// until the leg opens.
    pub(crate) fn on_leg_signal(&self, peer: PublicIdentity, leg: u64, data: Vec<u8>) {
        let me = self.identity();
        let call = {
            let gc = lock(&self.shared.group_calls);
            match gc.current.as_ref().filter(|c| {
                c.info.joined && call::leg_id(c.info.call, me.as_bytes(), peer.as_bytes()) == leg
            }) {
                Some(c) => c.info.call,
                None => return,
            }
        };
        self.emit(Event::GroupLegSignal {
            call,
            from: peer,
            data,
        });
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
    /// own session with it, else through a participant linked to both of
    /// us (the best-linked first). False when it couldn't go (dropped, not
    /// queued).
    pub fn send_leg_media(
        &self,
        call: u64,
        peer: &PublicIdentity,
        stream: u8,
        payload: &[u8],
    ) -> Result<bool> {
        let me = self.identity();
        let (packet, relays) = {
            let mut gc = lock(&self.shared.group_calls);
            let c = gc
                .current
                .as_mut()
                .filter(|c| c.info.call == call && c.info.joined)
                .ok_or_else(|| NetError::NotAllowed("no such call".into()))?;
            let mine = self.links_of(c);
            let hub = c.hub(&me, &mine);
            // Participants we reach that say they reach `peer`.
            let mut relays: Vec<PublicIdentity> = mine
                .iter()
                .filter(|r| *r != peer && c.linked(r, peer))
                .copied()
                .collect();
            relays.sort_by_key(|r| (*r != hub, *r.as_bytes()));
            let leg = c
                .legs
                .get_mut(peer)
                .ok_or_else(|| NetError::NotAllowed("not in the call".into()))?;
            (leg.keys.seal(stream, payload)?, relays)
        };
        if self.connected(peer) {
            return Ok(self.send_packet(peer, packet));
        }
        let Some(relay) = relays.first() else {
            return Ok(false);
        };
        let wrapped = call::wrap_relayed(peer.as_bytes(), &packet);
        Ok(self.send_packet(relay, wrapped))
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
    /// (for us to pass on, or passed on to us) or one of our legs.
    /// Returns false when it isn't, for the one-to-one call.
    pub(crate) fn on_group_media(&self, from: PublicIdentity, packet: &[u8]) -> bool {
        let me = self.identity();
        if let Some((device, inner)) = call::unwrap_relayed(packet) {
            let mut gc = lock(&self.shared.group_calls);
            let Some(c) = gc.current.as_mut().filter(|c| c.info.joined) else {
                return true;
            };
            let Ok(other) = PublicIdentity::from_bytes(&device) else {
                return true;
            };
            let id = call::call_of(inner);
            if !c.is_participant(&from) || !c.is_participant(&other) {
                return true;
            }
            // `other`'s packet on our leg with it, passed on by `from`.
            if id == Some(call::leg_id(c.info.call, me.as_bytes(), other.as_bytes())) {
                open_leg(c, &other, inner);
                return true;
            }
            // For us to pass on: only a packet of the leg between the two.
            if other != me
                && id == Some(call::leg_id(c.info.call, from.as_bytes(), other.as_bytes()))
            {
                drop(gc);
                self.send_packet(&other, call::wrap_relayed(from.as_bytes(), inner));
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

    /// Stops a group call ringing after a minute, drops participants whose
    /// legs went silent, and reports when our links change
    /// ([`Event::GroupCallLinks`], for the group layer to send an update).
    fn watch_group_call(&self, call: u64) {
        let node = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_EVERY).await;
                let now = now_ms();
                let (ring_over, silent, relinked) = {
                    let gc = lock(&node.shared.group_calls);
                    let Some(c) = gc.current.as_ref().filter(|c| c.info.call == call) else {
                        return;
                    };
                    let mut mine = node.links_of(c);
                    mine.sort_by_key(|p| *p.as_bytes());
                    let mut told = c.told.clone();
                    told.sort_by_key(|p| *p.as_bytes());
                    let relinked = c.info.joined && mine != told;
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
                    (ring_over, silent, relinked)
                };
                if ring_over {
                    node.leave_group_call(call);
                    return;
                }
                if relinked {
                    node.emit(Event::GroupCallLinks { call });
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
