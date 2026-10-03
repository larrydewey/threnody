//! Onion circuits over link sessions (`docs/appendix-i-onion.md`).
//!
//! ```text
//! OnionMsg = { 0: op, 1: circ u64, ? 2: e_pub, ? 3: id, ? 4: ct, ? 5: sig, ? 6: cell }
//! op: 1 create, 2 created, 3 cell, 4 destroy
//! ```
//!
//! Each relay keeps, per circuit, its hop keys and the link circuit to the
//! next hop. It learns only its neighbours on the path: the destination is
//! named inside an EXTEND cell readable by the last relay alone.

use std::collections::HashMap;
use std::time::Duration;

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::crypto::random_bytes;
use threnody_core::onion::{self, Cell, Cmd, CreateState, HopKeys, MAX_DATA, OnionPath, Payload};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{NetError, Result};
use crate::handshake;
use crate::node::{Node, lock};

/// Most circuits one neighbour may hold through us.
pub const MAX_CIRCUITS_PER_PEER: usize = 64;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum OnionMsg {
    Create {
        circ: u64,
        e_pub: Vec<u8>,
    },
    Created {
        circ: u64,
        id: [u8; 32],
        ct: Vec<u8>,
        sig: [u8; 64],
    },
    Cell {
        circ: u64,
        cell: Vec<u8>,
    },
    Destroy {
        circ: u64,
    },
}

impl OnionMsg {
    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(onion::CELL_LEN + 1400, |e| {
            match self {
                Self::Create { circ, e_pub } => {
                    e.map_len(3)?.u8(0)?.u8(1)?.u8(1)?.u64(*circ)?;
                    e.u8(2)?.bytes(e_pub)?;
                }
                Self::Created { circ, id, ct, sig } => {
                    e.map_len(5)?.u8(0)?.u8(2)?.u8(1)?.u64(*circ)?;
                    e.u8(3)?.bytes(id)?;
                    e.u8(4)?.bytes(ct)?;
                    e.u8(5)?.bytes(sig)?;
                }
                Self::Cell { circ, cell } => {
                    e.map_len(3)?.u8(0)?.u8(3)?.u8(1)?.u64(*circ)?;
                    e.u8(6)?.bytes(cell)?;
                }
                Self::Destroy { circ } => {
                    e.map_len(2)?.u8(0)?.u8(4)?.u8(1)?.u64(*circ)?;
                }
            }
            Ok(())
        })
    }

    fn decode(b: &[u8]) -> threnody_core::Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut circ, mut e_pub, mut id, mut ct, mut sig, mut cell) =
            (None, None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => circ = Some(d.u64()?),
                2 => e_pub = Some(d.bytes()?.to_vec()),
                3 => id = Some(fixed_bytes::<32>(d)?),
                4 => ct = Some(d.bytes()?.to_vec()),
                5 => sig = Some(fixed_bytes::<64>(d)?),
                6 => cell = Some(d.bytes()?.to_vec()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let circ = required(circ, "circuit id")?;
        Ok(match required(op, "onion op")? {
            1 => Self::Create {
                circ,
                e_pub: required(e_pub, "ephemeral key")?,
            },
            2 => Self::Created {
                circ,
                id: required(id, "hop identity")?,
                ct: required(ct, "kem ciphertext")?,
                sig: required(sig, "hop signature")?,
            },
            3 => Self::Cell {
                circ,
                cell: required(cell, "cell")?,
            },
            4 => Self::Destroy { circ },
            other => return Err(threnody_core::Error::UnexpectedType(u64::from(other))),
        })
    }
}

type Link = (PublicIdentity, u64);
type CreatedFields = ([u8; 32], Vec<u8>, [u8; 64]);

/// This node as a hop on someone's circuit.
struct RelayHop {
    keys: HopKeys,
    next: Option<Link>,
    endpoint: Option<mpsc::UnboundedSender<Vec<u8>>>,
}

