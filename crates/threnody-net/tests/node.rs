use std::time::Duration;

use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity};
use threnody_net::{AcceptPolicy, Event, NetError, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn node(
    dir: &tempfile::TempDir,
    name: &str,
    policy: AcceptPolicy,
    rate: Option<Duration>,
) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    Node::new(NodeConfig {
        home,
        identity,
        policy,
        constant_rate: rate,
        tunnel_port: Some(51820),
    })
    .unwrap()
}

async fn next(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(10), async {
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

#[tokio::test]
async fn chat_and_mutual_approval_over_tcp() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();

    let bob_id = alice
        .connect(&addr.to_string(), Some(bob.identity().fingerprint()))
        .await
        .unwrap();
    assert_eq!(bob_id, bob.identity());
    let Event::Connected {
        peer, new_contact, ..
    } = next(&mut brx, |e| matches!(e, Event::Connected { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(peer, alice.identity());
    assert!(new_contact);

    alice
        .send(
            &bob_id,
            AppMessage::Text {
                sent_ms: 1,
                body: "hi bob".into(),
                expires_in_s: None,
            },
        )
        .unwrap();
    // Bob never contacted Alice, so her first message is a request.
    let Event::MessageRequest { msg, .. } =
        next(&mut brx, |e| matches!(e, Event::MessageRequest { .. })).await
    else {
        unreachable!()
    };
    bob.accept_contact(&alice.identity());
    assert_eq!(
        msg,
        AppMessage::Text {
            sent_ms: 1,
            body: "hi bob".into(),
            expires_in_s: None
        }
    );

    bob.send(
        &alice.identity(),
        AppMessage::Text {
            sent_ms: 2,
            body: "hi alice".into(),
            expires_in_s: None,
        },
    )
    .unwrap();
    next(&mut arx, |e| matches!(e, Event::Message { .. })).await;

    alice.set_approval(&bob_id, true).unwrap();
    next(&mut brx, |e| {
        matches!(
            e,
            Event::ApprovalChanged {
                remote_approved: true,
                mutual: false,
                ..
            }
        )
    })
    .await;
    bob.set_approval(&alice.identity(), true).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    assert!(alice.contacts().get(&bob_id).unwrap().mutually_approved());

    // Revocation propagates.
    bob.set_approval(&alice.identity(), false).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: false, .. })
    })
    .await;
    assert!(!alice.contacts().get(&bob_id).unwrap().mutually_approved());
    assert_eq!(
        alice.contacts().get(&bob_id).unwrap().last_addr.as_deref(),
        Some(addr.to_string().as_str())
    );
}

#[tokio::test]
async fn pinned_fingerprint_mismatch_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, _brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let wrong = Identity::generate().public().fingerprint();
    let r = alice.connect(&addr.to_string(), Some(wrong)).await;
    assert!(matches!(r, Err(NetError::IdentityMismatch { .. })));
    assert!(alice.sessions().is_empty());
}

#[tokio::test]
async fn approved_only_policy_rejects_strangers() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::ApprovedOnly, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    // The handshake completes (alice learns who bob is), then bob hangs up.
    let _ = alice.connect(&addr.to_string(), None).await;
    let e = next(&mut brx, |e| matches!(e, Event::Rejected { .. })).await;
    assert!(matches!(e, Event::Rejected { reason, .. } if reason.contains("refused")));
    assert!(bob.sessions().is_empty());
}

