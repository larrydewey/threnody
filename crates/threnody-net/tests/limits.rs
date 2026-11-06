//! What strangers can make a node hold: connections that never finish a
//! handshake are capped, in all and per source, and so are the sessions
//! of strangers who do finish one, which also end when they fall idle.

use std::time::Duration;

use threnody_core::AppMessage;
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

/// Connections over the internet can't use up the allowance of radio
/// links: with every TCP place taken, a Bluetooth handshake still goes
/// through.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn internet_load_leaves_radio_links_their_own_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, _brx) = node(&dir, "bob");
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let from = |host: u8| async move {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind(([127, 0, 0, host], 0).into()).unwrap();
        sock.connect(addr).await.unwrap()
    };
    // Sixteen silent connections from each of sixteen addresses: every
    // place for TCP handshakes.
    let mut idle = Vec::new();
    for host in 1..=16 {
        for _ in 0..16 {
            idle.push(from(host).await);
        }
    }
    let mut extra = from(17).await;
    assert!(closed(&mut extra).await, "TCP is full");

    let (alice, _arx) = node(&dir, "alice");
    let (x, y) = tokio::io::duplex(1 << 16);
    let b = bob.clone();
    let accepted = tokio::spawn(async move { b.accept_stream(y, "ble", "alice".into()).await });
    timeout(
        Duration::from_secs(10),
        alice.connect_stream(x, "ble", "bob".into(), Some(bob.identity().fingerprint())),
    )
    .await
    .expect("handshake answered")
    .unwrap();
    assert_eq!(
        accepted.await.unwrap().unwrap(),
        alice.identity(),
        "the radio link was let in"
    );
}

/// Waits up to `secs` seconds for `cond`.
async fn until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    timeout(Duration::from_secs(secs), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok()
}

fn has(node: &Node, peer: &Node) -> bool {
    node.sessions().iter().any(|s| s.peer == peer.identity())
}

/// Whether a request carrying `body` from anyone arrives on `rx` within
/// `secs` seconds.
async fn request(rx: &mut UnboundedReceiver<Event>, body: &str, secs: u64) -> bool {
    timeout(Duration::from_secs(secs), async {
        loop {
            match rx.recv().await {
                Some(Event::MessageRequest {
                    msg: AppMessage::Text { body: b, .. },
                    ..
                }) if b == body => return,
                Some(_) => {}
                None => std::future::pending::<()>().await,
            }
        }
    })
    .await
    .is_ok()
}

/// Four strangers from one network hold sessions; a fifth is refused, and
/// what it sends gets through on its own once one of the four goes.
#[tokio::test]
async fn strangers_from_one_network_are_capped_and_requests_wait_for_room() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, mut brx) = node(&dir, "bob");
    let addr = bob.listen("127.0.0.1:0").await.unwrap().to_string();
    let pin = Some(bob.identity().fingerprint());
    let mut four = Vec::new();
    for i in 0..4 {
        let (s, rx) = node(&dir, &format!("s{i}"));
        s.connect(&addr, pin).await.unwrap();
        four.push((s, rx));
    }
    assert!(until(5, || bob.sessions().len() == 4).await);

    let (eve, _erx) = node(&dir, "eve");
    eve.set_retry_after(Duration::from_millis(200));
    // The handshake completes; the session is closed straight after.
    eve.connect(&addr, pin).await.unwrap();
    assert!(until(5, || !has(&eve, &bob)).await, "the fifth is refused");
    assert_eq!(bob.sessions().len(), 4);
    eve.send_text(&bob.identity(), "let me in").unwrap();
    assert!(
        !request(&mut brx, "let me in", 2).await,
        "no room: it waits with eve"
    );
    four[0].0.shutdown();
    assert!(
        request(&mut brx, "let me in", 15).await,
        "a place came free and eve's node tried again"
    );
}

