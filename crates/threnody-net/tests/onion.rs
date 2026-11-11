use std::time::Duration;

use threnody_core::AppMessage;
use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, FallbackPath, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
}

async fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
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
    N { node, rx, addr }
}

async fn next(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(20), async {
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

async fn link(x: &mut N, y: &mut N) {
    let yid = x.node.connect(&y.addr, None).await.unwrap();
    next(&mut y.rx, |e| matches!(e, Event::Connected { .. })).await;
    x.node.set_approval(&yid, true).unwrap();
    y.node.set_approval(&x.node.identity(), true).unwrap();
    next(&mut x.rx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut y.rx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
}

/// Approves x <-> y over a temporary session, then drops the link.
async fn befriend(x: &mut N, y: &mut N) {
    link(x, y).await;
    let yid = y.node.identity();
    x.node.disconnect(&yid);
    next(&mut y.rx, |e| matches!(e, Event::Disconnected { .. })).await;
}

fn text(s: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
        id: 0,
        quote: None,
        forwarded: false,
        preview: None,
    }
}

#[tokio::test]
async fn two_relay_onion_circuit_carries_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    befriend(&mut a, &mut r2).await; // a knows r2 as a contact, no link
    link(&mut a, &mut r1).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    let cid = c.node.identity();

    let peer = a.node.connect_onion(cid.fingerprint(), 2).await.unwrap();
    assert_eq!(peer, cid);
    next(
        &mut c.rx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == a.node.identity()),
    )
    .await;
    let sa = a
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == cid)
        .unwrap();
    assert_eq!((sa.transport, sa.via), ("onion", Some(r1.node.identity())));
    let sc = c
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == a.node.identity())
        .unwrap();
    assert_eq!(
        sc.via,
        Some(r2.node.identity()),
        "c only sees its own neighbour"
    );
    assert_eq!((r1.node.onion_hops(), r2.node.onion_hops()), (1, 1));
    // r1 has no link to c at all, so it cannot have learned it.
    assert!(r1.node.sessions().iter().all(|s| s.peer != cid));

    c.node.accept_contact(&a.node.identity()); // else a's messages are requests
    a.node.send(&cid, text("through two relays")).unwrap();
    next(
        &mut c.rx,
        |e| matches!(e, Event::Message { msg, .. } if *msg == text("through two relays")),
    )
    .await;
    // A large message spans many fixed-size cells.
    let big = AppMessage::File {
        sent_ms: 0,
        name: "f".into(),
        data: vec![42; 100_000],
        id: 0,
        sensitive: false,
        caption: String::new(),
        album: 0,
        clip: None,
        forwarded: false,
    };
    c.node.send(&a.node.identity(), big.clone()).unwrap();
    next(
        &mut a.rx,
        |e| matches!(e, Event::Message { msg, .. } if *msg == big),
    )
    .await;
}

#[tokio::test]
async fn minimum_relays_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut r, mut c) = (
        spawn(&dir, "a").await,
        spawn(&dir, "r").await,
        spawn(&dir, "c").await,
    );
    link(&mut a, &mut r).await;
    link(&mut r, &mut c).await;
    let cfp = c.node.identity().fingerprint();
    assert!(
        a.node.connect_onion(cfp, 2).await.is_err(),
        "only a one-relay path exists"
    );
    assert_eq!(
        a.node.connect_onion(cfp, 1).await.unwrap(),
        c.node.identity()
    );
    next(
        &mut c.rx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == a.node.identity()),
    )
    .await;
}

#[tokio::test]
async fn circuits_die_with_their_links() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    befriend(&mut a, &mut r2).await;
    link(&mut a, &mut r1).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    let cid = c.node.identity();
    a.node.connect_onion(cid.fingerprint(), 2).await.unwrap();
    next(
        &mut c.rx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == a.node.identity()),
    )
    .await;
    // Cut the middle link: everything downstream and upstream unwinds.
    r1.node.disconnect(&r2.node.identity());
    next(
        &mut a.rx,
        |e| matches!(e, Event::Disconnected { peer, .. } if *peer == cid),
    )
    .await;
    next(
        &mut c.rx,
        |e| matches!(e, Event::Disconnected { peer, .. } if *peer == a.node.identity()),
    )
    .await;
    timeout(Duration::from_secs(5), async {
        while r1.node.onion_hops() + r2.node.onion_hops() > 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("relay state not cleaned up");
}

#[tokio::test]
async fn contacts_are_reached_through_onions_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    befriend(&mut a, &mut r2).await;
    link(&mut a, &mut r1).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    let cid = c.node.identity();
    assert!(a.node.prefer_onion(), "on by default");

    // a could dial c directly, but goes through r1 and r2.
    let peer = a
        .node
        .reach(Some(&c.addr), Some(cid.fingerprint()))
        .await
        .unwrap();
    assert_eq!(peer, cid);
    let transport = |a: &N| {
        a.node
            .sessions()
            .into_iter()
            .find(|s| s.peer == cid)
            .map(|s| s.transport)
    };
    assert_eq!(transport(&a), Some("onion"));

    // Turned off, it dials directly, and that's no fallback.
    a.node.set_prefer_onion(false);
    a.node.disconnect(&cid);
    next(&mut c.rx, |e| matches!(e, Event::Disconnected { .. })).await;
    a.node
        .reach(Some(&c.addr), Some(cid.fingerprint()))
        .await
        .unwrap();
    assert_eq!(transport(&a), Some("tcp"));
    while let Ok(e) = a.rx.try_recv() {
        assert!(!matches!(e, Event::Fallback { .. }), "{e:?}");
    }
}