#[tokio::test]
async fn constant_rate_mode_delivers_and_hides_idle() {
    let dir = tempfile::tempdir().unwrap();
    let rate = Some(Duration::from_millis(20));
    let (alice, _arx) = node(&dir, "alice", AcceptPolicy::Anyone, rate);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, rate);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    alice
        .send(
            &bob_id,
            AppMessage::Text {
                sent_ms: 3,
                body: "steady".into(),
                expires_in_s: None,
            },
        )
        .unwrap();
    let Event::Message { msg, .. } = next(&mut brx, |e| matches!(e, Event::Message { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(
        msg,
        AppMessage::Text {
            sent_ms: 3,
            body: "steady".into(),
            expires_in_s: None
        }
    );
}

#[tokio::test]
async fn cover_traffic_can_be_switched_on_and_off_live() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    let say = |s: &str| AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
    };
    for (rate, body) in [
        (Some(Duration::from_millis(20)), "at a constant rate"),
        (None, "immediately again"),
        (Some(Duration::from_millis(50)), "and slower"),
    ] {
        alice.set_constant_rate(rate);
        assert_eq!(alice.constant_rate(), rate);
        alice.send(&bob_id, say(body)).unwrap();
        next(
            &mut brx,
            |e| matches!(e, Event::Message { msg, .. } if *msg == say(body)),
        )
        .await;
    }
}

#[tokio::test]
async fn tunnels_follow_mutual_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();

    let up = |e: &Event| matches!(e, Event::TunnelUp { .. });
    let Event::TunnelUp {
        peer: pa,
        endpoint: ea,
        psk: ka,
        overlay: oa,
        ..
    } = next(&mut arx, up).await
    else {
        unreachable!()
    };
    let Event::TunnelUp {
        peer: pb,
        psk: kb,
        overlay: ob,
        ..
    } = next(&mut brx, up).await
    else {
        unreachable!()
    };
    assert_eq!(pa, bob_id);
    assert_eq!(pb, alice.identity());
    assert_eq!(ea, "127.0.0.1:51820".parse().unwrap());
    assert_eq!(*ka.0, *kb.0, "both ends must derive the same WireGuard PSK");
    assert_eq!(oa, threnody_core::tunnel::overlay_addr(&bob_id));
    assert_eq!(ob, threnody_core::tunnel::overlay_addr(&alice.identity()));
    assert_eq!(alice.tunnel_peers(), vec![bob_id]);

    bob.set_approval(&alice.identity(), false).unwrap();
    next(&mut arx, |e| {
        matches!(
            e,
            Event::TunnelDown {
                wg_public: Some(_),
                ..
            }
        )
    })
    .await;
    next(&mut brx, |e| matches!(e, Event::TunnelDown { .. })).await;
    assert!(alice.tunnel_peers().is_empty());
}

#[tokio::test]
async fn no_tunnel_without_mutual_approval() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, _brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    alice.set_approval(&bob_id, true).unwrap();
    alice
        .send(
            &bob_id,
            AppMessage::Text {
                sent_ms: 0,
                body: "sync".into(),
                expires_in_s: None,
            },
        )
        .unwrap();
    let r = timeout(Duration::from_millis(500), async {
        loop {
            if let Some(Event::TunnelUp { .. }) = arx.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(r.is_err(), "tunnel offered with one-sided approval");
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn approved_peers_rediscover_each_other_on_the_lan() {
    use threnody_net::DiscoveryConfig;
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let a_tcp = alice.listen("127.0.0.1:0").await.unwrap();
    let b_tcp = bob.listen("127.0.0.1:0").await.unwrap();

    // Pair once over TCP so both sides hold the discovery key.
    let bob_id = alice.connect(&b_tcp.to_string(), None).await.unwrap();
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut brx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
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
    assert_eq!(
        alice.contacts().get(&bob_id).unwrap().discovery_key,
        bob.contacts().get(&alice.identity()).unwrap().discovery_key
    );

    // Drop the session; forget where bob was.
    alice.disconnect(&bob_id);
    next(&mut brx, |e| matches!(e, Event::Disconnected { .. })).await;
    alice.update_contacts(|c| c.get_mut(&bob_id).unwrap().last_addr = None);

    let (pa, pb) = (free_udp_port(), free_udp_port());
    let cfg = |me: u16, other: u16| DiscoveryConfig {
        bind: format!("127.0.0.1:{me}").parse().unwrap(),
        targets: vec![format!("127.0.0.1:{other}").parse().unwrap()],
        interval: Duration::from_millis(100),
        auto_connect: true,
    };
    alice.start_discovery(cfg(pa, pb), a_tcp.port()).unwrap();
    bob.start_discovery(cfg(pb, pa), b_tcp.port()).unwrap();

    next(
        &mut arx,
        |e| matches!(e, Event::Discovered { peer, .. } if *peer == bob_id),
    )
    .await;
    next(&mut brx, |e| matches!(e, Event::Discovered { .. })).await;
    // One side dials (smaller key), and both end up connected again.
    next(
        &mut arx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == bob_id),
    )
    .await;
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
}

#[tokio::test]
async fn revocation_clears_discovery_keys() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut brx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    bob.set_approval(&alice.identity(), false).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: false, .. })
    })
    .await;
    assert!(
        alice
            .contacts()
            .get(&bob_id)
            .unwrap()
            .discovery_key
            .is_none()
    );
    assert!(
        bob.contacts()
            .get(&alice.identity())
            .unwrap()
            .discovery_key
            .is_none()
    );
}

