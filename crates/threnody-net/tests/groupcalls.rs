//! Group calls (Appendix Q, *Group calls*): legs between every two
//! participants, relayed by a participant linked to both when a pair has
//! no session. The group layer's signalling is played here by calling the
//! node directly.

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

/// `n` hears that `who` is in the call, linked to `links` (as the group
/// layer would tell it).
fn joined(n: &Node, who: PublicIdentity, links: &[PublicIdentity]) {
    n.group_call_joined(CALL, who, false, links.to_vec());
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
    assert!(b.group_call_rang(GROUP, CALL, ai, BASE, false));
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
    assert!(c.group_call_rang(GROUP, CALL, ai, BASE, false));
    b.join_group_call(CALL, false).unwrap();
    joined(&a, bi, &[ai]);
    joined(&c, bi, &[ai]);
    c.join_group_call(CALL, false).unwrap();
    joined(&a, ci, &[ai]);
    joined(&b, ci, &[ai]);
    // A says it reaches both (its answer to their joins).
    joined(&b, ai, &[bi, ci]);
    joined(&c, ai, &[bi, ci]);
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
    // B and C through A, both ways.
    assert!(b.send_leg_media(CALL, &ci, 2, b"b to c").unwrap());
    assert_eq!(recv(&mut c_from_b).await, Some((2, b"b to c".to_vec())));
    assert!(c.send_leg_media(CALL, &bi, 2, b"c to b").unwrap());
    assert_eq!(recv(&mut b_from_c).await, Some((2, b"c to b".to_vec())));

    // A passes on only packets between two participants: D, not in the
    // call, can't use it to reach C.
    let junk = wrap_relayed(ci.as_bytes(), &[1; 64]);
    d.send(&ai, AppMessage::Media(junk)).unwrap();
    // Nor can a participant pass off another leg's packet.
    let mut forged = vec![0u8; 64];
    forged[..8].copy_from_slice(&5u64.to_be_bytes());
    b.send(&ai, AppMessage::Media(wrap_relayed(ci.as_bytes(), &forged)))
        .unwrap();
    assert_eq!(recv(&mut c_from_b).await, None);

    // A is the best linked: the hub. When it leaves, B and C are linked
    // to nobody, and tie.
    assert_eq!(b.group_call().unwrap().hub, ai);
    a.leave_group_call(CALL);
    b.group_call_left(CALL, ai);
    c.group_call_left(CALL, ai);
    let smaller = if bi.as_bytes() < ci.as_bytes() {
        bi
    } else {
        ci
    };
    assert_eq!(b.group_call().unwrap().hub, smaller);
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
    assert!(!a.group_call_rang(GROUP, 99, b.identity(), BASE, false));
}

/// The starter (S) reaches only A, and B reaches only A: S and B's leg goes
/// through A, a participant linked to both, though A didn't start the call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leg_goes_through_whoever_is_linked_to_both() {
    let dir = tempfile::tempdir().unwrap();
    let (s, _srx) = node(&dir, "s");
    let (a, _arx) = node(&dir, "a");
    let (b, _brx) = node(&dir, "b");
    connect(&s, &a).await;
    connect(&b, &a).await;
    let (si, ai, bi) = (s.identity(), a.identity(), b.identity());

    s.start_group_call(GROUP, CALL, BASE, false).unwrap();
    for n in [&a, &b] {
        assert!(n.group_call_rang(GROUP, CALL, si, BASE, false));
        n.join_group_call(CALL, false).unwrap();
    }
    assert_eq!(a.group_call_links(CALL, true), vec![si]);
    // Everyone hears everyone's join and links.
    joined(&s, ai, &[si, bi]);
    joined(&s, bi, &[ai]);
    joined(&a, bi, &[ai]);
    joined(&a, si, &[ai]);
    joined(&b, ai, &[si, bi]);
    joined(&b, si, &[ai]);
    // A's links grew when B joined: the node says so.
    let mut a_links = a.group_call_links(CALL, true);
    a_links.sort_by_key(|p| *p.as_bytes());
    let mut want = vec![si, bi];
    want.sort_by_key(|p| *p.as_bytes());
    assert_eq!(a_links, want);
    assert_eq!(s.group_call().unwrap().hub, ai, "A is linked to both");

    let mut s_from_b = s.group_leg_media(CALL, &bi).unwrap();
    let mut b_from_s = b.group_leg_media(CALL, &si).unwrap();
    assert!(b.send_leg_media(CALL, &si, 3, b"b to s").unwrap());
    assert_eq!(recv(&mut s_from_b).await, Some((3, b"b to s".to_vec())));
    assert!(s.send_leg_media(CALL, &bi, 3, b"s to b").unwrap());
    assert_eq!(recv(&mut b_from_s).await, Some((3, b"s to b".to_vec())));
}

/// A leg over a TCP session we dialed moves to QUIC for datagrams, as a
/// one-to-one call's does (over TCP, one late packet stalls the rest).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_call_legs_over_tcp_move_to_quic() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx) = node(&dir, "a");
    let (b, _brx) = node(&dir, "b");
    a.listen_quic("127.0.0.1:0").await.unwrap();
    let tcp = b.listen("127.0.0.1:0").await.unwrap();
    b.listen_quic(&tcp.to_string()).await.unwrap();
    let bi = a.connect(&tcp.to_string(), None).await.unwrap();
    assert!(!a.media_datagrams(&bi));

    b.start_group_call(GROUP, CALL, BASE, false).unwrap();
    assert!(a.group_call_rang(GROUP, CALL, bi, BASE, false));
    a.join_group_call(CALL, false).unwrap();
    timeout(Duration::from_secs(10), async {
        while !a.media_datagrams(&bi) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the leg's session moved to QUIC");
}

/// A build without call media (it withholds the calls bit) isn't rung and
/// can't start or join a call; a caller is told it can't reach it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn builds_without_calls_are_not_rung_and_cannot_join() {
    use threnody_core::message::FEATURE_CALLS;
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx) = node(&dir, "a");
    let (b, mut brx) = node(&dir, "b");
    b.withhold_features(FEATURE_CALLS);
    connect(&b, &a).await;
    let (ai, bi) = (a.identity(), b.identity());
    timeout(Duration::from_secs(10), async {
        while !a.supports(&bi, threnody_core::message::FEATURE_ACKS) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("b's Hello arrives");

    a.start_group_call(GROUP, CALL, BASE, false).unwrap();
    assert!(!b.group_call_rang(GROUP, CALL, ai, BASE, false));
    assert!(b.group_call().is_none());
    assert!(b.join_group_call(CALL, false).is_err());
    assert!(b.start_group_call(GROUP, CALL + 1, BASE, false).is_err());
    assert!(b.start_call(&ai, false).is_err());
    // Nothing rang on b.
    while let Ok(e) = brx.try_recv() {
        assert!(!matches!(e, Event::GroupCallRinging { .. }), "{e:?}");
    }
    // A one-to-one call to it finds no device to ring.
    a.leave_group_call(CALL);
    assert!(a.start_call(&bi, false).is_err());
}