#[derive(Default)]
pub(crate) struct OnionState {
    /// Keyed by the upstream link circuit.
    hops: HashMap<Link, RelayHop>,
    /// Downstream link circuit -> upstream link circuit.
    down: HashMap<Link, Link>,
    /// Downstream creates we sent for an EXTEND, awaiting CREATED.
    extending: HashMap<Link, Link>,
    /// Our own circuits: first-hop link circuit -> backward cells.
    origins: HashMap<Link, mpsc::UnboundedSender<Cell>>,
    /// Our first-hop CREATEs awaiting CREATED.
    creating: HashMap<Link, oneshot::Sender<CreatedFields>>,
}

impl Node {
    fn onion_send(&self, to: &PublicIdentity, m: &OnionMsg) -> bool {
        m.encode()
            .ok()
            .is_some_and(|b| self.send(to, AppMessage::Onion(b)).is_ok())
    }

    pub(crate) fn on_onion(&self, from: PublicIdentity, payload: &[u8]) {
        let Ok(msg) = OnionMsg::decode(payload) else {
            return;
        };
        match msg {
            OnionMsg::Create { circ, e_pub } => self.onion_create(from, circ, &e_pub),
            OnionMsg::Created { circ, id, ct, sig } => self.onion_created(from, circ, id, ct, sig),
            OnionMsg::Cell { circ, cell } => {
                if let Ok(cell) = onion::cell_from(&cell) {
                    self.onion_cell(from, circ, cell);
                }
            }
            OnionMsg::Destroy { circ } => self.onion_destroy((from, circ), true),
        }
    }

    fn onion_create(&self, from: PublicIdentity, circ: u64, e_pub: &[u8]) {
        let admitted = {
            let st = lock(&self.shared.onion);
            self.shared.mutual(&from)
                && !st.hops.contains_key(&(from, circ))
                && st.hops.keys().filter(|(p, _)| *p == from).count() < MAX_CIRCUITS_PER_PEER
        };
        let responded = admitted
            .then(|| onion::respond(self.identity_ref(), e_pub).ok())
            .flatten();
        let Some((keys, ct, sig)) = responded else {
            self.onion_send(&from, &OnionMsg::Destroy { circ });
            return;
        };
        lock(&self.shared.onion).hops.insert(
            (from, circ),
            RelayHop {
                keys,
                next: None,
                endpoint: None,
            },
        );
        let id = *self.identity().as_bytes();
        self.onion_send(&from, &OnionMsg::Created { circ, id, ct, sig });
    }

    fn onion_created(
        &self,
        from: PublicIdentity,
        circ: u64,
        id: [u8; 32],
        ct: Vec<u8>,
        sig: [u8; 64],
    ) {
        let mut st = lock(&self.shared.onion);
        if let Some(tx) = st.creating.remove(&(from, circ)) {
            let _ = tx.send((id, ct, sig));
            return;
        }
        let Some(up) = st.extending.remove(&(from, circ)) else {
            return;
        };
        let Some(hop) = st.hops.get_mut(&up) else {
            return;
        };
        hop.next = Some((from, circ));
        let mut data = id.to_vec();
        data.extend_from_slice(&ct);
        data.extend_from_slice(&sig);
        let cell = hop.keys.relay_originate(&Payload::new(Cmd::Extended, data));
        st.down.insert((from, circ), up);
        drop(st);
        if let Ok(cell) = cell {
            self.onion_send(
                &up.0,
                &OnionMsg::Cell {
                    circ: up.1,
                    cell: cell.to_vec(),
                },
            );
        }
    }

    fn onion_cell(&self, from: PublicIdentity, circ: u64, mut cell: Cell) {
        let link = (from, circ);
        let mut st = lock(&self.shared.onion);
        // From upstream: peel our layer.
        if let Some(hop) = st.hops.get_mut(&link) {
            match hop.keys.relay_forward(&mut cell) {
                Some(p) => {
                    drop(st);
                    self.onion_recognized(link, p);
                }
                None => {
                    if let Some((next, c)) = hop.next {
                        drop(st);
                        self.onion_send(
                            &next,
                            &OnionMsg::Cell {
                                circ: c,
                                cell: cell.to_vec(),
                            },
                        );
                    }
                }
            }
            return;
        }
        // From downstream: add our layer and pass upstream.
        if let Some(up) = st.down.get(&link).copied() {
            if let Some(hop) = st.hops.get_mut(&up) {
                hop.keys.relay_backward(&mut cell);
                drop(st);
                self.onion_send(
                    &up.0,
                    &OnionMsg::Cell {
                        circ: up.1,
                        cell: cell.to_vec(),
                    },
                );
            }
            return;
        }
        // For one of our own circuits.
        if let Some(tx) = st.origins.get(&link) {
            let _ = tx.send(cell);
        }
    }

