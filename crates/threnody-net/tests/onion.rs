use std::time::Duration;

use threnody_core::AppMessage;
use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
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

    // Turned off, it dials directly.
    a.node.set_prefer_onion(false);
    a.node.disconnect(&cid);
    next(&mut c.rx, |e| matches!(e, Event::Disconnected { .. })).await;
    a.node
        .reach(Some(&c.addr), Some(cid.fingerprint()))
        .await
        .unwrap();
    assert_eq!(transport(&a), Some("tcp"));
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
}
