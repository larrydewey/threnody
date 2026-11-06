//! Userspace WireGuard (Appendix D): two mutually approved nodes carry
//! IPv6 packets between their TUN interfaces, here in-memory ones.
#![cfg(feature = "boringtun")]

use std::io;
use std::net::Ipv6Addr;
use std::time::Duration;

use threnody_core::store::Home;
use threnody_core::tunnel::overlay_addr;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig, TunIo, WgDevice};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::timeout;

/// A TUN interface in memory: `inject` is what the OS would send through
/// it, `delivered` what the tunnel wrote to it.
struct MemTun {
    from_os: Mutex<UnboundedReceiver<Vec<u8>>>,
    to_os: UnboundedSender<Vec<u8>>,
}

impl TunIo for MemTun {
    async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let p = self
            .from_os
            .lock()
            .await
            .recv()
            .await
            .ok_or(io::ErrorKind::BrokenPipe)?;
        buf[..p.len()].copy_from_slice(&p);
        Ok(p.len())
    }

    async fn write(&self, packet: &[u8]) -> io::Result<()> {
        self.to_os
            .send(packet.to_vec())
            .map_err(|_| io::ErrorKind::BrokenPipe.into())
    }
}

fn mem_tun() -> (MemTun, UnboundedSender<Vec<u8>>, UnboundedReceiver<Vec<u8>>) {
    let (inject, from_os) = unbounded_channel();
    let (to_os, delivered) = unbounded_channel();
    let tun = MemTun {
        from_os: Mutex::new(from_os),
        to_os,
    };
    (tun, inject, delivered)
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("[::]:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn node(dir: &tempfile::TempDir, name: &str, port: u16) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: Some(port),
    })
    .unwrap()
}

/// A UDP packet over IPv6, as an app would send through the tunnel.
fn ipv6_udp(src: Ipv6Addr, dst: Ipv6Addr, body: &[u8]) -> Vec<u8> {
    let len = u16::try_from(8 + body.len()).unwrap();
    let mut p = vec![0x60, 0, 0, 0];
    p.extend(len.to_be_bytes());
    p.extend([17, 64]); // UDP, hop limit
    p.extend(src.octets());
    p.extend(dst.octets());
    p.extend(9000u16.to_be_bytes());
    p.extend(9000u16.to_be_bytes());
    p.extend(len.to_be_bytes());
    p.extend([0, 0]); // no checksum: the tunnel doesn't look
    p.extend(body);
    p
}

async fn up(rx: &mut UnboundedReceiver<Event>) {
    timeout(Duration::from_secs(10), async {
        while !matches!(rx.recv().await, Some(Event::TunnelUp { .. })) {}
    })
    .await
    .expect("tunnel offered");
}

#[tokio::test]
async fn packets_cross_the_tunnel_between_approved_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let (a_port, b_port) = (free_udp_port(), free_udp_port());
    let (alice, mut arx) = node(&dir, "alice", a_port);
    let (bob, mut brx) = node(&dir, "bob", b_port);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity());
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    up(&mut arx).await;
    up(&mut brx).await;

    let (a_tun, a_in, mut a_out) = mem_tun();
    let (b_tun, b_in, mut b_out) = mem_tun();
    let _a = WgDevice::start(alice.clone(), a_tun, a_port).await.unwrap();
    let _b = WgDevice::start(bob.clone(), b_tun, b_port).await.unwrap();

    let (a_ip, b_ip) = (overlay_addr(&alice.identity()), overlay_addr(&bob_id));
    // A packet claiming another source is never delivered: each peer may
    // only send from its own overlay address.
    let spoofed = ipv6_udp(
        overlay_addr(&threnody_core::Identity::generate().public()),
        b_ip,
        b"spoofed",
    );
    let hello = ipv6_udp(a_ip, b_ip, b"hello bob");

    // The first packets wait for the WireGuard handshake; keep sending
    // until one arrives, as an app's retries would.
    let got = timeout(Duration::from_secs(10), async {
        loop {
            a_in.send(spoofed.clone()).unwrap();
            a_in.send(hello.clone()).unwrap();
            if let Ok(Some(p)) = timeout(Duration::from_millis(300), b_out.recv()).await {
                return p;
            }
        }
    })
    .await
    .expect("a packet crossed the tunnel");
    assert_eq!(got, hello);

    let reply = ipv6_udp(b_ip, a_ip, b"hello alice");
    b_in.send(reply.clone()).unwrap();
    let back = timeout(Duration::from_secs(5), a_out.recv())
        .await
        .expect("reply crossed")
        .unwrap();
    assert_eq!(back, reply);
    while let Ok(p) = b_out.try_recv() {
        assert_eq!(p, hello, "only alice's own packets reach bob");
    }

    // Revoking approval takes the peer out of the device.
    bob.set_approval(&alice.identity(), false).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while b_out.try_recv().is_ok() {}
    a_in.send(hello.clone()).unwrap();
    assert!(
        timeout(Duration::from_secs(2), b_out.recv()).await.is_err(),
        "nothing crosses after revocation"
    );
}

