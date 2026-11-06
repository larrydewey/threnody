//! Anonymous links (Appendix P).
//!
//! A link to a volunteer relay or a directory, or from one, that is not a
//! contact session: nobody becomes a contact, the app hears nothing, and
//! only onion cells and directory messages travel over it. The dialing side
//! usually presents a fresh identity, so the far end learns an address but
//! not who is dialing.
//!
//! The initiator's first frame is [`MAGIC`] instead of the handshake's
//! first message; then the usual handshake and an encrypted session follow
//! (cover traffic included, as configured for the node).

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Duration;

use threnody_core::message::FEATURES;
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, SecureChannel};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::error::{NetError, Result};
use crate::frame::write_frame;
use crate::gate::{Gate, Pass, Source};
use crate::handshake;
use crate::node::{Inbound, Node, lock, read_inbound};

/// The first frame of an anonymous link.
pub(crate) const MAGIC: &[u8] = b"threnody anonymous link v1";
/// Inbound anonymous links held at once, and from one address, counted
/// from the moment one announces itself (handshakes in progress too).
const MAX_INBOUND: usize = 1024;
const MAX_PER_ADDRESS: usize = 16;
/// An outbound link nothing has used for this long is closed.
const IDLE: Duration = Duration::from_secs(120);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Messages waiting for cover ticks on one link, at most: a link this far
/// behind is useless.
const MAX_QUEUED: usize = 4096;
/// Cells of one circuit waiting on a link, at most (2 MB). A volunteer's
/// links carry many people's circuits, so the circuit filling one is what
/// ends, not the link.
const MAX_QUEUED_PER_CIRCUIT: usize = 1024;

/// Messages waiting for cover ticks on an anonymous link, counted by onion
/// circuit.
#[derive(Default)]
struct Backlog {
    queue: VecDeque<(Option<u64>, AppMessage)>,
    per_circ: HashMap<u64, usize>,
}

impl Backlog {
    /// Queues `m`. When that puts one circuit, or the whole link, too far
    /// behind, the circuit with the most cells waiting is dropped from the
    /// queue (with `m`, if it is that circuit's) and returned for teardown;
    /// `Err` when only messages of no circuit fill the link.
    fn push(&mut self, m: AppMessage) -> std::result::Result<Option<u64>, ()> {
        let circ = match &m {
            AppMessage::Onion(p) => crate::onion::circ_of(p),
            _ => None,
        };
        let full =
            circ.filter(|c| self.per_circ.get(c).copied().unwrap_or(0) >= MAX_QUEUED_PER_CIRCUIT);
        let drop = match full {
            Some(c) => Some(c),
            None if self.queue.len() >= MAX_QUEUED => {
                let most = self.per_circ.iter().max_by_key(|(_, n)| **n);
                Some(most.map(|(c, _)| *c).ok_or(())?)
            }
            None => None,
        };
        if let Some(d) = drop {
            self.per_circ.remove(&d);
            self.queue.retain(|(c, _)| *c != Some(d));
        }
        if drop.is_none() || circ != drop {
            if let Some(c) = circ {
                *self.per_circ.entry(c).or_default() += 1;
            }
            self.queue.push_back((circ, m));
        }
        Ok(drop)
    }

    fn pop(&mut self) -> Option<AppMessage> {
        let (circ, m) = self.queue.pop_front()?;
        if let Some(c) = circ
            && let Some(n) = self.per_circ.get_mut(&c)
        {
            *n -= 1;
            if *n == 0 {
                self.per_circ.remove(&c);
            }
        }
        Some(m)
    }
}

struct Handle {
    id: u64,
    tx: mpsc::UnboundedSender<AppMessage>,
    addr: SocketAddr,
    outbound: bool,
}

pub(crate) struct AnonState {
    links: HashMap<PublicIdentity, Handle>,
    next_id: u64,
    /// Places for inbound links; each link's task holds its own.
    inbound: Gate,
}

impl Default for AnonState {
    fn default() -> Self {
        Self {
            links: HashMap::new(),
            next_id: 0,
            inbound: Gate::new(MAX_INBOUND, MAX_PER_ADDRESS),
        }
    }
}

