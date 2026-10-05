//! QUIC sessions, and reaching contacts through DHT rendezvous and hole
//! punching (Appendix N), on loopback with a local DHT testnet.

use std::time::Duration;

use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity};
use threnody_net::reach::ReachConfig;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn node(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity: Identity = home.create_identity(None).unwrap();
    Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap()
}

async fn next(
    rx: &mut UnboundedReceiver<Event>,
    secs: u64,
    pred: impl Fn(&Event) -> bool,
) -> Event {
    timeout(Duration::from_secs(secs), async {
        loop {
            let e = rx.recv().await.expect("event stream open");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
    .expect("expected event did not arrive")
}

fn text(body: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 1,
        body: body.into(),
        expires_in_s: None,
        id: 0,
    }
}

#[tokio::test]
async fn sessions_run_over_quic() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();

    let bob_id = alice
        .connect_quic(addr, Some(bob.identity().fingerprint()))
        .await
        .unwrap();
    assert_eq!(bob_id, bob.identity());
    next(&mut brx, 10, |e| matches!(e, Event::Connected { .. })).await;
    let s = alice.sessions();
    assert_eq!(s[0].transport, "quic");
    assert_eq!(s[0].addr, addr);

    alice.send(&bob_id, text("over udp")).unwrap();
    let Event::MessageRequest { msg, .. } =
        next(&mut brx, 10, |e| matches!(e, Event::MessageRequest { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(msg, text("over udp"));
}

#[tokio::test]
async fn quic_refuses_the_wrong_identity() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, _brx) = node(&dir, "bob");
    let (carol, _crx) = node(&dir, "carol");
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();
    let r = alice
        .connect_quic(addr, Some(carol.identity().fingerprint()))
        .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn dialing_an_address_falls_back_to_quic() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, _brx) = node(&dir, "bob");
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    // Bob listens on UDP only: TCP to that port is refused.
    let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();
    let peer = alice
        .connect(&addr.to_string(), Some(bob.identity().fingerprint()))
        .await
        .unwrap();
    assert_eq!(peer, bob.identity());
    assert_eq!(alice.sessions()[0].transport, "quic");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_find_each_other_through_the_dht() {
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");

    // Meet once (over TCP) and approve each other, so both hold the
    // pairwise discovery key, then part.
    let b_tcp = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&b_tcp.to_string(), None).await.unwrap();
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    for rx in [&mut arx, &mut brx] {
        next(rx, 10, |e| {
            matches!(e, Event::ApprovalChanged { mutual: true, .. })
        })
        .await;
    }
    timeout(Duration::from_secs(5), async {
        while alice
            .contacts()
            .get(&bob_id)
            .unwrap()
            .discovery_key
            .is_none()
            || bob
                .contacts()
                .get(&alice.identity())
                .unwrap()
                .discovery_key
                .is_none()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    alice.disconnect(&bob_id);
    next(&mut brx, 10, |e| matches!(e, Event::Disconnected { .. })).await;
    assert!(alice.sessions().is_empty());

    let cfg = ReachConfig {
        bootstrap: Some(testnet.bootstrap.clone()),
        reflectors: vec![],
        poll_foreground: Duration::from_secs(1),
        poll_seeking: Duration::from_secs(1),
        loopback: true,
        ..ReachConfig::default()
    };
    for n in [&alice, &bob] {
        n.listen_quic("127.0.0.1:0").await.unwrap();
        n.start_reach(cfg.clone()).unwrap();
    }
    alice.seek(&bob_id);

    let Event::Connected { peer, .. } =
        next(&mut arx, 60, |e| matches!(e, Event::Connected { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(peer, bob_id);
    assert_eq!(alice.sessions()[0].transport, "quic");
    assert!(alice.reachability().online);
    assert!(!alice.reachability().candidates.is_empty());

    alice.send(&bob_id, text("found you")).unwrap();
    let Event::Message { msg, .. } =
        next(&mut brx, 10, |e| matches!(e, Event::Message { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(msg, text("found you"));
}

#[tokio::test]
async fn nothing_reaches_out_while_turned_off() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    alice.set_reach(false);
    alice
        .start_reach(ReachConfig {
            bootstrap: Some(vec!["127.0.0.1:1".into()]),
            loopback: true,
            ..ReachConfig::default()
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = alice.reachability();
    assert!(!r.enabled);
    assert!(!r.online, "joined the DHT while off");
    assert!(r.candidates.is_empty());
}

/// Forwards UDP between a client and `server` until `cut` is set, then
/// drops everything: a path that dies without a word, as Wi-Fi does.
async fn udp_proxy(
    server: std::net::SocketAddr,
    cut: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::net::SocketAddr {
    use std::sync::atomic::Ordering;
    let front = std::sync::Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let back = std::sync::Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = front.local_addr().unwrap();
    let client = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (f, b, c, x) = (front.clone(), back.clone(), client.clone(), cut.clone());
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, src)) = f.recv_from(&mut buf).await {
            *c.lock().unwrap() = Some(src);
            if !x.load(Ordering::Relaxed) {
                let _ = b.send_to(&buf[..n], server).await;
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = back.recv_from(&mut buf).await {
            let to = *client.lock().unwrap();
            if let Some(to) = to
                && !cut.load(Ordering::Relaxed)
            {
                let _ = front.send_to(&buf[..n], to).await;
            }
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silently_dead_path_is_repaired_within_seconds() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    // Cover traffic every 200 ms: the lease is then about 2.5 s.
    for n in [&alice, &bob] {
        n.set_constant_rate(Some(Duration::from_millis(200)));
    }
    let cfg = ReachConfig {
        bootstrap: Some(testnet.bootstrap.clone()),
        reflectors: vec![],
        loopback: true,
        ..ReachConfig::default()
    };
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    let b_quic = bob.listen_quic("127.0.0.1:0").await.unwrap();
    for n in [&alice, &bob] {
        n.start_reach(cfg.clone()).unwrap();
    }
    // Wait until both know their own addresses, so `Paths` carry them.
    timeout(Duration::from_secs(10), async {
        while alice.reachability().candidates.is_empty() || bob.reachability().candidates.is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();

    let cut = std::sync::Arc::new(AtomicBool::new(false));
    let proxy = udp_proxy(b_quic, cut.clone()).await;
    let bob_id = alice.connect_quic(proxy, None).await.unwrap();
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    for rx in [&mut arx, &mut brx] {
        next(rx, 10, |e| {
            matches!(e, Event::ApprovalChanged { mutual: true, .. })
        })
        .await;
    }
    // Let the `Paths` messages cross.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    cut.store(true, Ordering::Relaxed);
    let cut_at = std::time::Instant::now();
    next(&mut arx, 10, |e| matches!(e, Event::Disconnected { .. })).await;
    let lost_after = cut_at.elapsed();
    let Event::Connected { peer, addr, .. } =
        next(&mut arx, 30, |e| matches!(e, Event::Connected { .. })).await
    else {
        unreachable!()
    };
    let back_after = cut_at.elapsed();
    println!("lost after {lost_after:?}, back after {back_after:?}");
    assert_eq!(peer, bob_id);
    assert_ne!(addr, proxy, "reconnected around the dead path");
    assert!(
        lost_after < Duration::from_secs(5),
        "lease took {lost_after:?}"
    );
    assert!(
        back_after < Duration::from_secs(12),
        "recovery took {back_after:?}"
    );
}
