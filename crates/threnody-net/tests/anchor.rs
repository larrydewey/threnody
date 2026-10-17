//! An always-on anchor (Appendix N): nodes keep a direct session with it,
//! redial it when that drops, and two of them with no path to each other
//! reach each other through it to call.

use std::time::Duration;

use threnody_core::message::FEATURE_CALLS;
use threnody_core::store::Home;
use threnody_net::reach::ReachConfig;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
}

fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
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
    N { node, rx }
}

/// A phone-like node: QUIC for the reach tasks (the anchor's among them),
/// but with the DHT off and no listener anyone could dial.
async fn phone(dir: &tempfile::TempDir, name: &str) -> N {
    let n = spawn(dir, name);
    n.node.set_reach(false);
    n.node.listen_quic("127.0.0.1:0").await.unwrap();
    n.node
        .start_reach(ReachConfig {
            bootstrap: Some(Vec::new()),
            loopback: true,
            ..ReachConfig::default()
        })
        .unwrap();
    n
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

async fn until(what: &str, f: impl Fn() -> bool) {
    timeout(Duration::from_secs(20), async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn approve(x: &mut N, y: &mut N) {
    let yid = y.node.identity();
    x.node.set_approval(&yid, true).unwrap();
    y.node.set_approval(&x.node.identity(), true).unwrap();
    for n in [x, y] {
        next(&mut n.rx, |e| {
            matches!(e, Event::ApprovalChanged { mutual: true, .. })
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phones_reach_each_other_through_their_anchor_to_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut relay = spawn(&dir, "relay");
    let relay_addr = relay.node.listen("127.0.0.1:0").await.unwrap().to_string();
    let r_id = relay.node.identity();
    let (mut alice, mut bob) = (phone(&dir, "alice").await, phone(&dir, "bob").await);
    let b_id = bob.node.identity();

    for p in [&mut alice, &mut bob] {
        p.node.connect(&relay_addr, None).await.unwrap();
        approve(p, &mut relay).await;
    }
    assert!(
        bob.node.set_anchor(Some(alice.node.identity())).is_err(),
        "only a mutually approved contact can be the anchor"
    );
    // Alice and Bob meet through the relay and approve each other; neither
    // ever learns an address for the other.
    alice
        .node
        .connect_relayed(b_id.fingerprint())
        .await
        .unwrap();
    approve(&mut alice, &mut bob).await;
    assert!(alice.node.disconnect(&b_id));
    until("Alice and Bob apart", || {
        !alice.node.sessions().iter().any(|s| s.peer == b_id)
    })
    .await;

    for p in [&alice, &bob] {
        p.node.set_anchor(Some(r_id)).unwrap();
        assert_eq!(p.node.anchor(), Some(r_id));
    }
    // A dropped anchor session is dialed again.
    assert!(relay.node.disconnect(&alice.node.identity()));
    until("Alice back with her anchor", || {
        alice.node.anchor_connected()
    })
    .await;
    until("Bob with his anchor", || bob.node.anchor_connected()).await;

    alice.node.reach_for_call(&b_id).await.unwrap();
    let s = alice
        .node
        .sessions()
        .into_iter()
        .find(|s| s.peer == b_id)
        .expect("a session with Bob");
    assert_eq!(s.via, Some(r_id), "through the anchor");
    until("hellos", || alice.node.supports(&b_id, FEATURE_CALLS)).await;
    let id = alice.node.start_call(&b_id, false).unwrap();
    let Event::CallIncoming { call, .. } =
        next(&mut bob.rx, |e| matches!(e, Event::CallIncoming { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(call, id);

    alice.node.set_anchor(None).unwrap();
    assert_eq!(alice.node.anchor(), None);
}

/// A call that starts over a relay circuit moves to a direct path once
/// the two ends punch one, and carries on there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relayed_call_moves_to_a_direct_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut relay = spawn(&dir, "relay");
    let relay_addr = relay.node.listen("127.0.0.1:0").await.unwrap().to_string();
    let r_id = relay.node.identity();
    let cfg = ReachConfig {
        bootstrap: Some(Vec::new()),
        reflectors: vec![],
        loopback: true,
        ..ReachConfig::default()
    };
    let (mut alice, mut bob) = (spawn(&dir, "alice"), spawn(&dir, "bob"));
    for p in [&mut alice, &mut bob] {
        p.node.listen_quic("127.0.0.1:0").await.unwrap();
        p.node.start_reach(cfg.clone()).unwrap();
        p.node.connect(&relay_addr, None).await.unwrap();
        approve(p, &mut relay).await;
    }
    let (a_id, b_id) = (alice.node.identity(), bob.node.identity());
    alice
        .node
        .connect_relayed(b_id.fingerprint())
        .await
        .unwrap();
    approve(&mut alice, &mut bob).await;
    until("hellos", || {
        alice.node.supports(&b_id, FEATURE_CALLS) && bob.node.supports(&a_id, FEATURE_CALLS)
    })
    .await;
    let via = |n: &Node, p| {
        n.sessions()
            .into_iter()
            .find(|s| s.peer == p)
            .map(|s| s.via)
    };
    assert_eq!(via(&alice.node, b_id), Some(Some(r_id)));

    let id = alice.node.start_call(&b_id, false).unwrap();
    next(&mut bob.rx, |e| matches!(e, Event::CallIncoming { .. })).await;
    bob.node.answer_call(id, false).unwrap();
    next(&mut alice.rx, |e| matches!(e, Event::CallStarted { .. })).await;

    until("a direct session on both ends", || {
        via(&alice.node, b_id) == Some(None) && via(&bob.node, a_id) == Some(None)
    })
    .await;
    assert!(
        alice.node.media_datagrams(&b_id),
        "QUIC: media as datagrams"
    );
    let mut at_bob = bob.node.call_media(id).unwrap();
    assert!(alice.node.send_media(id, 1, b"direct now").unwrap());
    let got = timeout(Duration::from_secs(5), at_bob.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, (1, b"direct now".to_vec()));
    assert!(alice.node.call().is_some(), "the call carried on");
}