    fn onion_reply(&self, up: Link, p: &Payload) {
        let cell = lock(&self.shared.onion)
            .hops
            .get_mut(&up)
            .and_then(|h| h.keys.relay_originate(p).ok());
        if let Some(cell) = cell {
            self.onion_send(
                &up.0,
                &OnionMsg::Cell {
                    circ: up.1,
                    cell: cell.to_vec(),
                },
            );
        }
    }

    /// A cell addressed to us as a hop on `up`'s circuit.
    fn onion_recognized(&self, up: Link, p: Payload) {
        match p.cmd {
            Cmd::Extend => {
                let target = (p.data.len() == 20 + threnody_core::crypto::hybrid::PUBLIC_LEN)
                    .then(|| <[u8; 20]>::try_from(&p.data[..20]).ok())
                    .flatten()
                    .map(Fingerprint);
                let already = lock(&self.shared.onion)
                    .hops
                    .get(&up)
                    .is_none_or(|h| h.next.is_some());
                // Extend only to a live, mutually approved direct contact.
                let next = target.filter(|_| !already).and_then(|fp| {
                    self.sessions()
                        .into_iter()
                        .find(|s| s.via.is_none() && s.peer.fingerprint() == fp && s.peer != up.0)
                        .map(|s| s.peer)
                        .filter(|p| self.shared.mutual(p))
                });
                let Some(next) = next else {
                    self.onion_reply(up, &Payload::new(Cmd::ExtendFailed, vec![]));
                    return;
                };
                let c = u64::from_le_bytes(random_bytes());
                lock(&self.shared.onion).extending.insert((next, c), up);
                let e_pub = p.data[20..].to_vec();
                if !self.onion_send(&next, &OnionMsg::Create { circ: c, e_pub }) {
                    lock(&self.shared.onion).extending.remove(&(next, c));
                    self.onion_reply(up, &Payload::new(Cmd::ExtendFailed, vec![]));
                }
            }
            Cmd::Begin => self.onion_endpoint(up),
            Cmd::Data => {
                if let Some(tx) = lock(&self.shared.onion)
                    .hops
                    .get(&up)
                    .and_then(|h| h.endpoint.clone())
                {
                    let _ = tx.send(p.data);
                }
            }
            Cmd::End => self.onion_destroy(up, true),
            Cmd::Extended | Cmd::ExtendFailed => {}
        }
    }