/// Two approved nodes whose tunnels can't use WireGuard's own UDP port:
/// each offers a port nobody listens on, as when a carrier's NAT maps it
/// somewhere unknown. Sessions over `quic` (datagrams) or TCP (stream).
async fn blocked_pair(quic: bool) -> (Node, Node, WgDevice, WgDevice, Pipes, Pipes) {
    let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    // Offered, but never bound: the direct path goes nowhere.
    let (a_dead, b_dead) = (free_udp_port(), free_udp_port());
    let (alice, mut arx) = node(dir, "alice", a_dead);
    let (bob, mut brx) = node(dir, "bob", b_dead);
    let bob_id = if quic {
        let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();
        alice.listen_quic("127.0.0.1:0").await.unwrap();
        alice.connect_quic(addr, None).await.unwrap()
    } else {
        let addr = bob.listen("127.0.0.1:0").await.unwrap();
        alice.connect(&addr.to_string(), None).await.unwrap()
    };
    bob.accept_contact(&alice.identity());
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    up(&mut arx).await;
    up(&mut brx).await;
    let (a_tun, a_in, a_out) = mem_tun();
    let (b_tun, b_in, b_out) = mem_tun();
    let a = WgDevice::start(alice.clone(), a_tun, 0).await.unwrap();
    let b = WgDevice::start(bob.clone(), b_tun, 0).await.unwrap();
    (alice, bob, a, b, (a_in, a_out), (b_in, b_out))
}

type Pipes = (UnboundedSender<Vec<u8>>, UnboundedReceiver<Vec<u8>>);

async fn crosses_over_the_session(quic: bool) {
    let (alice, bob, _a, _b, (a_in, mut a_out), (b_in, mut b_out)) = blocked_pair(quic).await;
    let (a_ip, b_ip) = (
        overlay_addr(&alice.identity()),
        overlay_addr(&bob.identity()),
    );
    let hello = ipv6_udp(a_ip, b_ip, b"hello over the session");
    let got = timeout(Duration::from_secs(15), async {
        loop {
            a_in.send(hello.clone()).unwrap();
            if let Ok(Some(p)) = timeout(Duration::from_millis(500), b_out.recv()).await {
                return p;
            }
        }
    })
    .await
    .expect("a packet crossed through the session");
    assert_eq!(got, hello);
    // Bigger than a QUIC datagram can hold before path MTU discovery:
    // such packets take the stream instead.
    let big = ipv6_udp(b_ip, a_ip, &[7u8; 1200]);
    b_in.send(big.clone()).unwrap();
    let back = timeout(Duration::from_secs(5), a_out.recv())
        .await
        .expect("a full-size reply crossed")
        .unwrap();
    assert_eq!(back, big);
}

#[tokio::test]
async fn tunnels_fall_back_to_quic_sessions_when_udp_is_blocked() {
    crosses_over_the_session(true).await;
}

#[tokio::test]
async fn tunnels_fall_back_to_tcp_sessions_when_udp_is_blocked() {
    crosses_over_the_session(false).await;
}

// Persistence (Appendix D, *Persistence*): peers kept across restarts.

/// Opens a node again from the home `node` made, with tunnels on `port`.
/// Nothing reaches across the internet: nothing but the test dials.
fn reopen(dir: &tempfile::TempDir, name: &str, port: u16) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = home.load_identity(None).unwrap();
    let (n, rx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: Some(port),
    })
    .unwrap();
    n.set_reach(false);
    (n, rx)
}

struct Pair {
    alice: Node,
    bob: Node,
    brx: UnboundedReceiver<Event>,
    a_port: u16,
    b_port: u16,
}

/// Alice and Bob, mutually approved over a TCP session, each holding
/// the other's tunnel.
async fn pair(dir: &tempfile::TempDir) -> Pair {
    let (a_port, b_port) = (free_udp_port(), free_udp_port());
    let (alice, mut arx) = node(dir, "alice", a_port);
    let (bob, mut brx) = node(dir, "bob", b_port);
    alice.set_reach(false);
    bob.set_reach(false);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity());
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    up(&mut arx).await;
    up(&mut brx).await;
    // Each side answers the other's first offer with its own: let those
    // land too.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = arx;
    Pair {
        alice,
        bob,
        brx,
        a_port,
        b_port,
    }
}

