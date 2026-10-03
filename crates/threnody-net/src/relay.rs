//! Multi-hop relay circuits between mutually approved nodes (spec §7.3, §7.4).
//!
//! A circuit is a chain of hops, each a pair `(peer, circuit id)` local to
//! one link. Every relay keeps only "frames arriving on (P, c) go to
//! (Q, d)". The two endpoints run an ordinary Threnody handshake and
//! session over the circuit, so relays see only end-to-end ciphertext, and
//! each hop additionally wraps it in that link's own ratchet.
//!
//! ```text
//! RelayMsg = { 0: op, 1: circ u64, ? 2: dest fingerprint (20), ? 3: ttl, ? 4: nonce (16), ? 5: frame }
//! op: 1 open, 2 opened, 3 refused, 4 data, 5 close
//! ```
//!
//! Policy: a node relays only between its own mutually approved contacts
//! and accepts circuits only from mutually approved neighbours. Route
//! discovery is a depth-first search bounded by `ttl` (at most
//! [`MAX_TTL`] relays); open nonces are remembered so loops are refused.
//! See `docs/appendix-g-relay.md`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::crypto::random_bytes;
use threnody_core::{AppMessage, Fingerprint, PublicIdentity};
use tokio::sync::{mpsc, oneshot};

use crate::error::{NetError, Result};
use crate::frame::{read_frame, write_frame};
use crate::handshake;
use crate::node::{Node, lock};

/// Most relays a circuit may pass through.
pub const MAX_TTL: u8 = 3;
/// Most circuits one neighbour may hold through us.
pub const MAX_CIRCUITS_PER_PEER: usize = 64;
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
const NONCE_MEMORY: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RelayMsg {
    Open {
        circ: u64,
        dest: [u8; 20],
        ttl: u8,
        nonce: [u8; 16],
    },
    Opened {
        circ: u64,
    },
    Refused {
        circ: u64,
    },
    Data {
        circ: u64,
        frame: Vec<u8>,
    },
    Close {
        circ: u64,
    },
}

