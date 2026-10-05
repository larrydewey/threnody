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
use crate::frame::{read_frame, write_frame};
use crate::handshake;
use crate::node::{Node, lock};

/// The first frame of an anonymous link.
pub(crate) const MAGIC: &[u8] = b"threnody anonymous link v1";
/// Inbound anonymous links held at once, and from one address.
const MAX_INBOUND: usize = 1024;
const MAX_PER_ADDRESS: usize = 16;
/// An outbound link nothing has used for this long is closed.
const IDLE: Duration = Duration::from_secs(120);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

struct Handle {
    id: u64,
    tx: mpsc::UnboundedSender<AppMessage>,
    addr: SocketAddr,
    outbound: bool,
}

#[derive(Default)]
pub(crate) struct AnonState {
    links: HashMap<PublicIdentity, Handle>,
    next_id: u64,
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
        let chan = handshake::initiate(&mut stream, &me).await?;
        let peer = *chan.peer();
        if let Some(want) = expect
            && peer.fingerprint() != want
        {
            return Err(NetError::IdentityMismatch {
                expected: want.to_string(),
                got: peer.fingerprint().to_string(),
            });
        }
        self.spawn_anon(stream, chan, remote, true);
        Ok(peer)
    }

    /// Accepts an anonymous link whose [`MAGIC`] frame was already read.
    pub(crate) async fn accept_anon<S>(&self, mut stream: S, addr: SocketAddr) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        {
            let st = lock(&self.shared.anon);
            let inbound = st.links.values().filter(|h| !h.outbound);
            let (all, here) = inbound.fold((0, 0), |(a, h), l| {
                (a + 1, h + usize::from(l.addr.ip() == addr.ip()))
            });
            if all >= MAX_INBOUND || here >= MAX_PER_ADDRESS {
                return Err(NetError::Refused("too many anonymous links".into()));
            }
        }
        let chan = handshake::accept(&mut stream, self.identity_ref()).await?;
        self.spawn_anon(stream, chan, addr, false);
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

    /// Number of anonymous links open (both directions).
    pub fn anonymous_links(&self) -> usize {
        lock(&self.shared.anon).links.len()
    }

    fn spawn_anon<S>(&self, stream: S, chan: SecureChannel, addr: SocketAddr, outbound: bool)
    where
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
    let (in_tx, mut inbox) = mpsc::channel::<Result<Vec<u8>>>(16);
    let reader = tokio::spawn(async move {
        loop {
            let r = read_frame(&mut rd).await;
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
    let mut pending: VecDeque<AppMessage> = VecDeque::new();
    // Opens the ratchet; nothing about us beyond what every node supports.
    pending.push_back(AppMessage::Hello { features: FEATURES });
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
                while let Some(m) = pending.pop_front() {
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
                    match chan.open(&frame)? {
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
                        pending.push_back(m);
                    }
                    None => return Ok(()),
                },
                Ok(()) = rate.changed() => ticker = make_ticker(*rate.borrow_and_update()),
                () = tick(&mut ticker) => {
                    if chan.can_send() {
                        let m = pending.pop_front().unwrap_or(AppMessage::Cover);
                        write_frame(&mut wr, &chan.seal(&m)?).await?;
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