fn entry(n: &Node, peer: &threnody_core::PublicIdentity) -> Option<threnody_net::node::TunnelPeer> {
    n.tunnel_peer_entries()
        .into_iter()
        .find(|(k, _)| k == peer)
        .map(|(_, tp)| tp)
}

fn saved(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name).join("tunnels.state")
}

/// Stops `n` and gives its tasks a moment to finish writing.
async fn stop(n: Node) {
    n.shutdown();
    drop(n);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Whether some `TunnelUp` arrives within `wait`.
async fn tunnel_up_within(rx: &mut UnboundedReceiver<Event>, wait: Duration) -> Option<Event> {
    timeout(wait, async {
        loop {
            match rx.recv().await {
                Some(e @ Event::TunnelUp { .. }) => return e,
                Some(_) => {}
                None => std::future::pending().await,
            }
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn tunnels_survive_a_restart_without_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let Pair {
        alice, bob, b_port, ..
    } = pair(&dir).await;
    let (aid, bid) = (alice.identity(), bob.identity());
    let (b_tun, b_in, mut b_out) = mem_tun();
    let _b = WgDevice::start(bob.clone(), b_tun, b_port).await.unwrap();
    let bob_psk = *entry(&bob, &aid).unwrap().psk.0;
    assert!(saved(&dir, "alice").exists());
    stop(alice).await;

    // Alice comes back on another port and dials no one.
    let a_port2 = free_udp_port();
    let (alice, _arx) = reopen(&dir, "alice", a_port2);
    assert_eq!(alice.tunnel_peers(), vec![bid]);
    assert!(alice.sessions().is_empty());
    assert_eq!(*entry(&alice, &bid).unwrap().psk.0, bob_psk);

    let (a_tun, a_in, mut a_out) = mem_tun();
    let _a = WgDevice::start(alice.clone(), a_tun, a_port2)
        .await
        .unwrap();
    let (a_ip, b_ip) = (overlay_addr(&aid), overlay_addr(&bid));
    let hello = ipv6_udp(a_ip, b_ip, b"back again");
    let got = timeout(Duration::from_secs(10), async {
        loop {
            a_in.send(hello.clone()).unwrap();
            if let Ok(Some(p)) = timeout(Duration::from_millis(300), b_out.recv()).await {
                return p;
            }
        }
    })
    .await
    .expect("a packet crossed the restored tunnel");
    assert_eq!(got, hello);
    let reply = ipv6_udp(b_ip, a_ip, b"welcome back");
    b_in.send(reply.clone()).unwrap();
    let back = timeout(Duration::from_secs(5), a_out.recv())
        .await
        .expect("reply crossed")
        .unwrap();
    assert_eq!(back, reply);
    assert!(alice.sessions().is_empty(), "no session was needed");
    // Bob's device saw Alice's authenticated packets come from her new
    // port, and that is what Bob keeps for her.
    let moved: std::net::SocketAddr = ([127, 0, 0, 1], a_port2).into();
    assert_eq!(entry(&bob, &aid).unwrap().seen, Some(moved));
    let home = Home::new(dir.path().join("bob"));
    let plain = home
        .load_state(bob.identity_ref(), threnody_core::tunnel::STATE)
        .unwrap()
        .unwrap();
    let kept = threnody_core::tunnel::decode_saved(&plain).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].endpoint, moved);
}

#[tokio::test]
async fn restore_skips_contacts_no_longer_mutual() {
    let dir = tempfile::tempdir().unwrap();
    let Pair { alice, bob, .. } = pair(&dir).await;
    let bid = bob.identity();
    stop(alice).await;
    // Revoked while the node was down (as an older version or another
    // tool might leave it): the restore filter, not `tunnel_down`.
    let home = Home::new(dir.path().join("alice"));
    let id = home.load_identity(None).unwrap();
    let mut contacts = home.load_contacts(&id).unwrap();
    contacts.get_mut(&bid).unwrap().local_approved = false;
    home.save_contacts(&id, &contacts).unwrap();

    let (alice, _arx) = reopen(&dir, "alice", free_udp_port());
    assert!(alice.tunnel_peers().is_empty());
    assert!(
        !saved(&dir, "alice").exists(),
        "the restore pruned the file"
    );
}

#[tokio::test]
async fn corrupt_tunnel_state_is_forgotten_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice", free_udp_port());
    stop(alice).await;
    std::fs::write(saved(&dir, "alice"), b"not a state file at all").unwrap();
    let (alice, _arx) = reopen(&dir, "alice", free_udp_port());
    assert!(alice.tunnel_peers().is_empty());
    assert!(!saved(&dir, "alice").exists());
}

#[tokio::test]
async fn turning_tunnels_off_deletes_saved_peers() {
    let dir = tempfile::tempdir().unwrap();
    let Pair { alice, a_port, .. } = pair(&dir).await;
    assert!(saved(&dir, "alice").exists());
    alice.set_tunnel_port(None).unwrap();
    assert!(!saved(&dir, "alice").exists());
    assert!(alice.tunnel_peers().is_empty());
    // Nothing comes back: Bob took an offer this session already, so he
    // doesn't answer the new one (Appendix D, *Not yet done*).
    alice.set_tunnel_port(Some(a_port)).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(alice.tunnel_peers().is_empty());
    assert!(!saved(&dir, "alice").exists());
}

#[tokio::test]
async fn port_change_keeps_peers_and_moves_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let Pair {
        alice,
        bob,
        mut brx,
        ..
    } = pair(&dir).await;
    let (aid, bid) = (alice.identity(), bob.identity());
    let psk = *entry(&bob, &aid).unwrap().psk.0;
    while brx.try_recv().is_ok() {}
    let new = free_udp_port();
    alice.set_tunnel_port(Some(new)).unwrap();
    assert_eq!(alice.tunnel_peers(), vec![bid]);
    let Some(Event::TunnelUp {
        endpoint, psk: p, ..
    }) = tunnel_up_within(&mut brx, Duration::from_secs(5)).await
    else {
        panic!("Bob heard of the new port");
    };
    assert_eq!(endpoint.port(), new);
    assert_eq!(*p.0, psk, "the same session's PSK");
    assert_eq!(entry(&bob, &aid).unwrap().endpoint.port(), new);
    assert!(
        tunnel_up_within(&mut brx, Duration::from_millis(1000))
            .await
            .is_none(),
        "exactly one"
    );
}

