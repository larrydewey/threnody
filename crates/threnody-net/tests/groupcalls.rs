//! Group calls (Appendix Q, *Group calls*): legs between every two
//! participants, relayed by the hub when a pair has no session, and a new
//! hub when the hub leaves. The group layer's signalling is played here
//! by calling the node directly.

use std::time::Duration;

use threnody_core::call::wrap_relayed;
use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};
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

async fn connect(a: &Node, b: &Node) {
    let addr = b.listen_quic("127.0.0.1:0").await.unwrap();
    a.listen_quic("127.0.0.1:0").await.unwrap();
    a.connect_quic(addr, Some(b.identity().fingerprint()))
        .await
        .unwrap();
    let (ai, bi) = (a.identity(), b.identity());
    timeout(Duration::from_secs(10), async {
        while !(a.sessions().iter().any(|s| s.peer == bi)
            && b.sessions().iter().any(|s| s.peer == ai))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("connected");
}

async fn recv(rx: &mut Receiver<(u8, Vec<u8>)>) -> Option<(u8, Vec<u8>)> {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .ok()
        .flatten()
}

const GROUP: [u8; 16] = [7; 16];
const CALL: u64 = 42;
const BASE: [u8; 32] = [9; 32];

/// `n` hears that `joined` are in the call (as the group layer would tell it).
fn roster(n: &Node, joined: &[PublicIdentity]) {
    for d in joined {
        n.group_call_joined(CALL, *d, false);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_call_legs_go_direct_or_through_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx) = node(&dir, "a");
    let (b, mut brx) = node(&dir, "b");
    let (c, _crx) = node(&dir, "c");
    let (d, _drx) = node(&dir, "d");
    // A (the hub) reaches everyone; B and C never meet; D isn't in the call.
    connect(&b, &a).await;
    connect(&c, &a).await;
    connect(&d, &a).await;
    let (ai, bi, ci) = (a.identity(), b.identity(), c.identity());

    a.start_group_call(GROUP, CALL, BASE, false).unwrap();
    assert!(b.group_call_rang(GROUP, CALL, ai, ai, BASE, false));
    let Event::GroupCallRinging { from, .. } = timeout(Duration::from_secs(5), async {
        loop {
            let e = brx.recv().await.unwrap();
            if matches!(e, Event::GroupCallRinging { .. }) {
                return e;
            }
        }
    })
    .await
    .unwrap() else {
        unreachable!()
    };
    assert_eq!(from, ai);
    assert!(c.group_call_rang(GROUP, CALL, ai, ai, BASE, false));
    b.join_group_call(CALL, false).unwrap();
    roster(&a, &[bi]);
    roster(&c, &[bi]);
    c.join_group_call(CALL, false).unwrap();
    roster(&a, &[ci]);
    roster(&b, &[ci]);
    assert_eq!(b.group_call().unwrap().participants.len(), 2);

    let mut a_from_b = a.group_leg_media(CALL, &bi).unwrap();
    let mut b_from_a = b.group_leg_media(CALL, &ai).unwrap();
    let mut b_from_c = b.group_leg_media(CALL, &ci).unwrap();
    let mut c_from_b = c.group_leg_media(CALL, &bi).unwrap();
    assert!(
        b.group_leg_media(CALL, &bi).is_none(),
        "no leg to ourselves"
    );

    // Direct legs.
    assert!(b.send_leg_media(CALL, &ai, 1, b"to a").unwrap());
    assert_eq!(recv(&mut a_from_b).await, Some((1, b"to a".to_vec())));
    assert!(a.send_leg_media(CALL, &bi, 1, b"to b").unwrap());
    assert_eq!(recv(&mut b_from_a).await, Some((1, b"to b".to_vec())));
    // B and C through the hub, both ways.
    assert!(b.send_leg_media(CALL, &ci, 2, b"b to c").unwrap());
    assert_eq!(recv(&mut c_from_b).await, Some((2, b"b to c".to_vec())));
    assert!(c.send_leg_media(CALL, &bi, 2, b"c to b").unwrap());
    assert_eq!(recv(&mut b_from_c).await, Some((2, b"c to b".to_vec())));

    // The hub passes on only packets between two participants: D, not in
    // the call, can't use it to reach C.
    let junk = wrap_relayed(ci.as_bytes(), &[1; 64]);
    d.send(&ai, AppMessage::Media(junk)).unwrap();
    // Nor can a participant pass off another leg's packet.
    let mut forged = vec![0u8; 64];
    forged[..8].copy_from_slice(&5u64.to_be_bytes());
    b.send(&ai, AppMessage::Media(wrap_relayed(ci.as_bytes(), &forged)))
        .unwrap();
    assert_eq!(recv(&mut c_from_b).await, None);

    // The hub leaves: the participant with the smaller key takes over.
    a.leave_group_call(CALL);
    let next_b = b.group_call_left(CALL, ai);
    let next_c = c.group_call_left(CALL, ai);
    let expect = if bi.as_bytes() < ci.as_bytes() {
        bi
    } else {
        ci
    };
    assert_eq!(b.group_call().unwrap().hub, expect);
    assert_eq!(c.group_call().unwrap().hub, expect);
    assert_eq!(next_b.or(next_c), Some(expect));
    assert!(next_b.is_none() || next_c.is_none(), "one new hub");
    // B and C now need a path of their own.
    assert!(!b.send_leg_media(CALL, &ci, 1, b"nowhere").unwrap());
    connect(&b, &c).await;
    assert!(b.send_leg_media(CALL, &ci, 1, b"direct").unwrap());
    assert_eq!(recv(&mut c_from_b).await, Some((1, b"direct".to_vec())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_calls_and_one_to_one_calls_exclude_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx) = node(&dir, "a");
    let (b, _brx) = node(&dir, "b");
    connect(&b, &a).await;
    a.accept_contact(&b.identity());
    b.accept_contact(&a.identity());
    a.start_group_call(GROUP, CALL, BASE, false).unwrap();
    timeout(Duration::from_secs(10), async {
        while !b.supports(&a.identity(), threnody_core::message::FEATURE_CALLS) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        a.start_call(&b.identity(), false).is_err(),
        "busy in a group call"
    );
    // B's call to A gets busy.
    b.start_call(&a.identity(), false).unwrap();
    timeout(Duration::from_secs(10), async {
        while b.call().is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("busy");
    // And a group call doesn't ring during a one-to-one call.
    a.leave_group_call(CALL);
    b.start_call(&a.identity(), false).unwrap();
    timeout(Duration::from_secs(10), async {
        while a.call().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(!a.group_call_rang(GROUP, 99, b.identity(), b.identity(), BASE, false));
}