impl Node {
    /// Dials an anonymous link to `addr`. With `expect`, the far end must
    /// have that fingerprint; with `ephemeral`, we present a fresh identity
    /// (else our own, as a relay registering with a directory does).
    /// Reuses a live outbound link to the expected peer.
    pub(crate) async fn anon_dial(
        &self,
        addr: &str,
        expect: Option<Fingerprint>,
        ephemeral: bool,
    ) -> Result<PublicIdentity> {
        if let Some(fp) = expect
            && let Some(peer) = lock(&self.shared.anon)
                .links
                .iter()
                .find(|(p, h)| h.outbound && p.fingerprint() == fp)
                .map(|(p, _)| *p)
        {
            return Ok(peer);
        }
        let mut stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| NetError::Timeout)??;
        let _ = stream.set_nodelay(true);
        let remote = stream.peer_addr()?;
        write_frame(&mut stream, MAGIC).await?;
        let me = if ephemeral {
            Identity::generate()
        } else {
            Identity::from_seed(&self.identity_ref().seed())
        };
        let chan = handshake::initiate(&mut stream, &me, expect).await?;
        let peer = *chan.peer();
        self.spawn_anon(stream, chan, remote, true, None);
        Ok(peer)
    }

    /// Accepts an anonymous link whose [`MAGIC`] frame was already read.
    pub(crate) async fn accept_anon<S>(&self, mut stream: S, addr: SocketAddr) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        // The place is taken before the handshake: counting only links
        // already up let any number through at once.
        let pass = lock(&self.shared.anon)
            .inbound
            .enter(Source::ip(addr.ip()))
            .ok_or_else(|| NetError::Refused("too many anonymous links".into()))?;
        let chan = handshake::accept(&mut stream, self.identity_ref()).await?;
        self.spawn_anon(stream, chan, addr, false, Some(pass));
        Ok(())
    }

    /// Queues a message on the anonymous link to `to`; false if there is none.
    pub(crate) fn anon_send(&self, to: &PublicIdentity, msg: AppMessage) -> bool {
        lock(&self.shared.anon)
            .links
            .get(to)
            .is_some_and(|h| h.tx.send(msg).is_ok())
    }

    /// Where the far end of an anonymous link is (for per-address limits).
    pub(crate) fn anon_addr(&self, to: &PublicIdentity) -> Option<SocketAddr> {
        lock(&self.shared.anon).links.get(to).map(|h| h.addr)
    }

    /// Tests only: an inbound anonymous link from `peer` at `addr` whose
    /// outgoing messages land in the returned receiver.
    #[cfg(test)]
    pub(crate) fn fake_anon_link(
        &self,
        peer: PublicIdentity,
        addr: SocketAddr,
    ) -> mpsc::UnboundedReceiver<AppMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut st = lock(&self.shared.anon);
        st.next_id += 1;
        let id = st.next_id;
        st.links.insert(
            peer,
            Handle {
                id,
                tx,
                addr,
                outbound: false,
            },
        );
        rx
    }

    /// Number of anonymous links open (both directions).
    pub fn anonymous_links(&self) -> usize {
        lock(&self.shared.anon).links.len()
    }

    /// Runs a link; an inbound one holds `pass` until it ends.
    fn spawn_anon<S>(
        &self,
        stream: S,
        chan: SecureChannel,
        addr: SocketAddr,
        outbound: bool,
        pass: Option<Pass>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let peer = *chan.peer();
        let (tx, rx) = mpsc::unbounded_channel();
        let id = {
            let mut st = lock(&self.shared.anon);
            st.next_id += 1;
            let id = st.next_id;
            st.links.insert(
                peer,
                Handle {
                    id,
                    tx,
                    addr,
                    outbound,
                },
            );
            id
        };
        let node = self.clone();
        tokio::spawn(async move {
            let _pass = pass;
            let _ = run_anon(&node, stream, chan, outbound, rx).await;
            {
                let mut st = lock(&node.shared.anon);
                if st.links.get(&peer).is_some_and(|h| h.id == id) {
                    st.links.remove(&peer);
                }
            }
            node.drop_onion_circuits_of(&peer);
            node.dir_link_closed(&peer);
        });
    }
}

