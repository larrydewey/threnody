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
        quote: None,
        forwarded: false,
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

/// Slowing cover traffic mid-session (as a phone does when its network
/// turns metered) tells the peer the new heartbeat first: its lease,
/// sized for the old one, must not end the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slowing_cover_traffic_mid_session_keeps_the_session() {
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    // Every 200 ms: Bob's lease for Alice is then 3 s.
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
    let bob_id = alice.connect_quic(b_quic, None).await.unwrap();
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

    alice.set_constant_rate(Some(Duration::from_secs(4)));
    let dropped = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(Event::Disconnected { reason, .. }) = brx.recv().await {
                return reason;
            }
        }
    })
    .await;
    assert!(dropped.is_err(), "session dropped: {dropped:?}");
    assert!(alice.sessions().iter().any(|s| s.peer == bob_id));
}

/// A TCP proxy to `to` passing at most `per_s` bytes a second from the
/// dialer, as a slow uplink would; the other way at full speed.
async fn slow_proxy(to: std::net::SocketAddr, per_s: usize) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let server = tokio::net::TcpStream::connect(to).await.unwrap();
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut sr, &mut cw).await;
        });
        let mut buf = vec![0u8; per_s / 10];
        while let Ok(n) = cr.read(&mut buf).await {
            if n == 0 || sw.write_all(&buf[..n]).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    addr
}

/// A file that takes longer to arrive than the lease still gets through:
/// its bytes arriving count as the peer talking, not just whole frames.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_slower_than_the_lease_still_arrives() {
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    // Every 200 ms: the lease is then 3 s.
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
    bob.listen_quic("127.0.0.1:0").await.unwrap();
    for n in [&alice, &bob] {
        n.start_reach(cfg.clone()).unwrap();
    }
    timeout(Duration::from_secs(10), async {
        while alice.reachability().candidates.is_empty() || bob.reachability().candidates.is_empty()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let b_tcp = bob.listen("127.0.0.1:0").await.unwrap();
    // 1.5 MB at 300 kB/s: about 5 s for the one frame.
    let proxy = slow_proxy(b_tcp, 300_000).await;
    let bob_id = alice.connect(&proxy.to_string(), None).await.unwrap();
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

    alice
        .send_file(
            &bob_id,
            threnody_net::history::OutgoingFile {
                name: "big.bin".into(),
                data: vec![7u8; 1_500_000],
                location: None,
                sensitive: false,
                caption: String::new(),
                album: 0,
                clip: None,
                forwarded: false,
            },
        )
        .unwrap();
    let got = next(&mut brx, 30, |e| {
        matches!(
            e,
            Event::Disconnected { .. }
                | Event::Message {
                    msg: AppMessage::File { .. },
                    ..
                }
        )
    })
    .await;
    assert!(
        matches!(got, Event::Message { .. }),
        "the session ended first: {got:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strangers_find_each_other_by_handle() {
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
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
    let handle = bob.set_handle(Some("Bob"), 0).unwrap().unwrap();
    assert_eq!(bob.handle(), Some(handle.clone()));
    next(
        &mut brx,
        60,
        |e| matches!(e, Event::ReachNote { note } if note.starts_with("published handle")),
    )
    .await;

    // A handle nobody publishes isn't found.
    let nobody = threnody_core::handle::Handle::new(&alice.identity(), "nobody", 0).unwrap();
    assert!(alice.find_handle(&nobody.to_string()).await.is_err());

    let typed = handle.to_string().to_uppercase();
    let peer = alice.find_handle(&typed).await.unwrap();
    assert_eq!(peer, bob.identity());
    alice.send(&peer, text("found you by handle")).unwrap();
    let Event::MessageRequest { msg, .. } =
        next(&mut brx, 10, |e| matches!(e, Event::MessageRequest { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(msg, text("found you by handle"));

    // Turned off, it stops answering once the record ages out; the
    // node no longer claims one.
    bob.set_handle(None, 0).unwrap();
    assert_eq!(bob.handle(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invites_find_their_maker_when_its_address_is_dead() {
    let testnet = tokio::task::spawn_blocking(|| mainline::Testnet::builder(8).build().unwrap())
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
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
    // The address a phone on mobile data would offer: nobody can dial it.
    let link = bob.invite_link(Some("192.0.2.1:7450"));
    assert_eq!(link.addrs, ["192.0.2.1:7450"]);
    let key = link.key.expect("reachability is on: the link has a key");
    assert_eq!(bob.invite_key(), key, "the key is kept");
    next(
        &mut brx,
        60,
        |e| matches!(e, Event::ReachNote { note } if note == "published invite"),
    )
    .await;

    // Through the printed link, as a scanned QR code would give it.
    let scanned: threnody_core::invite::InviteLink = link.to_string().parse().unwrap();
    let peer = timeout(Duration::from_secs(60), alice.reach_invite(&scanned))
        .await
        .expect("found in time")
        .unwrap();
    assert_eq!(peer, bob.identity());
    alice.send(&peer, text("found you by invite")).unwrap();
    let Event::MessageRequest { msg, .. } =
        next(&mut brx, 10, |e| matches!(e, Event::MessageRequest { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(msg, text("found you by invite"));

    // A link naming someone else under that key is refused, not dialed.
    let (carol, _crx) = node(&dir, "carol");
    let forged = threnody_core::invite::InviteLink {
        fingerprint: carol.identity().fingerprint(),
        addrs: vec![],
        key: Some(key),
    };
    let (dave, _drx) = node(&dir, "dave");
    dave.listen_quic("127.0.0.1:0").await.unwrap();
    dave.start_reach(cfg.clone()).unwrap();
    assert!(dave.reach_invite(&forged).await.is_err());
    assert!(dave.sessions().iter().all(|s| s.peer != bob.identity()));

    // A new key: old links no longer lead here.
    assert_ne!(bob.reset_invite_key(), key);
}

#[tokio::test]
async fn invites_try_every_address_ipv6_included() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, _brx) = node(&dir, "bob");
    let Ok(addr) = bob.listen("[::1]:0").await else {
        eprintln!("no IPv6 loopback here; skipping");
        return;
    };
    // A dead address first (one that hangs, as a carrier's private one
    // does from outside), then IPv6: the link still works, and soon.
    let link: threnody_core::invite::InviteLink = format!(
        "threnody://{}@192.0.2.1:7450,{addr}",
        bob.identity().fingerprint().compact()
    )
    .parse()
    .unwrap();
    assert_eq!(link.addrs[1], format!("[::1]:{}", addr.port()));
    let peer = timeout(Duration::from_secs(5), alice.reach_invite(&link))
        .await
        .expect("didn't wait for the dead address")
        .unwrap();
    assert_eq!(peer, bob.identity());
}