#[tokio::test]
async fn without_relays_contacts_are_dialled_directly() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut c = spawn(&dir, "c").await;
    befriend(&mut a, &mut c).await;
    let cid = c.node.identity();
    a.node
        .reach(Some(&c.addr), Some(cid.fingerprint()))
        .await
        .unwrap();
    let s = a
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == cid)
        .unwrap();
    assert_eq!(s.transport, "tcp");
    // Onion routing was wanted but impossible: the app hears so.
    next(
        &mut a.rx,
        |e| matches!(e, Event::Fallback { peer, path: FallbackPath::Direct } if *peer == cid),
    )
    .await;
}

/// How our session with `peer` runs, if there is one.
fn transport(x: &N, peer: &N) -> Option<&'static str> {
    let id = peer.node.identity();
    x.node
        .sessions()
        .into_iter()
        .find(|s| s.peer == id)
        .map(|s| s.transport)
}

/// Waits until `x` has a session with `peer`, and says how it runs.
async fn transport_soon(x: &N, peer: &N) -> &'static str {
    timeout(Duration::from_secs(20), async {
        loop {
            if let Some(t) = transport(x, peer) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a session came up")
}

/// a knows r1 (connected), r2 and c (approved, not connected); r1, r2
/// and c are linked in a chain, so a circuit a → r1 → r2 → c exists.
async fn chain(dir: &tempfile::TempDir) -> (N, N, N, N) {
    let mut a = spawn(dir, "a").await;
    let mut r1 = spawn(dir, "r1").await;
    let mut r2 = spawn(dir, "r2").await;
    let mut c = spawn(dir, "c").await;
    befriend(&mut a, &mut r2).await;
    befriend(&mut a, &mut c).await;
    link(&mut a, &mut r1).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    (a, r1, r2, c)
}

#[tokio::test]
async fn background_reconnection_goes_through_a_circuit_when_one_can_be_built() {
    let dir = tempfile::tempdir().unwrap();
    let (a, r1, _r2, c) = chain(&dir).await;
    // c's address is known (a dialed it before): only onion-first keeps
    // a from dialing it there.
    assert!(
        a.node
            .contacts()
            .get(&c.node.identity())
            .unwrap()
            .last_addr
            .is_some()
    );
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("onion"));
    let via = a
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == c.node.identity())
        .and_then(|s| s.via);
    assert_eq!(via, Some(r1.node.identity()));
}

#[tokio::test]
async fn background_reconnection_goes_direct_when_no_circuit_can_be_built() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut c = spawn(&dir, "c").await;
    // One approved contact besides c: too few relays for a circuit.
    befriend(&mut a, &mut c).await;
    link(&mut a, &mut r1).await;
    link(&mut r1, &mut c).await;
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("tcp"));
}

#[tokio::test]
async fn background_reconnection_goes_direct_while_onion_routing_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _r1, _r2, c) = chain(&dir).await;
    a.node.set_prefer_onion(false);
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("tcp"));
}

/// With no session at all (just started), one reconnection goes direct
/// to give the other a first hop, and the other then goes through a
/// circuit.
#[tokio::test]
async fn the_first_background_reconnection_brings_up_a_first_hop() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    befriend(&mut a, &mut r1).await;
    befriend(&mut a, &mut r2).await;
    befriend(&mut a, &mut c).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    assert!(a.node.sessions().is_empty());
    let (r1id, cid) = (r1.node.identity(), c.node.identity());
    let (x, y) = tokio::join!(a.node.reconnect_peer(&r1id), a.node.reconnect_peer(&cid));
    x.unwrap();
    y.unwrap();
    let mut ways = [transport(&a, &r1).unwrap(), transport(&a, &c).unwrap()];
    ways.sort_unstable();
    assert_eq!(ways, ["onion", "tcp"]);
}