#[tokio::test]
async fn identical_reoffers_change_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let Pair {
        alice,
        mut brx,
        a_port,
        ..
    } = pair(&dir).await;
    let file = saved(&dir, "bob");
    let mtime = || std::fs::metadata(&file).unwrap().modified().unwrap();
    // Bob's own startup writes settle first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while brx.try_recv().is_ok() {}
    let before = mtime();
    alice.set_tunnel_port(Some(a_port)).unwrap();
    assert!(
        tunnel_up_within(&mut brx, Duration::from_millis(1500))
            .await
            .is_none()
    );
    assert_eq!(mtime(), before, "nothing written");
}

#[tokio::test]
async fn tunnels_off_race_never_resurrects_state() {
    let dir = tempfile::tempdir().unwrap();
    let Pair {
        alice, bob, b_port, ..
    } = pair(&dir).await;
    // Alice keeps moving her port, so each offer changes Bob's entry and
    // makes him save it, while Bob turns tunnels off and on.
    let a = alice.clone();
    let mover = tokio::spawn(async move {
        let (p1, p2) = (free_udp_port(), free_udp_port());
        loop {
            for p in [p1, p2] {
                a.set_tunnel_port(Some(p)).unwrap();
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
        }
    });
    let file = saved(&dir, "bob");
    for _ in 0..30 {
        bob.set_tunnel_port(None).unwrap();
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(4)).await;
            assert!(!file.exists(), "saved while tunnels were off");
            assert!(bob.tunnel_peers().is_empty());
        }
        bob.set_tunnel_port(Some(b_port)).unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    mover.abort();
    bob.set_tunnel_port(None).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!file.exists());
}

#[tokio::test]
async fn network_change_reoffers_tunnels_on_live_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let Pair {
        alice,
        bob,
        mut brx,
        b_port,
        ..
    } = pair(&dir).await;
    let aid = alice.identity();
    // Bob loses Alice's tunnel; Alice took an offer this session already,
    // so she doesn't answer his new one.
    bob.set_tunnel_port(None).unwrap();
    bob.set_tunnel_port(Some(b_port)).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(bob.tunnel_peers().is_empty());
    while brx.try_recv().is_ok() {}
    // Alice's network changes: she offers again, internet reach or not.
    alice.network_switched(false);
    let Some(Event::TunnelUp { peer, .. }) =
        tunnel_up_within(&mut brx, Duration::from_secs(5)).await
    else {
        panic!("Bob heard Alice's offer again");
    };
    assert_eq!(peer, aid);
    assert_eq!(bob.tunnel_peers(), vec![aid]);
}