impl RelayMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(
            64 + if let Self::Data { frame, .. } = self {
                frame.len()
            } else {
                0
            },
            |e| {
                match self {
                    Self::Open {
                        circ,
                        dest,
                        ttl,
                        nonce,
                    } => {
                        e.map_len(5)?;
                        e.u8(0)?.u8(1)?;
                        e.u8(1)?.u64(*circ)?;
                        e.u8(2)?.bytes(dest)?;
                        e.u8(3)?.u8(*ttl)?;
                        e.u8(4)?.bytes(nonce)?;
                    }
                    Self::Opened { circ } | Self::Refused { circ } | Self::Close { circ } => {
                        let op = match self {
                            Self::Opened { .. } => 2,
                            Self::Refused { .. } => 3,
                            _ => 5,
                        };
                        e.map_len(2)?;
                        e.u8(0)?.u8(op)?;
                        e.u8(1)?.u64(*circ)?;
                    }
                    Self::Data { circ, frame } => {
                        e.map_len(3)?;
                        e.u8(0)?.u8(4)?;
                        e.u8(1)?.u64(*circ)?;
                        e.u8(5)?.bytes(frame)?;
                    }
                }
                Ok(())
            },
        )
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut circ, mut dest, mut ttl, mut nonce, mut frame) =
            (None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => circ = Some(d.u64()?),
                2 => dest = Some(fixed_bytes::<20>(d)?),
                3 => ttl = Some(d.u8()?),
                4 => nonce = Some(fixed_bytes::<16>(d)?),
                5 => frame = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let circ = required(circ, "circuit id")?;
        Ok(match required(op, "relay op")? {
            1 => Self::Open {
                circ,
                dest: required(dest, "destination")?,
                ttl: required(ttl, "ttl")?,
                nonce: required(nonce, "nonce")?,
            },
            2 => Self::Opened { circ },
            3 => Self::Refused { circ },
            4 => Self::Data {
                circ,
                frame: required(frame, "frame")?,
            },
            5 => Self::Close { circ },
            other => return Err(threnody_core::Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// Where frames arriving on one hop go.
enum Hop {
    /// Relay: to this peer, on this circuit.
    Forward(PublicIdentity, u64),
    /// We are an endpoint: to the local session's pump.
    Endpoint(mpsc::UnboundedSender<Vec<u8>>),
}

#[derive(Default)]
pub(crate) struct RelayState {
    hops: HashMap<(PublicIdentity, u64), Hop>,
    pending: HashMap<(PublicIdentity, u64), oneshot::Sender<bool>>,
    seen: HashMap<[u8; 16], Instant>,
}

impl RelayState {
    fn circuits_with(&self, peer: &PublicIdentity) -> usize {
        self.hops.keys().filter(|(p, _)| p == peer).count()
    }

    /// Records an open nonce; false if it was already seen (a loop).
    fn first_sighting(&mut self, nonce: [u8; 16]) -> bool {
        self.seen.retain(|_, t| t.elapsed() < NONCE_MEMORY);
        self.seen.insert(nonce, Instant::now()).is_none()
    }
}

impl Node {
    fn relay_send(&self, to: &PublicIdentity, msg: &RelayMsg) -> bool {
        msg.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Relay(b)).is_ok())
    }

    /// Handles a relay message that arrived in the session with `from`.
    pub(crate) fn on_relay(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = RelayMsg::decode(payload) else {
            return;
        };
        match msg {
            RelayMsg::Open {
                circ,
                dest,
                ttl,
                nonce,
            } => {
                let admitted = {
                    let mut st = lock(&self.shared.relay);
                    self.shared.mutual(&from)
                        && ttl <= MAX_TTL
                        && st.circuits_with(&from) < MAX_CIRCUITS_PER_PEER
                        && !st.hops.contains_key(&(from, circ))
                        && st.first_sighting(nonce)
                };
                if !admitted {
                    self.relay_send(&from, &RelayMsg::Refused { circ });
                    return;
                }
                let node = self.clone();
                tokio::spawn(async move { node.handle_open(from, circ, dest, ttl, nonce).await });
            }
            RelayMsg::Opened { circ } | RelayMsg::Refused { circ } => {
                let ok = matches!(msg, RelayMsg::Opened { .. });
                if let Some(tx) = lock(&self.shared.relay).pending.remove(&(from, circ)) {
                    let _ = tx.send(ok);
                }
            }
            RelayMsg::Data { circ, frame } => {
                let target = match lock(&self.shared.relay).hops.get(&(from, circ)) {
                    Some(Hop::Forward(to, c2)) => Some((*to, *c2)),
                    Some(Hop::Endpoint(tx)) => {
                        let _ = tx.send(frame);
                        return;
                    }
                    None => None,
                };
                match target {
                    Some((to, c2)) => {
                        if !self.relay_send(&to, &RelayMsg::Data { circ: c2, frame }) {
                            self.close_circuit(from, circ);
                        }
                    }
                    None => {
                        self.relay_send(&from, &RelayMsg::Close { circ });
                    }
                }
            }
            RelayMsg::Close { circ } => self.close_circuit(from, circ),
        }
    }

    /// Tears down the circuit hop `(peer, circ)` and its partner hop.
    pub(crate) fn close_circuit(&self, peer: PublicIdentity, circ: u64) {
        let partner = {
            let mut st = lock(&self.shared.relay);
            match st.hops.remove(&(peer, circ)) {
                Some(Hop::Forward(to, c2)) => {
                    st.hops.remove(&(to, c2));
                    Some((to, c2))
                }
                // Dropping the endpoint sender ends the pump and its session.
                _ => None,
            }
        };
        if let Some((to, c2)) = partner {
            self.relay_send(&to, &RelayMsg::Close { circ: c2 });
        }
    }

    /// Closes every circuit that runs over the link to `peer` (it went away).
    pub(crate) fn drop_circuits_of(&self, peer: &PublicIdentity) {
        let circs: Vec<u64> = lock(&self.shared.relay)
            .hops
            .keys()
            .filter(|(p, _)| p == peer)
            .map(|(_, c)| *c)
            .collect();
        for c in circs {
            self.close_circuit(*peer, c);
        }
        lock(&self.shared.relay)
            .pending
            .retain(|(p, _), _| p != peer);
    }

    async fn handle_open(
        &self,
        from: PublicIdentity,
        circ: u64,
        dest: [u8; 20],
        ttl: u8,
        nonce: [u8; 16],
    ) {
        if dest == self.identity().fingerprint().0 {
            self.accept_circuit(from, circ);
            return;
        }
        let dest = Fingerprint(dest);
        match self.extend(dest, ttl, nonce, &[from]).await {
            Some((next, c2)) => {
                {
                    let mut st = lock(&self.shared.relay);
                    st.hops.insert((from, circ), Hop::Forward(next, c2));
                    st.hops.insert((next, c2), Hop::Forward(from, circ));
                }
                if !self.relay_send(&from, &RelayMsg::Opened { circ }) {
                    self.close_circuit(from, circ);
                }
            }
            None => {
                self.relay_send(&from, &RelayMsg::Refused { circ });
            }
        }
    }

    /// Finds the next hop towards `dest`: the destination itself if it is
    /// a live, mutually approved neighbour; otherwise each approved
    /// neighbour in turn while `ttl` allows another relay.
    async fn extend(
        &self,
        dest: Fingerprint,
        ttl: u8,
        nonce: [u8; 16],
        exclude: &[PublicIdentity],
    ) -> Option<(PublicIdentity, u64)> {
        let live: Vec<PublicIdentity> = self
            .sessions()
            .iter()
            .filter(|s| s.via.is_none())
            .map(|s| s.peer)
            .collect();
        let candidates: Vec<(PublicIdentity, u8)> = if let Some(d) = live
            .iter()
            .find(|p| p.fingerprint() == dest && self.shared.mutual(p))
        {
            vec![(*d, 0)]
        } else if ttl > 0 {
            live.into_iter()
                .filter(|p| {
                    !exclude.contains(p) && p.fingerprint() != dest && self.shared.mutual(p)
                })
                .map(|p| (p, ttl - 1))
                .collect()
        } else {
            vec![]
        };
        for (hop, hop_ttl) in candidates {
            let circ = u64::from_le_bytes(random_bytes());
            let (tx, rx) = oneshot::channel();
            lock(&self.shared.relay).pending.insert((hop, circ), tx);
            let open = RelayMsg::Open {
                circ,
                dest: dest.0,
                ttl: hop_ttl,
                nonce,
            };
            if !self.relay_send(&hop, &open) {
                lock(&self.shared.relay).pending.remove(&(hop, circ));
                continue;
            }
            match tokio::time::timeout(OPEN_TIMEOUT * u32::from(hop_ttl + 1), rx).await {
                Ok(Ok(true)) => return Some((hop, circ)),
                _ => {
                    lock(&self.shared.relay).pending.remove(&(hop, circ));
                }
            }
        }
        None
    }

    /// Endpoint side: binds the circuit to a fresh session (we respond).
    fn accept_circuit(&self, from: PublicIdentity, circ: u64) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        self.bind_endpoint(from, circ, ours);
        if !self.relay_send(&from, &RelayMsg::Opened { circ }) {
            self.close_circuit(from, circ);
            return;
        }
        let node = self.clone();
        tokio::spawn(async move {
            let mut stream = theirs;
            let result = async {
                let chan = handshake::accept(&mut stream, node.identity_ref()).await?;
                node.check_policy(chan.peer())?;
                Ok::<_, NetError>(chan)
            }
            .await;
            match result {
                Ok(chan) => node.spawn_relayed_session(stream, chan, from, false),
                Err(e) => {
                    node.close_circuit(from, circ);
                    node.emit_rejected_relay(from, e.to_string());
                }
            }
        });
    }

    /// Wires one end of an in-memory stream to circuit hop `(peer, circ)`.
    fn bind_endpoint(&self, peer: PublicIdentity, circ: u64, stream: tokio::io::DuplexStream) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        lock(&self.shared.relay)
            .hops
            .insert((peer, circ), Hop::Endpoint(tx));
        let node = self.clone();
        tokio::spawn(async move {
            let (mut rd, mut wr) = tokio::io::split(stream);
            loop {
                tokio::select! {
                    frame = read_frame(&mut rd) => match frame {
                        Ok(Some(f)) => {
                            if !node.relay_send(&peer, &RelayMsg::Data { circ, frame: f }) {
                                break;
                            }
                        }
                        _ => break,
                    },
                    inbound = rx.recv() => match inbound {
                        Some(f) => {
                            if write_frame(&mut wr, &f).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            node.close_circuit(peer, circ);
        });
    }

    /// Reaches `dest` through relays and starts an authenticated session
    /// with it. The destination's fingerprint is pinned.
    pub async fn connect_relayed(&self, dest: Fingerprint) -> Result<PublicIdentity> {
        if dest == self.identity().fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        let nonce: [u8; 16] = random_bytes();
        lock(&self.shared.relay).first_sighting(nonce);
        // As originator we never hand the destination a direct open: a
        // direct session would not need a relay.
        let (via, circ) = self
            .extend_from_origin(dest, nonce)
            .await
            .ok_or_else(|| NetError::NoRoute(dest.to_string()))?;
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        self.bind_endpoint(via, circ, ours);
        let mut stream = theirs;
        let chan = match handshake::initiate(&mut stream, self.identity_ref()).await {
            Ok(c) => c,
            Err(e) => {
                self.close_circuit(via, circ);
                return Err(e);
            }
        };
        if chan.peer().fingerprint() != dest {
            self.close_circuit(via, circ);
            return Err(NetError::IdentityMismatch {
                expected: dest.to_string(),
                got: chan.peer().fingerprint().to_string(),
            });
        }
        let peer = *chan.peer();
        self.spawn_relayed_session(stream, chan, via, true);
        Ok(peer)
    }

    async fn extend_from_origin(
        &self,
        dest: Fingerprint,
        nonce: [u8; 16],
    ) -> Option<(PublicIdentity, u64)> {
        let neighbours: Vec<PublicIdentity> = self
            .sessions()
            .iter()
            .filter(|s| {
                s.via.is_none() && s.peer.fingerprint() != dest && self.shared.mutual(&s.peer)
            })
            .map(|s| s.peer)
            .collect();
        for hop in neighbours {
            let circ = u64::from_le_bytes(random_bytes());
            let (tx, rx) = oneshot::channel();
            lock(&self.shared.relay).pending.insert((hop, circ), tx);
            let open = RelayMsg::Open {
                circ,
                dest: dest.0,
                ttl: MAX_TTL - 1,
                nonce,
            };
            if !self.relay_send(&hop, &open) {
                continue;
            }
            match tokio::time::timeout(OPEN_TIMEOUT * u32::from(MAX_TTL + 1), rx).await {
                Ok(Ok(true)) => return Some((hop, circ)),
                _ => {
                    lock(&self.shared.relay).pending.remove(&(hop, circ));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_messages_round_trip() {
        for m in [
            RelayMsg::Open {
                circ: 7,
                dest: [1; 20],
                ttl: 2,
                nonce: [3; 16],
            },
            RelayMsg::Opened { circ: u64::MAX },
            RelayMsg::Refused { circ: 0 },
            RelayMsg::Data {
                circ: 9,
                frame: vec![1, 2, 3],
            },
            RelayMsg::Close { circ: 1 },
        ] {
            assert_eq!(RelayMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(RelayMsg::decode(&[0xa1, 0x00, 0x01]).is_err());
    }
}