/// After a network change, sessions from before may be dead: they aren't
/// first hops until something has come back on the new network.
#[tokio::test]
async fn sessions_from_before_a_network_change_are_no_first_hops() {
    let dir = tempfile::tempdir().unwrap();
    let (a, r1, _r2, c) = chain(&dir).await;
    a.node.network_changed();
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("tcp"), "c is the first hop now");
    // r1's old session still stands; a fresh one from the new network
    // would make circuits possible again (as c's is, for r1).
    assert_eq!(transport(&a, &r1), Some("tcp"));
}

/// A contact found on the LAN is reached through a circuit when one can
/// be built, like any background reconnection.
#[tokio::test]
async fn lan_discovery_goes_through_a_circuit_when_one_can_be_built() {
    use threnody_net::DiscoveryConfig;
    let dir = tempfile::tempdir().unwrap();
    let mut x = spawn(&dir, "x").await;
    let mut y = spawn(&dir, "y").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    // Whichever of x and y dials has a first hop and two other contacts:
    // x → r1 → r2 → y, or y → r2 → r1 → x.
    befriend(&mut x, &mut y).await;
    befriend(&mut x, &mut r2).await;
    befriend(&mut y, &mut r1).await;
    link(&mut x, &mut r1).await;
    link(&mut y, &mut r2).await;
    link(&mut r1, &mut r2).await;
    let keyed = |a: &N, b: &N| {
        a.node
            .contacts()
            .get(&b.node.identity())
            .is_some_and(|c| c.discovery_key.is_some())
    };
    assert!(keyed(&x, &y) && keyed(&y, &x));
    let port = || {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (px, py) = (port(), port());
    let cfg = |me: u16, other: u16| DiscoveryConfig {
        bind: format!("127.0.0.1:{me}").parse().unwrap(),
        targets: vec![format!("127.0.0.1:{other}").parse().unwrap()],
        interval: Duration::from_millis(100),
        auto_connect: true,
    };
    let tcp = |n: &N| n.addr.parse::<std::net::SocketAddr>().unwrap().port();
    x.node.start_discovery(cfg(px, py), tcp(&x)).unwrap();
    y.node.start_discovery(cfg(py, px), tcp(&y)).unwrap();
    assert_eq!(transport_soon(&x, &y).await, "onion");
    assert_eq!(transport_soon(&y, &x).await, "onion");
}

/// A TCP proxy to `to` that `cut` silences: it then carries nothing
/// either way but keeps both connections open, as a session over a
/// network we left looks until its lease runs out.
async fn proxy(to: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    async fn pipe(
        mut r: impl AsyncRead + Unpin,
        mut w: impl AsyncWrite + Unpin,
        cut: Arc<AtomicBool>,
    ) {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match r.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            if !cut.load(Ordering::Relaxed) && w.write_all(&buf[..n]).await.is_err() {
                return;
            }
        }
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let cut = Arc::new(AtomicBool::new(false));
    let c = cut.clone();
    tokio::spawn(async move {
        while let Ok((inb, _)) = l.accept().await {
            let out = tokio::net::TcpStream::connect(&to).await.unwrap();
            let (ir, iw) = inb.into_split();
            let (or, ow) = out.into_split();
            tokio::spawn(pipe(ir, ow, c.clone()));
            tokio::spawn(pipe(or, iw, c.clone()));
        }
    });
    (addr, cut)
}

/// After a network change, a circuit starts only on a session from the
/// new network, however many dead ones from the old are still listed.
#[tokio::test]
async fn background_circuits_skip_first_hops_from_before_a_network_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut s1 = spawn(&dir, "s1").await;
    let mut s2 = spawn(&dir, "s2").await;
    let mut f = spawn(&dir, "f").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    for x in [&mut s1, &mut s2, &mut f, &mut r2, &mut c] {
        befriend(&mut a, x).await;
    }
    link(&mut f, &mut r2).await;
    link(&mut r2, &mut c).await;
    // Sessions with s1 and s2 that go dead without closing.
    let mut cuts = Vec::new();
    for s in [&s1, &s2] {
        let (addr, cut) = proxy(s.addr.clone()).await;
        a.node
            .connect(&addr, Some(s.node.identity().fingerprint()))
            .await
            .unwrap();
        cuts.push(cut);
    }
    for cut in &cuts {
        cut.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    a.node.network_changed();
    tokio::time::sleep(Duration::from_millis(5)).await;
    // f comes back on the new network.
    a.node
        .connect(&f.addr, Some(f.node.identity().fingerprint()))
        .await
        .unwrap();
    assert_eq!(transport(&a, &s1), Some("tcp"));
    assert_eq!(transport(&a, &s2), Some("tcp"));
    let start = std::time::Instant::now();
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("onion"), "through f, not direct");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
}