/// At most 64 strangers in all, however many networks they come from;
/// a contact we accepted gets in past the limit.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn strangers_are_capped_in_all_but_contacts_are_not() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, mut brx) = node(&dir, "bob");
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    let pin = Some(bob.identity().fingerprint());
    let from = |host: u8| async move {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind(([127, 0, 0, host], 0).into()).unwrap();
        sock.connect(addr).await.unwrap()
    };
    let mut strangers = Vec::new();
    for host in 1..=16 {
        for i in 0..4 {
            let (s, rx) = node(&dir, &format!("s{host}-{i}"));
            s.connect_stream(from(host).await, "tcp", "bob".into(), pin)
                .await
                .unwrap();
            strangers.push((s, rx));
        }
    }
    assert!(until(10, || bob.sessions().len() == 64).await);
    while brx.try_recv().is_ok() {}

    let (eve, _erx) = node(&dir, "eve");
    eve.connect_stream(from(17).await, "tcp", "bob".into(), pin)
        .await
        .unwrap();
    assert!(until(5, || !has(&eve, &bob)).await, "the 65th is refused");
    let refused = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::Rejected { reason, .. }) = brx.recv().await {
                return reason;
            }
        }
    })
    .await
    .unwrap();
    assert!(refused.contains("too many strangers"), "{refused}");
    assert_eq!(bob.sessions().len(), 64);

    let (carol, _crx) = node(&dir, "carol");
    bob.accept_contact(&carol.identity());
    carol
        .connect_stream(from(17).await, "tcp", "bob".into(), pin)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(has(&bob, &carol) && has(&carol, &bob), "a contact gets in");
}

/// A stranger's session ends once it has carried no messages for the
/// idle time; one that keeps talking stays, and so does an accepted
/// contact's, idle or not.
#[tokio::test]
async fn idle_stranger_sessions_close_and_contacts_stay() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, _brx) = node(&dir, "bob");
    bob.set_stranger_idle(Duration::from_millis(800));
    let addr = bob.listen("127.0.0.1:0").await.unwrap().to_string();
    let pin = Some(bob.identity().fingerprint());
    let (idle, _irx) = node(&dir, "idle");
    let (chatty, _hrx) = node(&dir, "chatty");
    let (carol, _crx) = node(&dir, "carol");
    bob.accept_contact(&carol.identity());
    for n in [&idle, &chatty, &carol] {
        n.connect(&addr, pin).await.unwrap();
    }
    assert!(until(5, || bob.sessions().len() == 3).await);
    for i in 0..8 {
        chatty
            .send_text(&bob.identity(), &format!("still here {i}"))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(!has(&bob, &idle), "the idle stranger was closed");
    assert!(!has(&idle, &bob), "and its node saw it go");
    assert!(has(&bob, &chatty), "the one talking stays");
    assert!(has(&bob, &carol), "an accepted contact stays");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(has(&bob, &carol), "an accepted contact never idles out");
}

/// Cover traffic isn't application traffic: a stranger's session that
/// carries nothing else idles out all the same.
#[tokio::test]
async fn cover_traffic_does_not_keep_a_stranger_session_open() {
    let dir = tempfile::tempdir().unwrap();
    let (bob, _brx) = node(&dir, "bob");
    let (idle, _irx) = node(&dir, "idle");
    // A cover frame each way every 100 ms, well inside the idle time.
    for n in [&bob, &idle] {
        n.set_constant_rate(Some(Duration::from_millis(100)));
    }
    bob.set_stranger_idle(Duration::from_millis(800));
    let addr = bob.listen("127.0.0.1:0").await.unwrap().to_string();
    idle.connect(&addr, Some(bob.identity().fingerprint()))
        .await
        .unwrap();
    assert!(until(5, || has(&bob, &idle)).await);
    assert!(
        until(5, || !has(&bob, &idle)).await,
        "closed for want of messages, cover or not"
    );
}

/// A new device of ours linking while strangers hold every place from
/// its network still gets in. One let in past the limit that doesn't
/// link is closed after the grace.
#[tokio::test]
async fn linking_a_device_gets_past_a_full_strangers_limit() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, _lrx) = node(&dir, "laptop");
    let addr = laptop.listen("127.0.0.1:0").await.unwrap().to_string();
    let pin = Some(laptop.identity().fingerprint());
    let mut four = Vec::new();
    for i in 0..4 {
        let (s, rx) = node(&dir, &format!("s{i}"));
        s.connect(&addr, pin).await.unwrap();
        four.push((s, rx));
    }
    assert!(until(5, || laptop.sessions().len() == 4).await);

    let code = laptop.create_link_code(addr.clone());
    let (phone, _prx) = node(&dir, "phone");
    timeout(Duration::from_secs(20), phone.link_with(&code))
        .await
        .expect("linking finished")
        .expect("the new device linked");
    assert!(until(5, || laptop.is_own_device(&phone.identity())).await);

    // Another code is out, but whoever comes now doesn't link with it.
    let _code = laptop.create_link_code(addr.clone());
    laptop.set_link_grace(Duration::from_millis(800));
    let (eve, _erx) = node(&dir, "eve");
    eve.connect(&addr, pin).await.unwrap();
    assert!(until(5, || has(&laptop, &eve)).await, "let in to link");
    assert!(
        until(5, || !has(&laptop, &eve)).await,
        "and closed when it didn't"
    );
    assert!(has(&laptop, &phone), "the linked device stays");
}