#[tokio::test]
async fn sessions_run_over_any_byte_stream() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let (a_end, b_end) = tokio::io::duplex(1 << 16);
    let bob2 = bob.clone();
    let accept =
        tokio::spawn(async move { bob2.accept_stream(b_end, "ble", "AA:BB".into()).await });
    let bid = alice
        .connect_stream(
            a_end,
            "ble",
            "CC:DD".into(),
            Some(bob.identity().fingerprint()),
        )
        .await
        .unwrap();
    assert_eq!(accept.await.unwrap().unwrap(), alice.identity());
    bob.accept_contact(&alice.identity());
    let s = alice
        .sessions()
        .into_iter()
        .find(|s| s.peer == bid)
        .unwrap();
    assert_eq!(
        (s.transport, s.remote.as_str(), s.via),
        ("ble", "CC:DD", None)
    );
    alice
        .send(
            &bid,
            AppMessage::Text {
                sent_ms: 0,
                body: "over a pipe".into(),
                expires_in_s: None,
            },
        )
        .unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;
}

#[tokio::test]
async fn bluetooth_beacons_pick_exactly_one_dialer() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let (carol, _crx) = node(&dir, "carol", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut brx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
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

    // Connected peers are not dialed.
    let (a_adv, b_adv) = (alice.ble_beacon(0x81), bob.ble_beacon(0x82));
    assert_eq!(a_adv.len(), 146, "one extended-advert-sized block");
    assert_eq!(bob.ble_heard(&a_adv), None);
    assert_eq!(alice.ble_heard(&b_adv), None);

    alice.disconnect(&bob_id);
    next(&mut brx, |e| matches!(e, Event::Disconnected { .. })).await;
    timeout(Duration::from_secs(5), async {
        while !alice.sessions().is_empty() || !bob.sessions().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // Exactly one side dials, with the PSM from the other's advert.
    let by_bob = bob.ble_heard(&alice.ble_beacon(0x81));
    let by_alice = alice.ble_heard(&bob.ble_beacon(0x82));
    if alice.identity().as_bytes() < bob_id.as_bytes() {
        assert_eq!((by_alice, by_bob), (Some((bob_id, 0x82)), None));
        assert_eq!(alice.ble_heard(&bob.ble_beacon(0x82)), None, "redial waits");
    } else {
        assert_eq!((by_alice, by_bob), (None, Some((alice.identity(), 0x81))));
        assert_eq!(bob.ble_heard(&alice.ble_beacon(0x81)), None, "redial waits");
    }
    // If the dialer never shows up, the other side dials after a while.
    let t0 = std::time::Instant::now();
    let (other, other_psm) = if alice.identity().as_bytes() < bob_id.as_bytes() {
        (&bob, 0x81)
    } else {
        (&alice, 0x82)
    };
    let heard = |n: &Node, at| {
        let b = if n.identity() == bob_id {
            alice.ble_beacon(0x81)
        } else {
            bob.ble_beacon(0x82)
        };
        n.ble_heard_at(&b, at)
    };
    assert_eq!(heard(other, t0 + Duration::from_secs(10)), None);
    assert_eq!(heard(other, t0 + Duration::from_secs(40)), None);
    let fallback = heard(other, t0 + Duration::from_secs(60)).expect("fallback dial");
    assert_eq!(fallback.1, other_psm);
    // A long silence restarts the clock.
    assert_eq!(heard(other, t0 + Duration::from_secs(300)), None);

    // Two sessions that started at once leave each side with the other's
    // key as "latest"; recent keys still match.
    let a_id = alice.identity();
    alice.update_contacts(|c| {
        let c = c.get_mut(&bob_id).unwrap();
        c.set_discovery_key([1; 32]);
        c.set_discovery_key([2; 32]);
    });
    bob.update_contacts(|c| {
        let c = c.get_mut(&a_id).unwrap();
        c.set_discovery_key([2; 32]);
        c.set_discovery_key([1; 32]);
    });
    let recognises = |n: &Node, b: &[u8], peer| {
        let keys: Vec<_> = n
            .contacts()
            .get(&peer)
            .unwrap()
            .recognition_keys()
            .copied()
            .collect();
        keys.iter().any(|k| {
            !threnody_core::discovery::recognise(b, [(&peer, k)], threnody_core::now_ms() / 1000)
                .is_empty()
        })
    };
    assert!(recognises(&bob, &alice.ble_beacon(0x81), a_id));
    assert!(recognises(&alice, &bob.ble_beacon(0x82), bob_id));

    // Strangers learn nothing they can act on.
    assert_eq!(carol.ble_heard(&alice.ble_beacon(0x81)), None);
    assert_eq!(
        threnody_core::discovery::beacon_port(&alice.ble_beacon(0x81)),
        Some(0x81)
    );
}

#[tokio::test]
async fn wifi_direct_offers_only_between_approved_neighbours() {
    use threnody_net::DirectOffer;
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice", AcceptPolicy::Anyone, None);
    let (bob, mut brx) = node(&dir, "bob", AcceptPolicy::Anyone, None);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let bob_id = alice.connect(&addr.to_string(), None).await.unwrap();
    bob.accept_contact(&alice.identity()); // else alice's messages are requests
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    let offer = DirectOffer {
        ssid: "DIRECT-th-test".into(),
        passphrase: "a long passphrase".into(),
        addr: "192.168.49.1:7450".into(),
    };
    // Not yet approved: refused locally.
    assert!(alice.offer_wifi_direct(&bob_id, offer.clone()).is_err());
    assert!(alice.request_wifi_direct(&bob_id).is_err());

    alice.set_approval(&bob_id, true).unwrap();
    bob.set_approval(&alice.identity(), true).unwrap();
    next(&mut arx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut brx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;

    alice.request_wifi_direct(&bob_id).unwrap();
    let e = next(&mut brx, |e| matches!(e, Event::WifiDirectRequested { .. })).await;
    assert!(matches!(e, Event::WifiDirectRequested { peer } if peer == alice.identity()));

    bob.offer_wifi_direct(&alice.identity(), offer.clone())
        .unwrap();
    let e = next(&mut arx, |e| matches!(e, Event::WifiDirectOffer { .. })).await;
    let Event::WifiDirectOffer { peer, offer: got } = e else {
        unreachable!()
    };
    assert_eq!((peer, got), (bob_id, offer.clone()));

    // Bad offers are refused before they leave.
    let mut bad = offer;
    bad.passphrase = "short".into();
    assert!(bob.offer_wifi_direct(&alice.identity(), bad).is_err());
}
