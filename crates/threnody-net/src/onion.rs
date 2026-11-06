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

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, fixed_bytes, read_map, required};
use threnody_core::crypto::random_bytes;
use threnody_core::onion::{
    self, Cell, Cmd, CreateState, ExtendReq, HopKeys, MAX_DATA, OnionPath, Payload,
};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

use crate::error::{NetError, Result};
use crate::handshake;
use crate::mailbox::DepositStatus;
use crate::node::{Node, lock};

/// Most circuits one neighbour may hold through us.
pub const MAX_CIRCUITS_PER_PEER: usize = 64;
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Steps over anonymous links wait for cover-traffic ticks at each hop.
const VOLUNTEER_STEP_TIMEOUT: Duration = Duration::from_secs(20);
/// Circuits this node carries for others through anonymous links, at most.
pub const MAX_STRANGER_HOPS: usize = 4096;
/// Of those, at most this many from one network (a /24 or a /56), so a
/// few addresses can't take them all.
pub const MAX_STRANGER_HOPS_PER_NETWORK: usize = 256;
/// A hop that hasn't extended, begun a stream or finished a deposit this
/// long after its CREATE is torn down. Building a circuit through
/// volunteers takes at most a minute.
const HOP_SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long an anonymous deposit may take once its circuit is up.
const DEPOSIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Anonymous deposits we accept per neighbour per minute: the depositor is
/// hidden, so the neighbour that carried the circuit is what we limit.
pub const MAX_DEPOSITS_PER_MINUTE: u32 = 30;
/// `to (32) || total_len (u32 BE)` before the sealed bytes.
const DEPOSIT_HEAD: usize = 36;

