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

/// Connects x -> y directly and (optionally) approves both ways.
async fn link(x: &mut N, y: &mut N, approve: bool) {
    let yid = x.node.connect(&y.addr, None).await.unwrap();
    next(&mut y.rx, |e| matches!(e, Event::Connected { .. })).await;
    if approve {
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
}

fn text(s: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
    }
}

#[tokio::test]
async fn one_relay_carries_an_end_to_end_session() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut c) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "c").await,
    );
    link(&mut a, &mut b, true).await;
    link(&mut c, &mut b, true).await;
    let cid = c.node.identity();

    assert_eq!(
        a.node.connect_relayed(cid.fingerprint()).await.unwrap(),
        cid
    );
    let Event::Connected { peer, .. } = next(
        &mut c.rx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == a.node.identity()),
    )
    .await
    else {
        unreachable!()
    };
    assert_eq!(peer, a.node.identity());
    let s = a
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == cid)
        .unwrap();
    assert_eq!((s.transport, s.via), ("relay", Some(b.node.identity())));

    a.node.send(&cid, text("through b")).unwrap();
    let Event::Message { peer, msg } =
        next(&mut c.rx, |e| matches!(e, Event::Message { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((peer, msg), (a.node.identity(), text("through b")));
    c.node.send(&a.node.identity(), text("and back")).unwrap();
    next(
        &mut a.rx,
        |e| matches!(e, Event::Message { msg, .. } if *msg == text("and back")),
    )
    .await;
    // The relay never saw a session with either endpoint beyond its own links.
    assert_eq!(b.node.sessions().len(), 2);
}

#[tokio::test]
async fn two_relays_in_a_chain() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut d = spawn(&dir, "d").await;
    let mut c = spawn(&dir, "c").await;
    link(&mut a, &mut b, true).await;
    link(&mut b, &mut d, true).await;
    link(&mut d, &mut c, true).await;
    let cid = c.node.identity();
    a.node.connect_relayed(cid.fingerprint()).await.unwrap();
    a.node.send(&cid, text("two hops")).unwrap();
    next(
        &mut c.rx,
        |e| matches!(e, Event::Message { msg, .. } if *msg == text("two hops")),
    )
    .await;
}

#[tokio::test]
async fn relays_only_serve_approved_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut c) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "c").await,
    );
    link(&mut a, &mut b, false).await; // connected, not approved
    link(&mut c, &mut b, true).await;
    assert!(
        a.node
            .connect_relayed(c.node.identity().fingerprint())
            .await
            .is_err()
    );

    // b approves a, but not the destination: still refused.
    let (mut a2, mut c2) = (spawn(&dir, "a2").await, spawn(&dir, "c2").await);
    link(&mut a2, &mut b, true).await;
    link(&mut c2, &mut b, false).await;
    assert!(
        a2.node
            .connect_relayed(c2.node.identity().fingerprint())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn relayed_session_ends_when_the_relay_link_drops() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut c) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "c").await,
    );
    link(&mut a, &mut b, true).await;
    link(&mut c, &mut b, true).await;
    let cid = c.node.identity();
    a.node.connect_relayed(cid.fingerprint()).await.unwrap();
    next(
        &mut c.rx,
        |e| matches!(e, Event::Connected { peer, .. } if *peer == a.node.identity()),
    )
    .await;
    // Drop the a-b link: the circuit, and so the a-c session, must end.
    a.node.disconnect(&b.node.identity());
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
}

#[tokio::test]
async fn cycles_without_a_path_are_refused_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut d = spawn(&dir, "d").await;
    let mut e = spawn(&dir, "e").await;
    let z = spawn(&dir, "z").await; // never linked
    link(&mut a, &mut b, true).await;
    link(&mut b, &mut d, true).await;
    link(&mut d, &mut e, true).await;
    link(&mut e, &mut b, true).await; // b-d-e-b cycle
    let started = std::time::Instant::now();
    assert!(
        a.node
            .connect_relayed(z.node.identity().fingerprint())
            .await
            .is_err()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "search did not terminate promptly"
    );
}
