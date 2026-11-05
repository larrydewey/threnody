//! What strangers can make a node hold: connections that never finish a
//! handshake are capped, in all and per source.

use std::time::Duration;

use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn node(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>) {
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
}

/// Whether the far end closed `s` within a few seconds.
async fn closed(s: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(
        timeout(Duration::from_secs(5), s.read(&mut b)).await,
        Ok(Ok(0) | Err(_))
    )
}

#[tokio::test]
async fn idle_tcp_connections_are_capped_per_source() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, _brx) = node(&dir, "bob");
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    // Sixteen silent connections from one address: each waits for a
    // handshake that never comes.
    let mut idle = Vec::new();
    for _ in 0..16 {
        idle.push(TcpStream::connect(addr).await.unwrap());
    }
    let mut extra = TcpStream::connect(addr).await.unwrap();
    assert!(closed(&mut extra).await, "past the limit: closed at once");
    let mut b = [0u8; 1];
    assert!(
        timeout(Duration::from_millis(300), idle[0].read(&mut b))
            .await
            .is_err(),
        "the ones let in still wait for their handshake"
    );

    // Someone else still gets in.
    #[cfg(target_os = "linux")]
    {
        let (alice, _arx) = node(&dir, "alice");
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind("127.0.0.2:0".parse().unwrap()).unwrap();
        let s = sock.connect(addr).await.unwrap();
        alice
            .connect_stream(s, "tcp", "bob".into(), Some(bob.identity().fingerprint()))
            .await
            .unwrap();
    }

    // And the first address again, once its connections are gone.
    drop(idle);
    let (carol, _crx) = node(&dir, "carol");
    timeout(Duration::from_secs(10), async {
        while carol
            .connect(&addr.to_string(), Some(bob.identity().fingerprint()))
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("places are given back");
}

#[tokio::test]
async fn radio_links_share_one_allowance_for_handshakes() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, _brx) = node(&dir, "bob");
    // Sixteen links whose far end never speaks.
    let mut ends = Vec::new();
    for _ in 0..16 {
        let (x, y) = tokio::io::duplex(1 << 16);
        ends.push(x);
        let b = bob.clone();
        tokio::spawn(async move { b.accept_stream(y, "ble", "silent".into()).await });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_x, y) = tokio::io::duplex(1 << 16);
    let r = timeout(
        Duration::from_secs(1),
        bob.accept_stream(y, "ble", "one more".into()),
    )
    .await
    .expect("refused at once");
    assert!(
        matches!(r, Err(threnody_net::NetError::Refused(_))),
        "{r:?}"
    );
}