async fn run_anon<S>(
    node: &Node,
    stream: S,
    mut chan: SecureChannel,
    outbound: bool,
    mut outbox: mpsc::UnboundedReceiver<AppMessage>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = *chan.peer();
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (in_tx, mut inbox) = mpsc::channel::<Result<Inbound>>(16);
    let shared = node.shared.clone();
    let reader = tokio::spawn(async move {
        loop {
            // Nobody on an anonymous link is trusted with memory.
            let r = read_inbound(&shared, &mut rd, || false).await;
            let stop = !matches!(r, Ok(Some(_)));
            let item = match r {
                Ok(Some(f)) => Ok(f),
                Ok(None) => Err(NetError::Closed),
                Err(e) => Err(e),
            };
            if in_tx.send(item).await.is_err() || stop {
                break;
            }
        }
    });
    let mut pending = Backlog::default();
    // Opens the ratchet; nothing about us beyond what every node supports:
    // the full FEATURES, even where the node withholds some from contacts,
    // so anonymous links can't tell builds apart.
    let _ = pending.push(AppMessage::Hello { features: FEATURES });
    let mut rate = node.shared.constant_rate.subscribe();
    let make_ticker = |r: Option<Duration>| {
        r.map(|d| {
            let mut t = tokio::time::interval(d);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t
        })
    };
    let mut ticker = make_ticker(*rate.borrow_and_update());
    let mut idle_check = tokio::time::interval(Duration::from_secs(15));
    let mut last_use = tokio::time::Instant::now();
    let result: Result<()> = async {
        loop {
            if ticker.is_none() && chan.can_send() {
                while let Some(m) = pending.pop() {
                    write_frame(&mut wr, &chan.seal(&m)?).await?;
                }
            }
            tokio::select! {
                frame = inbox.recv() => {
                    let frame = match frame {
                        Some(Ok(f)) => f,
                        Some(Err(NetError::Closed)) | None => return Ok(()),
                        Some(Err(e)) => return Err(e),
                    };
                    match chan.open(&frame.bytes)? {
                        AppMessage::Onion(p) => {
                            last_use = tokio::time::Instant::now();
                            node.on_onion_from(peer, &p, true);
                        }
                        AppMessage::Directory(p) => {
                            last_use = tokio::time::Instant::now();
                            node.on_directory(peer, &p);
                        }
                        // Nothing else belongs on an anonymous link.
                        _ => {}
                    }
                }
                out = outbox.recv() => match out {
                    Some(m) => {
                        last_use = tokio::time::Instant::now();
                        match pending.push(m) {
                            Ok(None) => {}
                            Ok(Some(circ)) => node.onion_overflowed((peer, circ)),
                            Err(()) => {
                                return Err(NetError::Refused("anonymous link too far behind".into()));
                            }
                        }
                    }
                    None => return Ok(()),
                },
                Ok(()) = rate.changed() => ticker = make_ticker(*rate.borrow_and_update()),
                () = tick(&mut ticker) => {
                    if chan.can_send() {
                        let m = pending.pop().unwrap_or(AppMessage::Cover);
                        write_frame(&mut wr, &crate::node::seal_link(&mut chan, &m, true)?).await?;
                    }
                }
                _ = idle_check.tick() => {
                    let busy = node.onion_busy(&peer) || node.dir_busy(&peer);
                    if busy {
                        last_use = tokio::time::Instant::now();
                    } else if outbound && last_use.elapsed() > IDLE {
                        return Ok(());
                    } else if !outbound && last_use.elapsed() > IDLE * 5 {
                        // An inbound link nobody uses holds a slot for nothing.
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    reader.abort();
    result
}

async fn tick(t: &mut Option<tokio::time::Interval>) {
    match t {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{AcceptPolicy, NodeConfig};
    use threnody_core::store::Home;

    fn test_node(dir: &tempfile::TempDir, name: &str) -> Node {
        let home = Home::new(dir.path().join(name));
        let identity = home.create_identity(None).unwrap();
        Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: Some(Duration::from_secs(10)),
            tunnel_port: None,
        })
        .unwrap()
        .0
    }

    fn cell(circ: u64) -> AppMessage {
        AppMessage::Onion(crate::onion::test_destroy(circ))
    }

    #[test]
    fn a_backlog_drops_the_circuit_that_fills_it() {
        let mut q = Backlog::default();
        let circ_count = |q: &Backlog, c| q.queue.iter().filter(|(x, _)| *x == Some(c)).count();
        assert_eq!(q.push(cell(2)), Ok(None));
        for _ in 0..MAX_QUEUED_PER_CIRCUIT {
            assert_eq!(q.push(cell(1)), Ok(None));
        }
        // One cell too many: all of that circuit's go, and only those.
        assert_eq!(q.push(cell(1)), Ok(Some(1)));
        assert_eq!(circ_count(&q, 1), 0);
        assert_eq!(circ_count(&q, 2), 1);
        assert_eq!(q.push(cell(1)), Ok(None));
        // Many circuits, none over its own limit, fill the link: the one
        // with the most waiting goes, and the newcomer still gets in.
        let mut q = Backlog::default();
        for i in 0..MAX_QUEUED {
            let c = if i < MAX_QUEUED_PER_CIRCUIT {
                7
            } else {
                100 + i as u64 % 8
            };
            assert_eq!(q.push(cell(c)), Ok(None));
        }
        assert_eq!(q.push(cell(3)), Ok(Some(7)));
        assert_eq!(circ_count(&q, 3), 1);
        assert_eq!(q.queue.len(), MAX_QUEUED - MAX_QUEUED_PER_CIRCUIT + 1);
        // Taking a message out counts it out.
        while q.pop().is_some() {}
        assert!(q.per_circ.is_empty());
        // Only messages of no circuit: then the link itself is too far behind.
        let mut q = Backlog::default();
        for _ in 0..MAX_QUEUED {
            assert_eq!(q.push(AppMessage::Cover), Ok(None));
        }
        assert_eq!(q.push(AppMessage::Cover), Err(()));
    }

    #[tokio::test(start_paused = true)]
    async fn one_circuit_filling_a_link_is_dropped_not_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let (x, y) = (test_node(&dir, "x"), test_node(&dir, "y"));
        let (a, b) = tokio::io::duplex(1 << 20);
        let ((a, ca), (b, cb)) = tokio::join!(
            async {
                let mut a = a;
                let ch = handshake::initiate(&mut a, &Identity::generate(), None).await;
                (a, ch.unwrap())
            },
            async {
                let mut b = b;
                let ch = handshake::accept(&mut b, y.identity_ref()).await;
                (b, ch.unwrap())
            },
        );
        let addr: SocketAddr = "192.0.2.1:7450".parse().unwrap();
        x.spawn_anon(a, ca, addr, true, None);
        y.spawn_anon(b, cb, addr, false, None);
        // One circuit on a slow link (a cell every 10 s) sends far more
        // than the link can carry; another sends a cell.
        let to = y.identity();
        for _ in 0..MAX_QUEUED + 10 {
            assert!(x.anon_send(&to, cell(1)));
        }
        assert!(x.anon_send(&to, cell(2)));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            x.anonymous_links(),
            1,
            "the link stays for its other circuits"
        );
    }

    /// Links whose handshakes are still running hold their places, so a
    /// burst from one address can't all slip in before any is counted.
    #[tokio::test]
    async fn links_in_their_handshake_count_toward_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::new(dir.path().join("n"));
        let identity = home.create_identity(None).unwrap();
        let (node, _rx) = Node::new(NodeConfig {
            home,
            identity,
            policy: AcceptPolicy::Anyone,
            constant_rate: None,
            tunnel_port: None,
        })
        .unwrap();
        let from: SocketAddr = "192.0.2.7:4000".parse().unwrap();
        let mut ends = Vec::new();
        for _ in 0..MAX_PER_ADDRESS {
            let (x, y) = tokio::io::duplex(1 << 16);
            ends.push(x);
            let n = node.clone();
            tokio::spawn(async move { n.accept_anon(y, from).await });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_x, y) = tokio::io::duplex(1 << 16);
        let r = tokio::time::timeout(Duration::from_secs(1), node.accept_anon(y, from))
            .await
            .expect("refused at once");
        assert!(matches!(r, Err(NetError::Refused(_))), "{r:?}");
        // Another address still gets in line.
        let (_x, y) = tokio::io::duplex(1 << 16);
        let other: SocketAddr = "192.0.2.8:4000".parse().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), node.accept_anon(y, other))
                .await
                .is_err(),
            "waiting for its handshake"
        );
        node.shutdown();
    }
}