#[derive(Debug, PartialEq, Eq)]
enum OnionMsg {
    Create {
        circ: u64,
        e_pub: Vec<u8>,
        /// Pays a volunteer relay (Appendix P).
        token: Option<Vec<u8>>,
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
    fn circ(&self) -> u64 {
        match self {
            Self::Create { circ, .. }
            | Self::Created { circ, .. }
            | Self::Cell { circ, .. }
            | Self::Destroy { circ } => *circ,
        }
    }

    fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        cbor::to_vec(onion::CELL_LEN + 1400, |e| {
            match self {
                Self::Create { circ, e_pub, token } => {
                    e.map_len(3 + usize::from(token.is_some()))?
                        .u8(0)?
                        .u8(1)?
                        .u8(1)?
                        .u64(*circ)?;
                    e.u8(2)?.bytes(e_pub)?;
                    if let Some(t) = token {
                        e.u8(7)?.bytes(t)?;
                    }
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
        let (mut op, mut circ, mut e_pub, mut id, mut ct, mut sig, mut cell, mut token) =
            (None, None, None, None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => circ = Some(d.u64()?),
                2 => e_pub = Some(d.bytes()?.to_vec()),
                3 => id = Some(fixed_bytes::<32>(d)?),
                4 => ct = Some(d.bytes()?.to_vec()),
                5 => sig = Some(fixed_bytes::<64>(d)?),
                6 => cell = Some(d.bytes()?.to_vec()),
                7 => {
                    let t = d.bytes()?;
                    if t.len() > MAX_DATA {
                        return Err(threnody_core::Error::Malformed("token too long"));
                    }
                    token = Some(t.to_vec());
                }
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
                token,
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

/// The circuit an encoded onion message is for, read without copying its
/// cell.
pub(crate) fn circ_of(b: &[u8]) -> Option<u64> {
    let mut dec = Decoder::new(b);
    let mut circ = None;
    read_map(&mut dec, |k, d| {
        if k == 1 {
            circ = Some(d.u64()?);
            return Ok(true);
        }
        Ok(false)
    })
    .ok()?;
    circ
}

/// Tests only: an encoded DESTROY for `circ`.
#[cfg(test)]
pub(crate) fn test_destroy(circ: u64) -> Vec<u8> {
    OnionMsg::Destroy { circ }.encode().unwrap()
}

/// One hop of a circuit we build.
#[derive(Clone, Debug)]
pub(crate) struct Hop {
    pub fp: Fingerprint,
    /// Where the previous hop dials it (volunteer relays and destinations
    /// reached through them); `None` for a live contact of the previous hop.
    pub addr: Option<String>,
    /// A volunteer relay, paid with a token.
    pub volunteer: Option<PublicIdentity>,
}

/// A circuit to build: its first hop and how we reach it, then every hop.
#[derive(Clone, Debug)]
pub(crate) struct OnionRoute {
    pub first: PublicIdentity,
    /// The first hop is a volunteer reached over an anonymous link at this
    /// address (else a live session).
    pub first_addr: Option<String>,
    pub hops: Vec<Hop>,
}

impl OnionRoute {
    /// A circuit through contacts only (Appendix I).
    fn contacts(first: PublicIdentity, path: &[Fingerprint]) -> Self {
        Self {
            first,
            first_addr: None,
            hops: path
                .iter()
                .map(|fp| Hop {
                    fp: *fp,
                    addr: None,
                    volunteer: None,
                })
                .collect(),
        }
    }

    fn anon(&self) -> bool {
        self.first_addr.is_some()
    }
}
type CreatedFields = ([u8; 32], Vec<u8>, [u8; 64]);

/// This node as a hop on someone's circuit.
struct RelayHop {
    keys: HopKeys,
    /// May extend: the circuit came from a mutually approved neighbour, or
    /// paid with a token. Others may only end here (BEGIN or DEPOSIT).
    full: bool,
    /// Came from a mutually approved neighbour's session: only such a
    /// circuit may go on to our contacts. A paid one goes on only to the
    /// address its EXTEND names, or a stranger could reach, and probe for,
    /// people who relay for their contacts alone.
    contact: bool,
    next: Option<Link>,
    endpoint: Option<mpsc::UnboundedSender<Vec<u8>>>,
    /// An anonymous mailbox deposit being received.
    deposit: Option<PendingDeposit>,
    /// Where an anonymous link's far end is (for per-network limits).
    net: Option<[u8; 16]>,
    /// When the CREATE came: tells this hop from a later one on the same
    /// link circuit.
    born: Instant,
}

impl RelayHop {
    fn new(keys: HopKeys, contact: bool, paid: bool, net: Option<[u8; 16]>) -> Self {
        Self {
            keys,
            full: contact || paid,
            contact,
            next: None,
            endpoint: None,
            deposit: None,
            net,
            born: Instant::now(),
        }
    }

    /// Carries a circuit on, or is its end.
    fn in_use(&self) -> bool {
        self.next.is_some() || self.endpoint.is_some()
    }
}

struct PendingDeposit {
    to: [u8; 32],
    total: usize,
    sealed: Vec<u8>,
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
    /// Per carrier: start of the current minute and deposits in it.
    deposit_rate: HashMap<Carrier, (Instant, u32)>,
    /// Link circuits that run over anonymous links rather than sessions.
    anon: HashSet<Link>,
    /// Per carrier: start of the current minute and tokens checked in it.
    token_rate: HashMap<Carrier, (Instant, u32)>,
}

/// Relay tokens checked per neighbour per minute: each costs pairings.
pub const MAX_TOKEN_CHECKS_PER_MINUTE: u32 = 120;
/// Anonymous deposits still arriving may hold this many bytes in all.
const MAX_PENDING_DEPOSIT_BYTES: usize = 4 * crate::mailbox::MAX_HELD_BYTES;

/// Who answers for a circuit: the neighbour that carried it to us or,
/// over an anonymous link, where any identity costs nothing, its network.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Carrier {
    Peer(PublicIdentity),
    Network([u8; 16]),
}

impl Carrier {
    fn of(link: &Link, net: Option<[u8; 16]>) -> Self {
        net.map_or(Self::Peer(link.0), Self::Network)
    }
}

impl OnionState {
    /// Whether another stranger's hop fits, from network `net`.
    fn stranger_room(&self, net: Option<[u8; 16]>) -> bool {
        let strangers = self.hops.iter().filter(|(l, _)| self.anon.contains(l));
        let (all, here) = strangers.fold((0, 0), |(a, n), (_, h)| {
            (a + 1, n + usize::from(net.is_some() && h.net == net))
        });
        all < MAX_STRANGER_HOPS && here < MAX_STRANGER_HOPS_PER_NETWORK
    }

    fn token_check_allowed(&mut self, from: Carrier) -> bool {
        let now = Instant::now();
        if self.token_rate.len() > 4 * MAX_STRANGER_HOPS {
            self.token_rate
                .retain(|_, (s, _)| now.duration_since(*s) < Duration::from_secs(60));
        }
        let (start, n) = self.token_rate.entry(from).or_insert((now, 0));
        if now.duration_since(*start) >= Duration::from_secs(60) {
            (*start, *n) = (now, 0);
        }
        *n += 1;
        *n <= MAX_TOKEN_CHECKS_PER_MINUTE
    }

    fn deposit_allowed(&mut self, from: Carrier) -> bool {
        let now = Instant::now();
        if self.deposit_rate.len() > 4 * MAX_STRANGER_HOPS {
            self.deposit_rate
                .retain(|_, (s, _)| now.duration_since(*s) < Duration::from_secs(60));
        }
        let (start, n) = self.deposit_rate.entry(from).or_insert((now, 0));
        if now.duration_since(*start) >= Duration::from_secs(60) {
            (*start, *n) = (now, 0);
        }
        *n += 1;
        *n <= MAX_DEPOSITS_PER_MINUTE
    }

    /// Whether `more` bytes of the deposit arriving over hop `up` may be
    /// kept: while they arrive, deposits are held in memory. What has
    /// arrived counts, not what a deposit's head claims, so holding room
    /// costs sending the bytes. Our contacts' circuits and everyone else's
    /// have room of their own, and within each one carrier (a contact, a
    /// stranger's network) may hold only one deposit's worth.
    fn deposit_room(&self, up: &Link, more: usize) -> bool {
        let Some(hop) = self.hops.get(up) else {
            return false;
        };
        let carrier = Carrier::of(up, hop.net);
        let (all, here) = self
            .hops
            .iter()
            .filter(|(_, h)| h.contact == hop.contact)
            .filter_map(|(l, h)| {
                h.deposit
                    .as_ref()
                    .map(|d| (Carrier::of(l, h.net), d.sealed.len()))
            })
            .fold((0, 0), |(a, n), (c, len)| {
                (a + len, n + if c == carrier { len } else { 0 })
            });
        all + more <= MAX_PENDING_DEPOSIT_BYTES && here + more <= crate::mailbox::MAX_HELD_BYTES
    }
}

impl Node {
    fn onion_send(&self, to: &PublicIdentity, m: &OnionMsg) -> bool {
        let anon = lock(&self.shared.onion).anon.contains(&(*to, m.circ()));
        self.onion_send_via(to, m, anon)
    }

    /// Sends over the anonymous link to `to` when `anon`, else its session.
    fn onion_send_via(&self, to: &PublicIdentity, m: &OnionMsg, anon: bool) -> bool {
        let Ok(b) = m.encode() else { return false };
        if anon {
            return self.anon_send(to, AppMessage::Onion(b));
        }
        self.send(to, AppMessage::Onion(b)).is_ok()
    }

    pub(crate) fn on_onion(&self, from: PublicIdentity, payload: &[u8]) {
        self.on_onion_from(from, payload, false);
    }

    /// An onion message from `from`, over an anonymous link when `anon`.
    pub(crate) fn on_onion_from(&self, from: PublicIdentity, payload: &[u8], anon: bool) {
        let Ok(msg) = OnionMsg::decode(payload) else {
            return;
        };
        match msg {
            OnionMsg::Create { circ, e_pub, token } => {
                self.onion_create(from, circ, &e_pub, token.as_deref(), anon)
            }
            OnionMsg::Created { circ, id, ct, sig } => self.onion_created(from, circ, id, ct, sig),
            OnionMsg::Cell { circ, cell } => {
                if let Ok(cell) = onion::cell_from(&cell) {
                    self.onion_cell(from, circ, cell);
                }
            }
            OnionMsg::Destroy { circ } => self.onion_destroy((from, circ), true),
        }
    }

    fn onion_create(
        &self,
        from: PublicIdentity,
        circ: u64,
        e_pub: &[u8],
        token: Option<&[u8]>,
        anon: bool,
    ) {
        let contact = !anon && self.shared.mutual(&from);
        let net = self
            .anon_addr(&from)
            .filter(|_| anon)
            .map(|a| crate::volunteer::network(a.ip()));
        let admitted = {
            let st = lock(&self.shared.onion);
            !st.hops.contains_key(&(from, circ))
                && st.hops.keys().filter(|(p, _)| *p == from).count() < MAX_CIRCUITS_PER_PEER
                && (contact || st.stranger_room(net))
        };
        // A stranger's circuit may end here; it goes further only if paid.
        let paid = !contact
            && token.is_some_and(|t| {
                admitted
                    && lock(&self.shared.onion).token_check_allowed(Carrier::of(&(from, circ), net))
                    && self.accept_token(t, e_pub)
            });
        let responded = admitted
            .then(|| onion::respond(self.identity_ref(), e_pub).ok())
            .flatten();
        let Some((keys, ct, sig)) = responded else {
            // Back the way it came; nothing of it is kept.
            self.onion_send_via(&from, &OnionMsg::Destroy { circ }, anon);
            return;
        };
        {
            let mut st = lock(&self.shared.onion);
            if anon {
                st.anon.insert((from, circ));
            }
            let hop = RelayHop::new(keys, contact, paid, net);
            self.watch_hop((from, circ), hop.born);
            st.hops.insert((from, circ), hop);
        }
        let id = *self.identity().as_bytes();
        self.onion_send(&from, &OnionMsg::Created { circ, id, ct, sig });
    }

    /// Tears hop `up` down if it still goes nowhere after
    /// [`HOP_SETUP_TIMEOUT`]: a stranger could otherwise hold it, and its
    /// share of the hops, for as long as its link stays up.
    fn watch_hop(&self, up: Link, born: Instant) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let node = self.clone();
        rt.spawn(async move {
            tokio::time::sleep(HOP_SETUP_TIMEOUT).await;
            let (stalled, depositing) = lock(&node.shared.onion)
                .hops
                .get(&up)
                .filter(|h| h.born == born && !h.in_use())
                .map_or((false, false), |h| (true, h.deposit.is_some()));
            if depositing {
                let declined = DepositStatus::Declined as u8;
                node.onion_reply(up, &Payload::new(Cmd::Deposited, vec![declined]));
            }
            if stalled {
                node.onion_send(&up.0, &OnionMsg::Destroy { circ: up.1 });
                node.onion_destroy(up, true);
            }
        });
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
            // The circuit went while the next hop was being made: that hop
            // goes too, or it would sit there taking up a circuit slot.
            drop(st);
            self.onion_send(&from, &OnionMsg::Destroy { circ });
            lock(&self.shared.onion).anon.remove(&(from, circ));
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
            Cmd::Extend => self.onion_extend(up, &p.data),
            Cmd::Begin => self.onion_endpoint(up),
            Cmd::Deposit => self.onion_deposit_start(up, &p.data),
            Cmd::Data => {
                let mut st = lock(&self.shared.onion);
                let Some(hop) = st.hops.get_mut(&up) else {
                    return;
                };
                if hop.deposit.is_some() {
                    let fits = st.deposit_room(&up, p.data.len());
                    let Some(hop) = st.hops.get_mut(&up) else {
                        return;
                    };
                    if fits && let Some(d) = &mut hop.deposit {
                        d.sealed.extend_from_slice(&p.data);
                    } else {
                        // No room for the rest: none of it is kept.
                        hop.deposit = None;
                        drop(st);
                        return self.onion_deposited(up, DepositStatus::Declined);
                    }
                    drop(st);
                    self.onion_deposit_check(up);
                } else if let Some(tx) = hop.endpoint.clone() {
                    drop(st);
                    let _ = tx.send(p.data);
                }
            }
            Cmd::End => self.onion_destroy(up, true),
            Cmd::Extended | Cmd::ExtendFailed | Cmd::Deposited => {}
        }
    }

    /// An EXTEND for us as a hop on `up`'s circuit: to a live, mutually
    /// approved contact (Appendix I), or, as a volunteer relay, to the
    /// address given (Appendix P).
    fn onion_extend(&self, up: Link, data: &[u8]) {
        let fail = move |n: &Self| n.onion_reply(up, &Payload::new(Cmd::ExtendFailed, vec![]));
        let Ok(req) = ExtendReq::decode(data) else {
            return fail(self);
        };
        let (allowed, from_contact) = {
            let st = lock(&self.shared.onion);
            // One extension at a time: a second would orphan the first.
            let extending = st.extending.values().any(|u| *u == up);
            st.hops.get(&up).map_or((false, false), |h| {
                (h.full && h.next.is_none() && !extending, h.contact)
            })
        };
        if !allowed {
            return fail(self);
        }
        let contact = from_contact
            .then(|| {
                self.sessions()
                    .into_iter()
                    .find(|s| s.via.is_none() && s.peer.fingerprint() == req.to && s.peer != up.0)
                    .map(|s| s.peer)
                    .filter(|p| self.shared.mutual(p))
            })
            .flatten();
        let c = u64::from_le_bytes(random_bytes());
        if let Some(next) = contact {
            lock(&self.shared.onion).extending.insert((next, c), up);
            let create = OnionMsg::Create {
                circ: c,
                e_pub: req.e_pub,
                token: req.token,
            };
            if !self.onion_send(&next, &create) {
                lock(&self.shared.onion).extending.remove(&(next, c));
                fail(self);
            }
            return;
        }
        let Some(addr) = req.addr.clone().filter(|_| self.volunteering()) else {
            return fail(self);
        };
        let node = self.clone();
        tokio::spawn(async move {
            let dialed = node.anon_dial(&addr, Some(req.to), false).await;
            let Ok(next) = dialed else {
                return fail(&node);
            };
            {
                let mut st = lock(&node.shared.onion);
                if !st.hops.contains_key(&up) {
                    return;
                }
                st.anon.insert((next, c));
                st.extending.insert((next, c), up);
            }
            let create = OnionMsg::Create {
                circ: c,
                e_pub: req.e_pub,
                token: req.token,
            };
            if !node.onion_send(&next, &create) {
                {
                    let mut st = lock(&node.shared.onion);
                    st.extending.remove(&(next, c));
                    st.anon.remove(&(next, c));
                }
                fail(&node);
            }
        });
    }

    /// We are the circuit's last hop and a mailbox: someone, hidden from us,
    /// leaves a sealed message for one of our contacts.
    fn onion_deposit_start(&self, up: Link, data: &[u8]) {
        let head = data.get(..DEPOSIT_HEAD).and_then(|h| {
            let to: [u8; 32] = h[..32].try_into().ok()?;
            let total = u32::from_be_bytes(h[32..].try_into().ok()?) as usize;
            (total <= crate::mailbox::MAX_HELD_BYTES).then_some((to, total))
        });
        // Only for our own mutually approved contacts: anything else would
        // be declined once complete, so it is declined before it is kept.
        let head = head
            .filter(|(to, _)| PublicIdentity::from_bytes(to).is_ok_and(|d| self.shared.mutual(&d)));
        let started = {
            let mut st = lock(&self.shared.onion);
            let hop = st.hops.get(&up);
            let free = hop
                .is_some_and(|h| h.next.is_none() && h.endpoint.is_none() && h.deposit.is_none());
            let net = hop.and_then(|h| h.net);
            let first = data.len().saturating_sub(DEPOSIT_HEAD);
            match head {
                Some((to, total))
                    if free
                        && st.deposit_room(&up, first)
                        && st.deposit_allowed(Carrier::of(&up, net)) =>
                {
                    if let Some(h) = st.hops.get_mut(&up) {
                        h.deposit = Some(PendingDeposit {
                            to,
                            total,
                            sealed: data[DEPOSIT_HEAD..].to_vec(),
                        });
                    }
                    true
                }
                _ => false,
            }
        };
        if started {
            self.onion_deposit_check(up);
        } else {
            self.onion_deposited(up, DepositStatus::Declined);
        }
    }

    /// Stores a deposit once all of it has arrived.
    fn onion_deposit_check(&self, up: Link) {
        let done = {
            let mut st = lock(&self.shared.onion);
            let Some(hop) = st.hops.get_mut(&up) else {
                return;
            };
            match &hop.deposit {
                Some(d) if d.sealed.len() >= d.total => hop.deposit.take(),
                _ => return,
            }
        };
        let Some(d) = done else { return };
        let status = if d.sealed.len() == d.total {
            // The depositor could be anyone: "delivered now" would tell
            // them the recipient is online. Held says all they need.
            match self.store_deposit(d.to, d.sealed) {
                DepositStatus::Delivered => DepositStatus::Held,
                s => s,
            }
        } else {
            DepositStatus::Declined
        };
        self.onion_deposited(up, status);
    }

    fn onion_deposited(&self, up: Link, status: DepositStatus) {
        self.onion_reply(up, &Payload::new(Cmd::Deposited, vec![status as u8]));
        self.onion_destroy(up, true);
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
                // A next hop still being made goes too.
                let making: Vec<Link> = st
                    .extending
                    .iter()
                    .filter(|(_, up)| **up == link)
                    .map(|(down, _)| *down)
                    .collect();
                for down in making {
                    st.extending.remove(&down);
                    notify.push(down);
                }
            } else if let Some(up) = st.down.remove(&link) {
                if let Some(hop) = st.hops.remove(&up) {
                    drop(hop);
                }
                notify.push(up);
            }
        }
        if propagate {
            for (peer, c) in &notify {
                self.onion_send(peer, &OnionMsg::Destroy { circ: *c });
            }
        }
        let mut st = lock(&self.shared.onion);
        st.anon.remove(&link);
        for l in notify {
            st.anon.remove(&l);
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

    /// Circuit `link` has more cells waiting on its anonymous link than the
    /// link can carry: it ends in both directions (its waiting cells are
    /// gone), and the link carries on for its other circuits.
    pub(crate) fn onion_overflowed(&self, link: Link) {
        self.onion_send_via(&link.0, &OnionMsg::Destroy { circ: link.1 }, true);
        self.onion_destroy(link, true);
    }

    /// Whether any circuit uses a link to `peer` (keeps anonymous links open).
    pub(crate) fn onion_busy(&self, peer: &PublicIdentity) -> bool {
        let st = lock(&self.shared.onion);
        st.hops
            .keys()
            .chain(st.down.keys())
            .chain(st.origins.keys())
            .chain(st.creating.keys())
            .chain(st.extending.keys())
            .any(|(p, _)| p == peer)
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
        self.connect_onion_since(dest, min_relays, 0).await
    }

    /// [`Node::connect_onion`], starting only on first hops begun or
    /// heard from after `since_ms` (see [`Node::first_hop_peers`]).
    pub(crate) async fn connect_onion_since(
        &self,
        dest: Fingerprint,
        min_relays: usize,
        since_ms: u64,
    ) -> Result<PublicIdentity> {
        if dest == self.identity().fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        for (first, path) in self.onion_paths_since(dest, &[], min_relays, since_ms) {
            if let Ok(peer) = self
                .try_onion_path(&OnionRoute::contacts(first, &path))
                .await
            {
                return Ok(peer);
            }
        }
        Err(NetError::NoRoute(format!(
            "{dest} (no onion path with {min_relays}+ relays among approved contacts)"
        )))
    }

    /// Candidate circuits to `dest`, each as its first hop (a live direct
    /// session) and the path: two relays among our mutually approved
    /// contacts, or one when `min_relays` allows. Nobody in `avoid` relays.
    pub(crate) fn onion_paths(
        &self,
        dest: Fingerprint,
        avoid: &[Fingerprint],
        min_relays: usize,
    ) -> Vec<(PublicIdentity, Vec<Fingerprint>)> {
        self.onion_paths_since(dest, avoid, min_relays, 0)
    }

    /// [`Node::onion_paths`] whose first hops were begun or heard from
    /// after `since_ms`, most recently heard from first.
    pub(crate) fn onion_paths_since(
        &self,
        dest: Fingerprint,
        avoid: &[Fingerprint],
        min_relays: usize,
        since_ms: u64,
    ) -> Vec<(PublicIdentity, Vec<Fingerprint>)> {
        let me = self.identity();
        let ok = |p: &PublicIdentity| {
            let fp = p.fingerprint();
            fp != dest && !avoid.contains(&fp) && *p != me
        };
        let firsts: Vec<PublicIdentity> = self
            .first_hop_peers(since_ms)
            .into_iter()
            .filter(|p| ok(p))
            .collect();
        let middles: Vec<PublicIdentity> = self
            .contacts()
            .iter()
            .filter(|c| c.mutually_approved() && ok(&c.key))
            .map(|c| c.key)
            .collect();
        let mut paths = Vec::new();
        for r1 in &firsts {
            for r2 in middles.iter().filter(|m| *m != r1) {
                paths.push((*r1, vec![r1.fingerprint(), r2.fingerprint(), dest]));
            }
        }
        if min_relays <= 1 {
            paths.extend(firsts.iter().map(|r1| (*r1, vec![r1.fingerprint(), dest])));
        }
        paths
    }

    /// Builds the circuit `route`. Returns the link circuit id, the hop
    /// keys and the backward cells.
    async fn build_circuit(
        &self,
        route: &OnionRoute,
    ) -> Result<(u64, OnionPath, mpsc::UnboundedReceiver<Cell>)> {
        let first = route.first;
        if let Some(addr) = &route.first_addr {
            // A fresh identity: the entry relay learns our address, not who we are.
            let peer = self
                .anon_dial(addr, Some(first.fingerprint()), true)
                .await?;
            if peer != first {
                return Err(NetError::NoRoute(first.fingerprint().to_string()));
            }
        }
        let step = if route.anon() {
            VOLUNTEER_STEP_TIMEOUT
        } else {
            STEP_TIMEOUT
        };
        let pay = |hop: &Hop, e_pub: &[u8]| -> Result<Option<Vec<u8>>> {
            match &hop.volunteer {
                None => Ok(None),
                Some(r) => self
                    .mint_token(r, e_pub)
                    .map(Some)
                    .ok_or_else(|| NetError::NoRoute(format!("{} (no relay token left)", hop.fp))),
            }
        };
        let circ = u64::from_le_bytes(random_bytes());
        let link = (first, circ);
        let (tx, mut rx) = mpsc::unbounded_channel::<Cell>();
        let result = async {
            let head = route.hops.first().ok_or(NetError::Closed)?;
            // First hop: CREATE over the link.
            let (st, e_pub) = CreateState::new(head.fp);
            let token = pay(head, &e_pub)?;
            let (ctx, crx) = oneshot::channel();
            {
                let mut s = lock(&self.shared.onion);
                s.origins.insert(link, tx);
                s.creating.insert(link, ctx);
                if route.anon() {
                    s.anon.insert(link);
                }
            }
            if !self.onion_send(&first, &OnionMsg::Create { circ, e_pub, token }) {
                return Err(NetError::Closed);
            }
            let (id, ct, sig) = tokio::time::timeout(step, crx)
                .await
                .map_err(|_| NetError::Timeout)?
                .map_err(|_| NetError::Closed)?;
            let mut onion = OnionPath::default();
            onion.push(st.finish(&id, &ct, &sig)?);

            // Every further hop: EXTEND through the circuit.
            for hop in &route.hops[1..] {
                let (st, e_pub) = CreateState::new(hop.fp);
                let token = pay(hop, &e_pub)?;
                let data = ExtendReq {
                    to: hop.fp,
                    e_pub,
                    addr: hop.addr.clone(),
                    token,
                }
                .encode()?;
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
                let back = tokio::time::timeout(step * 2, rx.recv())
                    .await
                    .map_err(|_| NetError::Timeout)?
                    .ok_or(NetError::Closed)?;
                let (at, p) = onion.unwrap(back)?;
                if at != onion.len() - 1 || p.cmd != Cmd::Extended || p.data.len() != 32 + 1120 + 64
                {
                    return Err(NetError::NoRoute(hop.fp.to_string()));
                }
                let id: [u8; 32] = p.data[..32].try_into().map_err(|_| NetError::Closed)?;
                let sig: [u8; 64] = p.data[32 + 1120..]
                    .try_into()
                    .map_err(|_| NetError::Closed)?;
                onion.push(st.finish(&id, &p.data[32..32 + 1120], &sig)?);
            }
            Ok(onion)
        }
        .await;
        match result {
            Ok(onion) => Ok((circ, onion, rx)),
            Err(e) => {
                self.onion_send(&first, &OnionMsg::Destroy { circ });
                self.onion_destroy(link, false);
                Err(e)
            }
        }
    }

    /// Leaves `sealed` for `to` in the mailbox at the end of `path`, which
    /// sees the circuit's last relay rather than us.
    async fn try_onion_deposit(
        &self,
        route: &OnionRoute,
        to: &PublicIdentity,
        sealed: &[u8],
    ) -> Result<DepositStatus> {
        let first = route.first;
        let (circ, mut onion, mut rx) = self.build_circuit(route).await?;
        let result = async {
            let last = onion.len() - 1;
            let mut head = to.as_bytes().to_vec();
            head.extend_from_slice(
                &u32::try_from(sealed.len())
                    .map_err(|_| NetError::Closed)?
                    .to_be_bytes(),
            );
            let split = sealed.len().min(MAX_DATA - DEPOSIT_HEAD);
            head.extend_from_slice(&sealed[..split]);
            let cells = std::iter::once(Payload::new(Cmd::Deposit, head)).chain(
                sealed[split..]
                    .chunks(MAX_DATA)
                    .map(|c| Payload::new(Cmd::Data, c.to_vec())),
            );
            for p in cells {
                let cell = onion.wrap(last, &p)?;
                if !self.onion_send(
                    &first,
                    &OnionMsg::Cell {
                        circ,
                        cell: cell.to_vec(),
                    },
                ) {
                    return Err(NetError::Closed);
                }
            }
            tokio::time::timeout(DEPOSIT_TIMEOUT, async {
                while let Some(back) = rx.recv().await {
                    if let Ok((h, p)) = onion.unwrap(back)
                        && h == last
                        && p.cmd == Cmd::Deposited
                        && let Some(status) =
                            p.data.first().and_then(|b| DepositStatus::from_wire(*b))
                    {
                        return Ok(status);
                    }
                }
                Err(NetError::Closed)
            })
            .await
            .map_err(|_| NetError::Timeout)?
        }
        .await;
        self.onion_send(&first, &OnionMsg::Destroy { circ });
        self.onion_destroy((first, circ), false);
        result
    }

    /// Tries anonymous deposits with mailbox `mailbox` until one gets an
    /// answer.
    pub(crate) async fn onion_deposit(
        &self,
        mailbox: &PublicIdentity,
        to: &PublicIdentity,
        sealed: &[u8],
    ) -> Result<DepositStatus> {
        let mut routes: Vec<OnionRoute> = self
            .onion_paths(mailbox.fingerprint(), &[to.fingerprint()], 2)
            .into_iter()
            .map(|(first, path)| OnionRoute::contacts(first, &path))
            .collect();
        // Through volunteers when contacts can't make a path.
        if let Some(addr) = self
            .contacts()
            .get(mailbox)
            .and_then(|c| c.last_addr.clone())
        {
            routes.extend(self.volunteer_routes(mailbox.fingerprint(), &addr, &[to.fingerprint()]));
        }
        for route in routes {
            if let Ok(status) = self.try_onion_deposit(&route, to, sealed).await {
                return Ok(status);
            }
        }
        Err(NetError::NoRoute(format!(
            "{} (no onion path to this mailbox)",
            mailbox.fingerprint()
        )))
    }

    async fn try_onion_path(&self, route: &OnionRoute) -> Result<PublicIdentity> {
        let first = route.first;
        let (circ, mut onion, mut rx) = self.build_circuit(route).await?;
        let link = (first, circ);
        let last = onion.len() - 1;
        let begun = onion
            .wrap(last, &Payload::new(Cmd::Begin, vec![]))
            .ok()
            .is_some_and(|begin| {
                self.onion_send(
                    &first,
                    &OnionMsg::Cell {
                        circ,
                        cell: begin.to_vec(),
                    },
                )
            });
        if !begun {
            self.onion_send(&first, &OnionMsg::Destroy { circ });
            self.onion_destroy(link, false);
            return Err(NetError::Closed);
        }

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
            node.onion_send(&first, &OnionMsg::Destroy { circ });
            node.onion_destroy(link, false);
        });

        let mut stream = theirs;
        let dest = route.hops.last().ok_or(NetError::Closed)?.fp;
        let chan = handshake::initiate(&mut stream, self.identity_ref(), Some(dest)).await?;
        let peer = *chan.peer();
        self.spawn_onion_session(stream, chan, first, true);
        Ok(peer)
    }
}

impl Node {
    /// Circuits to `dest` (at `addr`) through two volunteer relays, entered
    /// over an anonymous link, each hop paid with a token. Nobody in
    /// `avoid` relays; the two relays are on different networks when
    /// possible.
    pub(crate) fn volunteer_routes(
        &self,
        dest: Fingerprint,
        addr: &str,
        avoid: &[Fingerprint],
    ) -> Vec<OnionRoute> {
        if !self.use_volunteers() {
            return Vec::new();
        }
        let me = self.identity();
        let mut relays: Vec<_> = self
            .volunteer_relays()
            .into_iter()
            .filter(|r| {
                let fp = r.identity.fingerprint();
                r.identity != me && fp != dest && !avoid.contains(&fp) && self.can_pay(&r.identity)
            })
            .collect();
        // A random order, so circuits spread over the relays.
        relays.sort_by_key(|_| u64::from_le_bytes(random_bytes()));
        let net = |r: &threnody_core::directory::RelayDescriptor| -> String {
            let host = r.addrs[0]
                .rsplit_once(':')
                .map_or(r.addrs[0].as_str(), |(h, _)| h);
            host.trim_matches(['[', ']'])
                .split(['.', ':'])
                .take(2)
                .collect::<Vec<_>>()
                .join(".")
        };
        let mut pairs = Vec::new();
        for (i, a) in relays.iter().enumerate() {
            for b in relays.iter().skip(i + 1) {
                pairs.push((net(a) == net(b), a.clone(), b.clone()));
            }
        }
        // Different networks first.
        pairs.sort_by_key(|(same, _, _)| *same);
        pairs
            .into_iter()
            .take(4)
            .map(|(_, r1, r2)| OnionRoute {
                first: r1.identity,
                first_addr: Some(r1.addrs[0].clone()),
                hops: vec![
                    Hop {
                        fp: r1.identity.fingerprint(),
                        addr: None,
                        volunteer: Some(r1.identity),
                    },
                    Hop {
                        fp: r2.identity.fingerprint(),
                        addr: Some(r2.addrs[0].clone()),
                        volunteer: Some(r2.identity),
                    },
                    Hop {
                        fp: dest,
                        addr: Some(addr.to_owned()),
                        volunteer: None,
                    },
                ],
            })
            .collect()
    }

    /// Opens a session with `dest`, at `addr`, through two volunteer relays.
    pub async fn connect_volunteer(&self, dest: Fingerprint, addr: &str) -> Result<PublicIdentity> {
        if dest == self.identity().fingerprint() {
            return Err(NetError::NoRoute("ourselves".into()));
        }
        for route in self.volunteer_routes(dest, addr, &[]) {
            if let Ok(peer) = self.try_onion_path(&route).await {
                return Ok(peer);
            }
        }
        Err(NetError::NoRoute(format!(
            "{dest} (no volunteer relays with tokens; subscribe to a directory)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceptPolicy, NodeConfig};
    use threnody_core::store::Home;

    fn test_node(dir: &tempfile::TempDir, name: &str) -> Node {
        let home = Home::new(dir.path().join(name));
        let identity = home.create_identity(None).unwrap();
        Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap()
        .0
    }

    async fn listening(dir: &tempfile::TempDir, name: &str) -> (Node, Events, String) {
        let home = Home::new(dir.path().join(name));
        let identity = home.create_identity(None).unwrap();
        let (node, rx) = Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap();
        let addr = node.listen("127.0.0.1:0").await.unwrap().to_string();
        (node, rx, addr)
    }

    type Events = mpsc::UnboundedReceiver<crate::Event>;

    async fn mutual(x: &Node, xrx: &mut Events, y: &Node, yrx: &mut Events, yaddr: &str) {
        let yid = x.connect(yaddr, None).await.unwrap();
        x.set_approval(&yid, true).unwrap();
        y.set_approval(&x.identity(), true).unwrap();
        for rx in [xrx, yrx] {
            tokio::time::timeout(Duration::from_secs(20), async {
                while !matches!(
                    rx.recv().await,
                    Some(crate::Event::ApprovalChanged { mutual: true, .. }) | None
                ) {}
            })
            .await
            .unwrap();
        }
    }

    /// A hop on `node` for a circuit from `from`, as if it had been created.
    fn fake_hop(node: &Node, from: PublicIdentity, circ: u64, contact: bool, paid: bool) -> Link {
        let (_, e_pub) = CreateState::new(node.identity().fingerprint());
        let (keys, _, _) = onion::respond(node.identity_ref(), &e_pub).unwrap();
        let mut st = lock(&node.shared.onion);
        if !contact {
            st.anon.insert((from, circ));
        }
        st.hops
            .insert((from, circ), RelayHop::new(keys, contact, paid, None));
        (from, circ)
    }

    fn extend_to(to: Fingerprint) -> Vec<u8> {
        ExtendReq {
            to,
            e_pub: CreateState::new(to).1,
            addr: None,
            token: None,
        }
        .encode()
        .unwrap()
    }

    #[tokio::test]
    async fn paid_strangers_never_reach_the_relays_contacts() {
        let dir = tempfile::tempdir().unwrap();
        let (v, mut vrx, _) = listening(&dir, "v").await;
        let (x, mut xrx, xaddr) = listening(&dir, "x").await;
        mutual(&v, &mut vrx, &x, &mut xrx, &xaddr).await;
        let xfp = x.identity().fingerprint();
        // A stranger's hop, paid with a token: an EXTEND naming the
        // relay's contact, with no address, gets nowhere.
        let stranger = threnody_core::Identity::generate().public();
        let up = fake_hop(&v, stranger, 1, false, true);
        v.onion_extend(up, &extend_to(xfp));
        assert!(lock(&v.shared.onion).extending.is_empty());
        // A contact's circuit still goes on to the relay's contacts.
        let friend = threnody_core::Identity::generate().public();
        let up = fake_hop(&v, friend, 2, true, false);
        v.onion_extend(up, &extend_to(xfp));
        assert_eq!(lock(&v.shared.onion).extending.len(), 1);
    }

    async fn eventually(what: &str, f: impl Fn() -> bool) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("never happened: {what}");
    }

    #[tokio::test]
    async fn no_hop_is_left_behind_downstream() {
        let dir = tempfile::tempdir().unwrap();
        let (v, mut vrx, _) = listening(&dir, "v").await;
        let (x, mut xrx, xaddr) = listening(&dir, "x").await;
        mutual(&v, &mut vrx, &x, &mut xrx, &xaddr).await;
        let xfp = x.identity().fingerprint();
        let friend = threnody_core::Identity::generate().public();
        // EXTEND, then DESTROY before X's CREATED is back.
        let up = fake_hop(&v, friend, 1, true, false);
        v.onion_extend(up, &extend_to(xfp));
        v.onion_destroy(up, true);
        tokio::time::sleep(Duration::from_millis(300)).await;
        eventually("X drops the hop", || x.onion_hops() == 0).await;
        // One EXTEND at a time: a second one would orphan the first.
        let up = fake_hop(&v, friend, 2, true, false);
        v.onion_extend(up, &extend_to(xfp));
        v.onion_extend(up, &extend_to(xfp));
        assert_eq!(lock(&v.shared.onion).extending.len(), 1);
        eventually("the circuit reaches X", || {
            lock(&v.shared.onion)
                .hops
                .get(&up)
                .is_some_and(|h| h.next.is_some())
        })
        .await;
        v.onion_destroy(up, true);
        eventually("X drops it with the circuit", || x.onion_hops() == 0).await;
    }

    fn create(circ: u64, e_pub: Vec<u8>) -> Vec<u8> {
        OnionMsg::Create {
            circ,
            e_pub,
            token: None,
        }
        .encode()
        .unwrap()
    }

    #[tokio::test]
    async fn a_refused_stranger_create_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(&dir, "n");
        let stranger = threnody_core::Identity::generate().public();
        // Admitted, but the key is junk, so the hop is never made.
        for circ in 0..8 {
            node.on_onion_from(stranger, &create(circ, vec![1; 8]), true);
        }
        let st = lock(&node.shared.onion);
        assert!(st.hops.is_empty());
        assert!(st.anon.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn hops_that_go_nowhere_are_torn_down() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(&dir, "n");
        let stranger = threnody_core::Identity::generate().public();
        let (_, e_pub) = CreateState::new(node.identity().fingerprint());
        node.on_onion_from(stranger, &create(1, e_pub), true);
        assert_eq!(node.onion_hops(), 1);
        tokio::time::sleep(HOP_SETUP_TIMEOUT - Duration::from_secs(1)).await;
        assert_eq!(node.onion_hops(), 1);
        // Never extended, began a stream or finished a deposit.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(node.onion_hops(), 0);
        assert!(lock(&node.shared.onion).anon.is_empty());
    }

    #[test]
    fn one_network_holds_only_part_of_the_stranger_hops() {
        let me = threnody_core::Identity::generate();
        let hop = || {
            let (_, e_pub) = CreateState::new(me.public().fingerprint());
            let (keys, _, _) = onion::respond(&me, &e_pub).unwrap();
            keys
        };
        let mut st = OnionState::default();
        let (here, there) = ([1; 16], [2; 16]);
        for i in 0..MAX_STRANGER_HOPS_PER_NETWORK {
            assert!(st.stranger_room(Some(here)));
            let link = (threnody_core::Identity::generate().public(), i as u64);
            st.anon.insert(link);
            st.hops
                .insert(link, RelayHop::new(hop(), false, false, Some(here)));
        }
        assert!(!st.stranger_room(Some(here)));
        assert!(st.stranger_room(Some(there)));
    }

    #[test]
    fn anonymous_deposits_are_rate_limited_per_neighbour() {
        let mut st = OnionState::default();
        let (x, y) = (
            Carrier::Peer(threnody_core::Identity::generate().public()),
            Carrier::Peer(threnody_core::Identity::generate().public()),
        );
        for _ in 0..MAX_DEPOSITS_PER_MINUTE {
            assert!(st.deposit_allowed(x));
        }
        assert!(!st.deposit_allowed(x));
        assert!(st.deposit_allowed(y));
        // Over anonymous links identities are free: the network counts.
        let stranger = |n| {
            let link = (threnody_core::Identity::generate().public(), 1);
            Carrier::of(&link, Some([n; 16]))
        };
        for _ in 0..MAX_DEPOSITS_PER_MINUTE {
            assert!(st.deposit_allowed(stranger(1)));
        }
        assert!(!st.deposit_allowed(stranger(1)));
        assert!(st.deposit_allowed(stranger(2)));
    }

    #[tokio::test]
    async fn deposits_still_arriving_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let node = test_node(&dir, "m");
        let contact = threnody_core::Identity::generate().public();
        node.set_approval(&contact, true).unwrap();
        lock(&node.shared.contacts)
            .get_mut(&contact)
            .unwrap()
            .remote_approved = true;
        let deposit = |to: &PublicIdentity, total: usize| {
            let mut d = to.as_bytes().to_vec();
            d.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
            d.extend_from_slice(&[0; 100]);
            d
        };
        let pending = |node: &Node| {
            lock(&node.shared.onion)
                .hops
                .values()
                .filter(|h| h.deposit.is_some())
                .count()
        };
        // Not our contact: declined at once, nothing buffered.
        let stranger = threnody_core::Identity::generate().public();
        let up = fake_hop(&node, stranger, 1, false, false);
        let nobody = threnody_core::Identity::generate().public();
        node.onion_deposit_start(up, &deposit(&nobody, 4096));
        assert_eq!(pending(&node), 0);
        // What counts is what has arrived, not what a head claims: one
        // network may start several deposits.
        let max = crate::mailbox::MAX_HELD_BYTES;
        let hop_from = |n: u8, circ| {
            let from = threnody_core::Identity::generate().public();
            let up = fake_hop(&node, from, circ, false, false);
            lock(&node.shared.onion).hops.get_mut(&up).unwrap().net = Some([n; 16]);
            up
        };
        let data = |up: Link, len: usize| {
            node.onion_recognized(up, Payload::new(Cmd::Data, vec![0; len]));
        };
        let a = hop_from(1, 2);
        node.onion_deposit_start(a, &deposit(&contact, max));
        let b = hop_from(1, 3);
        node.onion_deposit_start(b, &deposit(&contact, max));
        assert_eq!(pending(&node), 2);
        // But together they may hold only so much from that network.
        data(a, max / 2);
        data(b, max / 2 - 200);
        assert_eq!(pending(&node), 2);
        data(b, 1);
        assert_eq!(pending(&node), 1, "the deposit that went over is declined");
        // And all strangers' deposits together only so much.
        let nets = MAX_PENDING_DEPOSIT_BYTES / max;
        for n in 2..=nets as u8 {
            let up = hop_from(n, 10 + u64::from(n));
            node.onion_deposit_start(up, &deposit(&contact, max));
            data(up, max - 200);
        }
        assert_eq!(pending(&node), nets);
        let late = hop_from(100, 200);
        node.onion_deposit_start(late, &deposit(&contact, max));
        assert_eq!(pending(&node), nets + 1);
        // Its network has room, but all of them together do not.
        data(late, max - 200);
        assert_eq!(pending(&node), nets);
        // Deposits over our contacts' circuits have their own room.
        let up = fake_hop(
            &node,
            threnody_core::Identity::generate().public(),
            300,
            true,
            false,
        );
        node.onion_deposit_start(up, &deposit(&contact, max));
        data(up, max - 200);
        assert_eq!(pending(&node), nets + 1);
    }

    #[test]
    fn onion_messages_round_trip() {
        for m in [
            OnionMsg::Create {
                circ: 1,
                e_pub: vec![1; 8],
                token: None,
            },
            OnionMsg::Create {
                circ: 9,
                e_pub: vec![1; 8],
                token: Some(vec![2; 300]),
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
