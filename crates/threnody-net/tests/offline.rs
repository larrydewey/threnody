use std::time::Duration;

use threnody_core::AppMessage;
use threnody_core::store::Home;
use threnody_net::mailbox::DepositStatus;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
}

fn open(dir: &tempfile::TempDir, name: &str, create: bool) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = if create {
        home.create_identity(None).unwrap()
    } else {
        home.load_identity(None).unwrap()
    };
    Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap()
}

async fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
    let (node, rx) = open(dir, name, true);
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

async fn wait_for(cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
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
async fn sealed_message_waits_in_a_mailbox_until_the_recipient_returns() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await; // exchanges prekey bundles
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;

    // B goes away entirely.
    a.node.disconnect(&bid);
    r.node.disconnect(&bid);
    next(&mut b.rx, |e| matches!(e, Event::Disconnected { .. })).await;
    next(&mut b.rx, |e| matches!(e, Event::Disconnected { .. })).await;
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;

    assert_eq!(
        a.node
            .send_offline(&bid, &text("while you were out"))
            .unwrap(),
        1
    );
    wait_for(|| r.node.held_messages() == 1).await;

    // B comes back to R only: R delivers, B opens and authenticates A.
    b.node.connect(&r.addr, None).await.unwrap();
    let Event::OfflineMessage { from, via, msg } =
        next(&mut b.rx, |e| matches!(e, Event::OfflineMessage { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(
        (from, via, msg),
        (
            a.node.identity(),
            r.node.identity(),
            text("while you were out")
        )
    );
    wait_for(|| r.node.held_messages() == 0).await;
}

#[tokio::test]
async fn several_mailboxes_deliver_once_and_live_recipients_get_it_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    link(&mut a, &mut b).await;
    for r in [&mut r1, &mut r2] {
        link(&mut a, r).await;
        link(&mut b, r).await;
    }
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.disconnect(&bid);
    next(&mut b.rx, |e| matches!(e, Event::Disconnected { .. })).await;
    wait_for(|| a.node.sessions().iter().all(|s| s.peer != bid)).await;

    // B is still connected to both mailboxes: they forward immediately.
    assert_eq!(a.node.send_offline(&bid, &text("dedup me")).unwrap(), 2);
    next(
        &mut b.rx,
        |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == text("dedup me")),
    )
    .await;
    let dup = timeout(Duration::from_millis(500), async {
        loop {
            if let Some(Event::OfflineMessage { .. }) = b.rx.recv().await {
                return;
            }
        }
    })
    .await;
    assert!(dup.is_err(), "duplicate delivered twice");
}

#[tokio::test]
async fn prekeys_and_bundles_survive_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    b.node.shutdown();

    // A restarts and still has B's bundle; B restarts and can still open.
    let a_id = a.node.identity();
    a.node.shutdown();
    let (a2, _a2rx) = open(&dir, "a", false);
    assert!(a2.can_send_offline(&bid));
    let a2r = a2.connect(&r.addr, None).await.unwrap();
    assert_eq!(a2r, r.node.identity());
    wait_for(|| r.node.sessions().iter().any(|s| s.peer == a_id)).await;
    wait_for(|| r.node.sessions().iter().all(|s| s.peer != bid)).await;
    a2.send_offline(&bid, &text("after restart")).unwrap();
    wait_for(|| r.node.held_messages() == 1).await;

    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&r.addr, None).await.unwrap();
    next(
        &mut b2rx,
        |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == text("after restart")),
    )
    .await;
    let _ = &mut r.rx;
}

#[tokio::test]
async fn mailboxes_decline_unapproved_recipients_and_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await; // r never approves b
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.disconnect(&bid);
    wait_for(|| a.node.sessions().iter().all(|s| s.peer != bid)).await;
    a.node.send_offline(&bid, &text("nope")).unwrap();
    next(&mut a.rx, |e| {
        matches!(
            e,
            Event::DepositReceipt {
                status: DepositStatus::Declined,
                ..
            }
        )
    })
    .await;
    assert_eq!(r.node.held_messages(), 0);
}

#[tokio::test]
async fn deposits_go_through_onion_circuits_and_hide_the_sender() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut m = spawn(&dir, "m").await;
    link(&mut a, &mut b).await;
    link(&mut a, &mut r1).await;
    link(&mut a, &mut r2).await;
    link(&mut a, &mut m).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut m).await;
    link(&mut b, &mut m).await; // only M will hold for B
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.disconnect(&bid);
    m.node.disconnect(&bid);
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && m.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;

    a.node.send_offline(&bid, &text("from nobody")).unwrap();
    let mid = m.node.identity();
    next(&mut a.rx, |e| {
        matches!(
            e,
            Event::DepositReceipt {
                mailbox,
                status: DepositStatus::Held,
                anonymous: true,
                ..
            } if *mailbox == mid
        )
    })
    .await;
    assert_eq!(m.node.held_messages(), 1);
    // M saw the deposit arrive over a circuit, never from A.
    let _ = &mut r1.rx;
    let _ = &mut r2.rx;
    // Bigger than one cell: continued in Data cells.
    let big = "x".repeat(6000);
    a.node.send_offline(&bid, &text(&big)).unwrap();
    wait_for(|| m.node.held_messages() == 2).await;

    b.node.connect(&m.addr, None).await.unwrap();
    let Event::OfflineMessage { from, via, msg } =
        next(&mut b.rx, |e| matches!(e, Event::OfflineMessage { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(
        (from, via, msg),
        (a.node.identity(), mid, text("from nobody"))
    );
    next(
        &mut b.rx,
        |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == text(&big)),
    )
    .await;
}

#[tokio::test]
async fn deposits_are_direct_when_onion_routing_is_off() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut r1 = spawn(&dir, "r1").await;
    let mut r2 = spawn(&dir, "r2").await;
    let mut m = spawn(&dir, "m").await;
    link(&mut a, &mut b).await;
    link(&mut a, &mut r1).await;
    link(&mut a, &mut r2).await;
    link(&mut a, &mut m).await;
    link(&mut r1, &mut r2).await;
    link(&mut r2, &mut m).await;
    link(&mut b, &mut m).await;
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.disconnect(&bid);
    m.node.disconnect(&bid);
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && m.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.set_prefer_onion(false);
    a.node.send_offline(&bid, &text("plain")).unwrap();
    let mid = m.node.identity();
    next(&mut a.rx, |e| {
        matches!(
            e,
            Event::DepositReceipt {
                mailbox,
                status: DepositStatus::Held,
                anonymous: false,
                ..
            } if *mailbox == mid
        )
    })
    .await;
    let _ = (&mut r1.rx, &mut r2.rx);
}
