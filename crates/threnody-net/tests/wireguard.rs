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