    /// We are the circuit's destination: attach a session to the stream.
    fn onion_endpoint(&self, up: Link) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        {
            let mut st = lock(&self.shared.onion);
            let Some(hop) = st.hops.get_mut(&up) else {
                return;
            };
            if hop.next.is_some() || hop.endpoint.is_some() {
                return;
            }
            hop.endpoint = Some(tx);
        }
        let node = self.clone();
        tokio::spawn(async move {
            let (mut rd, mut wr) = tokio::io::split(ours);
            let mut buf = vec![0u8; MAX_DATA];
            loop {
                tokio::select! {
                    n = rd.read(&mut buf) => match n {
                        Ok(n) if n > 0 => node.onion_reply(up, &Payload::new(Cmd::Data, buf[..n].to_vec())),
                        _ => break,
                    },
                    inbound = rx.recv() => match inbound {
                        Some(d) => if wr.write_all(&d).await.is_err() { break },
                        None => break,
                    },
                }
            }
            node.onion_reply(up, &Payload::new(Cmd::End, vec![]));
            node.onion_destroy(up, true);
        });
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
                Ok(chan) => node.spawn_onion_session(stream, chan, up.0, false),
                Err(e) => {
                    node.onion_destroy(up, true);
                    node.emit_rejected_relay(up.0, e.to_string());
                }
            }
        });
    }

    /// Tears down the circuit touching link circuit `link`, telling the
    /// other side when `propagate`.
    fn onion_destroy(&self, link: Link, propagate: bool) {
        let mut notify = Vec::new();
        {
            let mut st = lock(&self.shared.onion);
            st.origins.remove(&link);
            st.creating.remove(&link);
            st.extending.remove(&link);
            if let Some(hop) = st.hops.remove(&link) {
                if let Some(next) = hop.next {
                    st.down.remove(&next);
                    notify.push(next);
                }
            } else if let Some(up) = st.down.remove(&link) {
                if let Some(hop) = st.hops.remove(&up) {
                    drop(hop);
                }
                notify.push(up);
            }
        }
        if propagate {
            for (peer, c) in notify {
                self.onion_send(&peer, &OnionMsg::Destroy { circ: c });
            }
        }
    }

    /// Drops every onion circuit using the link to `peer`.
    pub(crate) fn drop_onion_circuits_of(&self, peer: &PublicIdentity) {
        let links: Vec<Link> = {
            let st = lock(&self.shared.onion);
            st.hops
                .keys()
                .chain(st.down.keys())
                .chain(st.origins.keys())
                .filter(|(p, _)| p == peer)
                .copied()
                .collect()
        };
        for l in links {
            self.onion_destroy(l, true);
        }
    }

    /// Number of onion circuits this node is carrying for others.
    pub fn onion_hops(&self) -> usize {
        lock(&self.shared.onion).hops.len()
    }

    /// Opens an onion circuit to `dest` through at least `min_relays`
    /// relays (2 by default hides the ends from every single relay), then
    /// an authenticated end-to-end session over it.
    pub async fn connect_onion(
        &self,
        dest: Fingerprint,
        min_relays: usize,
    ) -> Result<PublicIdentity> {
        let me = self.identity();
        if dest == me.fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        let firsts: Vec<PublicIdentity> = self
            .sessions()
            .into_iter()
            .filter(|s| {
                s.via.is_none() && s.peer.fingerprint() != dest && self.shared.mutual(&s.peer)
            })
            .map(|s| s.peer)
            .collect();
        let middles: Vec<PublicIdentity> = self
            .contacts()
            .iter()
            .filter(|c| c.mutually_approved() && c.fingerprint() != dest && c.key != me)
            .map(|c| c.key)
            .collect();
        let mut paths: Vec<Vec<Fingerprint>> = Vec::new();
        for r1 in &firsts {
            for r2 in middles.iter().filter(|m| *m != r1) {
                paths.push(vec![r1.fingerprint(), r2.fingerprint(), dest]);
            }
        }
        if min_relays <= 1 {
            paths.extend(firsts.iter().map(|r1| vec![r1.fingerprint(), dest]));
        }
        for path in paths {
            let first = firsts
                .iter()
                .find(|p| p.fingerprint() == path[0])
                .copied()
                .ok_or(NetError::Closed)?;
            if let Ok(peer) = self.try_onion_path(first, &path).await {
                return Ok(peer);
            }
        }
        Err(NetError::NoRoute(format!(
            "{dest} (no onion path with {min_relays}+ relays among approved contacts)"
        )))
    }

    async fn try_onion_path(
        &self,
        first: PublicIdentity,
        path: &[Fingerprint],
    ) -> Result<PublicIdentity> {
        let circ = u64::from_le_bytes(random_bytes());
        let link = (first, circ);
        let (tx, mut rx) = mpsc::unbounded_channel::<Cell>();
        let result = async {
            // First hop: CREATE over the link.
            let (st, e_pub) = CreateState::new(path[0]);
            let (ctx, crx) = oneshot::channel();
            {
                let mut s = lock(&self.shared.onion);
                s.origins.insert(link, tx);
                s.creating.insert(link, ctx);
            }
            if !self.onion_send(&first, &OnionMsg::Create { circ, e_pub }) {
                return Err(NetError::Closed);
            }
            let (id, ct, sig) = tokio::time::timeout(STEP_TIMEOUT, crx)
                .await
                .map_err(|_| NetError::Timeout)?
                .map_err(|_| NetError::Closed)?;
            let mut onion = OnionPath::default();
            onion.push(st.finish(&id, &ct, &sig)?);

            // Every further hop: EXTEND through the circuit.
            for fp in &path[1..] {
                let (st, e_pub) = CreateState::new(*fp);
                let mut data = fp.0.to_vec();
                data.extend_from_slice(&e_pub);
                let cell = onion.wrap(onion.len() - 1, &Payload::new(Cmd::Extend, data))?;
                if !self.onion_send(
                    &first,
                    &OnionMsg::Cell {
                        circ,
                        cell: cell.to_vec(),
                    },
                ) {
                    return Err(NetError::Closed);
                }
                let back = tokio::time::timeout(STEP_TIMEOUT, rx.recv())
                    .await
                    .map_err(|_| NetError::Timeout)?
                    .ok_or(NetError::Closed)?;
                let (hop, p) = onion.unwrap(back)?;
                if hop != onion.len() - 1
                    || p.cmd != Cmd::Extended
                    || p.data.len() != 32 + 1120 + 64
                {
                    return Err(NetError::NoRoute(fp.to_string()));
                }
                let id: [u8; 32] = p.data[..32].try_into().map_err(|_| NetError::Closed)?;
                let sig: [u8; 64] = p.data[32 + 1120..]
                    .try_into()
                    .map_err(|_| NetError::Closed)?;
                onion.push(st.finish(&id, &p.data[32..32 + 1120], &sig)?);
            }
            let last = onion.len() - 1;
            let begin = onion.wrap(last, &Payload::new(Cmd::Begin, vec![]))?;
            if !self.onion_send(
                &first,
                &OnionMsg::Cell {
                    circ,
                    cell: begin.to_vec(),
                },
            ) {
                return Err(NetError::Closed);
            }
            Ok(onion)
        }
        .await;
        let mut onion = match result {
            Ok(o) => o,
            Err(e) => {
                self.onion_destroy(link, false);
                self.onion_send(&first, &OnionMsg::Destroy { circ });
                return Err(e);
            }
        };

        // Stream pump between the end-to-end session and the circuit.
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let node = self.clone();
        tokio::spawn(async move {
            let (mut rd, mut wr) = tokio::io::split(ours);
            let mut buf = vec![0u8; MAX_DATA];
            let last = onion.len() - 1;
            loop {
                tokio::select! {
                    n = rd.read(&mut buf) => {
                        let Ok(n) = n else { break };
                        if n == 0 { break }
                        let Ok(cell) = onion.wrap(last, &Payload::new(Cmd::Data, buf[..n].to_vec())) else { break };
                        if !node.onion_send(&first, &OnionMsg::Cell { circ, cell: cell.to_vec() }) { break }
                    }
                    back = rx.recv() => {
                        let Some(back) = back else { break };
                        match onion.unwrap(back) {
                            Ok((h, p)) if h == last && p.cmd == Cmd::Data => {
                                if wr.write_all(&p.data).await.is_err() { break }
                            }
                            Ok((_, p)) if p.cmd == Cmd::End => break,
                            _ => {}
                        }
                    }
                }
            }
            node.onion_destroy(link, false);
            node.onion_send(&first, &OnionMsg::Destroy { circ });
        });

        let mut stream = theirs;
        let chan = handshake::initiate(&mut stream, self.identity_ref()).await?;
        let dest = *path.last().ok_or(NetError::Closed)?;
        if chan.peer().fingerprint() != dest {
            return Err(NetError::IdentityMismatch {
                expected: dest.to_string(),
                got: chan.peer().fingerprint().to_string(),
            });
        }
        let peer = *chan.peer();
        self.spawn_onion_session(stream, chan, first, true);
        Ok(peer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onion_messages_round_trip() {
        for m in [
            OnionMsg::Create {
                circ: 1,
                e_pub: vec![1; 8],
            },
            OnionMsg::Created {
                circ: 2,
                id: [3; 32],
                ct: vec![4; 8],
                sig: [5; 64],
            },
            OnionMsg::Cell {
                circ: 6,
                cell: vec![7; 16],
            },
            OnionMsg::Destroy { circ: 8 },
        ] {
            assert_eq!(OnionMsg::decode(&m.encode().unwrap()).unwrap(), m);
        }
    }
}