/// A network change that wasn't one (the platform's signal for any route
/// change): a session heard from since is a first hop again.
#[tokio::test]
async fn sessions_heard_from_after_a_network_change_are_first_hops_again() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, r1, _r2, c) = chain(&dir).await;
    a.node.network_changed();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let aid = a.node.identity();
    r1.node.send_text(&aid, "still here").unwrap();
    next(&mut a.rx, |e| matches!(e, Event::Message { .. })).await;
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("onion"));
}

/// Approves `x` and `y` both ways, then leaves `x` with an address for
/// `y` that accepts connections but never answers (an unreachable
/// contact, slow to fail), and shuts `y` down.
async fn unreachable(
    x: &mut N,
    mut y: N,
) -> (std::net::TcpListener, threnody_core::PublicIdentity) {
    befriend(x, &mut y).await;
    let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let to = hang.local_addr().unwrap().to_string();
    let yid = y.node.identity();
    x.node
        .update_contacts(|cs| cs.get_mut(&yid).unwrap().last_addr = Some(to));
    y.node.shutdown();
    (hang, yid)
}

/// Contacts that don't answer don't hold up those that do: their turns
/// to bring up a first hop end with their dials, and a second one may go
/// a second after the first.
#[tokio::test]
async fn unreachable_contacts_do_not_hold_up_background_reconnections() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut c = spawn(&dir, "c").await;
    let o1 = spawn(&dir, "o1").await;
    let mut o2 = spawn(&dir, "o2").await;
    for x in [&mut r1, &mut r2, &mut c] {
        befriend(&mut a, x).await;
    }
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut c).await;
    // o1 never answers; o2 is gone from where a last saw it (refused).
    let (_hang, o1id) = unreachable(&mut a, o1).await;
    befriend(&mut a, &mut o2).await;
    let o2id = o2.node.identity();
    o2.node.shutdown();
    assert!(a.node.sessions().is_empty());
    let start = std::time::Instant::now();
    for o in [o1id, o2id] {
        let node = a.node.clone();
        tokio::spawn(async move { node.reconnect_peer(&o).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    a.node.reconnect_peer(&r1.node.identity()).await.unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    a.node.reconnect_peer(&c.node.identity()).await.unwrap();
    assert_eq!(transport(&a, &c), Some("onion"), "r1 is the first hop");
}

/// LAN discovery doesn't wait behind reconnections over the internet for
/// a turn to bring up a first hop: the LAN peer is one.
#[tokio::test]
async fn lan_discovery_does_not_wait_for_a_first_hop_turn() {
    use threnody_net::DiscoveryConfig;
    let dir = tempfile::tempdir().unwrap();
    let mut x = spawn(&dir, "x").await;
    let mut y = spawn(&dir, "y").await;
    let mut h = spawn(&dir, "h").await;
    let mut r = spawn(&dir, "r").await;
    befriend(&mut x, &mut y).await;
    for n in [&mut x, &mut y] {
        befriend(n, &mut h).await;
        befriend(n, &mut r).await;
    }
    r.node.shutdown();
    // Both are reconnecting to h, which never answers: each holds the
    // turn to dial a first hop.
    let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let to = hang.local_addr().unwrap().to_string();
    let hid = h.node.identity();
    h.node.shutdown();
    for n in [&x, &y] {
        n.node
            .update_contacts(|cs| cs.get_mut(&hid).unwrap().last_addr = Some(to.clone()));
        let node = n.node.clone();
        tokio::spawn(async move { node.reconnect_peer(&hid).await });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let port = || {
        std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    };
    let (px, py) = (port(), port());
    let cfg = |me: u16, other: u16| DiscoveryConfig {
        bind: format!("127.0.0.1:{me}").parse().unwrap(),
        targets: vec![format!("127.0.0.1:{other}").parse().unwrap()],
        interval: Duration::from_millis(100),
        auto_connect: true,
    };
    let tcp = |n: &N| n.addr.parse::<std::net::SocketAddr>().unwrap().port();
    let start = std::time::Instant::now();
    x.node.start_discovery(cfg(px, py), tcp(&x)).unwrap();
    y.node.start_discovery(cfg(py, px), tcp(&y)).unwrap();
    assert_eq!(transport_soon(&x, &y).await, "tcp");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    drop(hang);
}
