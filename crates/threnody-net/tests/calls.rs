//! Voice and video calls (Appendix Q): signalling, and media over QUIC
//! datagrams or, without them, over the session stream.

use std::time::Duration;

use threnody_core::Identity;
use threnody_core::call::HangupReason;
use threnody_core::message::FEATURE_CALLS;
use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, CallPhase, Event, Node, NodeConfig};
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

type Peer = (Node, UnboundedReceiver<Event>);

/// `caller` dials `callee` (at its QUIC `addr`, or over TCP when `None`
/// and it listens there), and `callee` accepts it if `accept`; returns
/// once both know the other takes calls.
async fn meet(caller: &Node, callee: &Node, quic: Option<std::net::SocketAddr>, accept: bool) {
    let fp = Some(callee.identity().fingerprint());
    let id = match quic {
        Some(addr) => {
            caller.listen_quic("127.0.0.1:0").await.unwrap();
            caller.connect_quic(addr, fp).await.unwrap()
        }
        None => {
            let addr = callee.listen("127.0.0.1:0").await.unwrap();
            caller.connect(&addr.to_string(), fp).await.unwrap()
        }
    };
    if accept {
        callee.accept_contact(&caller.identity());
    }
    timeout(Duration::from_secs(10), async {
        while !(caller.supports(&id, FEATURE_CALLS)
            && callee.supports(&caller.identity(), FEATURE_CALLS))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("hellos exchanged");
}

/// Alice and Bob connected over QUIC or TCP, accepting each other, and
/// Bob's QUIC address.
async fn pair(dir: &tempfile::TempDir, quic: bool) -> (Peer, Peer, std::net::SocketAddr) {
    let (alice, arx) = node(dir, "alice");
    let (bob, brx) = node(dir, "bob");
    let addr = bob.listen_quic("127.0.0.1:0").await.unwrap();
    meet(&alice, &bob, quic.then_some(addr), true).await;
    ((alice, arx), (bob, brx), addr)
}

async fn call_and_talk(quic: bool) {
    let dir = tempfile::tempdir().unwrap();
    let ((alice, mut arx), (bob, mut brx), _) = pair(&dir, quic).await;
    let (a_id, b_id) = (alice.identity(), bob.identity());

    let id = alice.start_call(&b_id, true).unwrap();
    assert!(
        alice.start_call(&b_id, false).is_err(),
        "one call at a time"
    );
    let Event::CallIncoming { peer, call, video } =
        next(&mut brx, 10, |e| matches!(e, Event::CallIncoming { .. })).await
    else {
        unreachable!()
    };
    assert!(peer == a_id && call == id && video);
    next(&mut arx, 10, |e| matches!(e, Event::CallRinging { .. })).await;
    assert_eq!(alice.call().unwrap().phase, CallPhase::Ringing);
    assert_eq!(bob.call().unwrap().phase, CallPhase::Incoming);

    bob.answer_call(id, false).unwrap();
    let Event::CallStarted { peer_video, .. } =
        next(&mut arx, 10, |e| matches!(e, Event::CallStarted { .. })).await
    else {
        unreachable!()
    };
    assert!(!peer_video, "Bob answered without video");
    assert_eq!(alice.media_datagrams(&b_id), quic);
    assert_eq!(bob.media_datagrams(&a_id), quic);
    let mut at_alice = alice.call_media(id).unwrap();
    let mut at_bob = bob.call_media(id).unwrap();
    assert!(bob.call_media(id).is_none(), "taken once");

    // Media both ways, on two streams.
    for i in 0..50u8 {
        assert!(alice.send_media(id, 1, &[i; 160]).unwrap());
        assert!(bob.send_media(id, 2, &[i; 900]).unwrap());
    }
    let mut got = (Vec::new(), Vec::new());
    timeout(Duration::from_secs(10), async {
        while got.0.len() < 50 {
            got.0.push(at_bob.recv().await.unwrap());
        }
        while got.1.len() < 50 {
            got.1.push(at_alice.recv().await.unwrap());
        }
    })
    .await
    .expect("media arrived");
    assert!(got.0.iter().all(|(s, p)| *s == 1 && p.len() == 160));
    assert!(got.1.iter().all(|(s, p)| *s == 2 && p.len() == 900));
    let mut firsts: Vec<u8> = got.0.iter().map(|(_, p)| p[0]).collect();
    firsts.sort_unstable();
    assert_eq!(firsts, (0..50).collect::<Vec<_>>(), "each exactly once");

    bob.set_call_video(id, true).unwrap();
    let Event::CallVideo { video, .. } =
        next(&mut arx, 10, |e| matches!(e, Event::CallVideo { .. })).await
    else {
        unreachable!()
    };
    assert!(video);

    alice.hangup_call(id);
    let Event::CallEnded { reason, by_us, .. } =
        next(&mut brx, 10, |e| matches!(e, Event::CallEnded { .. })).await
    else {
        unreachable!()
    };
    assert!(reason == HangupReason::Ended && !by_us);
    assert!(alice.call().is_none() && bob.call().is_none());
    assert!(alice.send_media(id, 1, b"late").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_over_quic_datagrams() {
    call_and_talk(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_over_a_stream_without_datagrams() {
    call_and_talk(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn declined_busy_and_requests() {
    let dir = tempfile::tempdir().unwrap();
    let ((alice, mut arx), (bob, mut brx), bob_addr) = pair(&dir, true).await;
    let b_id = bob.identity();

    // Declined.
    let id = alice.start_call(&b_id, false).unwrap();
    next(&mut brx, 10, |e| matches!(e, Event::CallIncoming { .. })).await;
    bob.hangup_call(id);
    let Event::CallEnded { reason, .. } =
        next(&mut arx, 10, |e| matches!(e, Event::CallEnded { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(reason, HangupReason::Declined);

    // Busy: Carol calls Bob while he's talking to Alice.
    let id = alice.start_call(&b_id, false).unwrap();
    next(&mut brx, 10, |e| matches!(e, Event::CallIncoming { .. })).await;
    bob.answer_call(id, false).unwrap();
    let (carol, mut crx) = node(&dir, "carol");
    meet(&carol, &bob, Some(bob_addr), true).await;
    carol.start_call(&b_id, false).unwrap();
    let Event::CallEnded { reason, by_us, .. } =
        next(&mut crx, 10, |e| matches!(e, Event::CallEnded { .. })).await
    else {
        unreachable!()
    };
    assert!(reason == HangupReason::Busy && !by_us);
    assert_eq!(bob.call().unwrap().call, id, "still with Alice");
    alice.hangup_call(id);
    next(&mut brx, 10, |e| matches!(e, Event::CallEnded { .. })).await;

    // A stranger (a message request to Bob) calling doesn't ring.
    let (dave, _drx) = node(&dir, "dave");
    meet(&dave, &bob, Some(bob_addr), false).await;
    dave.start_call(&b_id, false).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(bob.call().is_none(), "requests don't ring");
}

/// A call over a TCP session moves to QUIC (for datagrams) when both
/// sides have a QUIC endpoint, and carries on there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn calls_over_tcp_move_to_quic() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    alice.listen_quic("127.0.0.1:0").await.unwrap();
    // Bob listens on TCP and QUIC on the same port, as nodes do.
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    bob.listen_quic(&addr.to_string()).await.unwrap();
    let b_id = alice
        .connect(&addr.to_string(), Some(bob.identity().fingerprint()))
        .await
        .unwrap();
    bob.accept_contact(&alice.identity());
    timeout(Duration::from_secs(10), async {
        while !(alice.supports(&b_id, FEATURE_CALLS)
            && bob.supports(&alice.identity(), FEATURE_CALLS))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(!alice.media_datagrams(&b_id), "a TCP session");

    let id = alice.start_call(&b_id, false).unwrap();
    next(&mut brx, 10, |e| matches!(e, Event::CallIncoming { .. })).await;
    bob.answer_call(id, false).unwrap();
    next(&mut arx, 10, |e| matches!(e, Event::CallStarted { .. })).await;
    timeout(Duration::from_secs(10), async {
        while !(alice.media_datagrams(&b_id) && bob.media_datagrams(&alice.identity())) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("moved to QUIC");
    assert!(alice.sessions().iter().all(|s| s.transport == "quic"));
    // The call survived the new session.
    let mut at_bob = bob.call_media(id).unwrap();
    assert!(alice.send_media(id, 1, b"after the move").unwrap());
    let got = timeout(Duration::from_secs(5), at_bob.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, (1, b"after the move".to_vec()));
    assert_eq!(alice.call().unwrap().phase, CallPhase::Active);
}
