use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity};
use threnody_net::mailbox::DepositStatus;
use threnody_net::{AcceptPolicy, Event, FallbackPath, Node, NodeConfig};
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
        quote: None,
        forwarded: false,
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
async fn anonymous_deposits_do_not_tell_whether_the_recipient_is_online() {
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
    // B is away from A, but still connected to M.
    a.node.disconnect(&bid);
    wait_for(|| a.node.sessions().iter().all(|s| s.peer != bid)).await;

    a.node.send_offline(&bid, &text("are you there")).unwrap();
    let mid = m.node.identity();
    let Event::DepositReceipt { status, .. } = next(
        &mut a.rx,
        |e| matches!(e, Event::DepositReceipt { mailbox, anonymous: true, .. } if *mailbox == mid),
    )
    .await
    else {
        unreachable!()
    };
    // Whoever deposits anonymously could be anyone: M doesn't say B is
    // online, though it passed the message straight on.
    assert_eq!(status, DepositStatus::Held);
    next(&mut b.rx, |e| matches!(e, Event::OfflineMessage { .. })).await;
    assert_eq!(m.node.held_messages(), 0);
    let _ = (&mut r1.rx, &mut r2.rx);
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
    // Onion routing off is the user's choice: nothing to warn about.
    while let Ok(e) = a.rx.try_recv() {
        assert!(!matches!(e, Event::Fallback { .. }), "{e:?}");
    }
    let _ = (&mut r1.rx, &mut r2.rx);
}

#[tokio::test]
async fn deposits_without_a_circuit_go_direct_and_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut m = spawn(&dir, "m").await;
    link(&mut a, &mut b).await;
    link(&mut a, &mut m).await;
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
    // M is A's only live neighbour: no circuit can reach it, so the
    // deposit goes straight to it, and the app hears so.
    assert!(a.node.prefer_onion());
    a.node.send_offline(&bid, &text("in the open")).unwrap();
    let mid = m.node.identity();
    next(&mut a.rx, |e| {
        matches!(
            e,
            Event::Fallback {
                peer,
                path: FallbackPath::Deposit { mailbox },
            } if *peer == bid && *mailbox == mid
        )
    })
    .await;
}

/// A message with no timer that arrives through a mailbox after a restart,
/// before any session with its sender, doesn't turn our timer off: the
/// sender's `Hello` bits are kept with the contact.
#[tokio::test]
async fn a_mailbox_message_after_a_restart_keeps_our_timer() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid)).await;
    // B chose an hour; A never chose a timer.
    b.node.set_timer(&aid, Some(3600)).unwrap();
    b.node.shutdown();
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.send_offline(&bid, &text("no timer here")).unwrap();
    wait_for(|| r.node.held_messages() == 1).await;

    // B restarts and hears from A only through R.
    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&r.addr, None).await.unwrap();
    next(
        &mut b2rx,
        |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == text("no timer here")),
    )
    .await;
    assert!(b2.sessions().iter().all(|s| s.peer != aid));
    assert_eq!(b2.timer(&aid), Some(3600), "B's choice stands");
    let last = b2
        .history(b2.conversation_for(&aid))
        .unwrap()
        .entries()
        .last()
        .cloned()
        .unwrap();
    assert_eq!(last.text, "no timer here");
    assert_eq!(last.expires_at_ms.map(|t| t - last.at_ms), Some(3_600_000));
    let _ = (&mut a.rx, &mut r.rx);
}

#[tokio::test]
async fn changes_to_a_chat_are_opened_from_a_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.send_text(&bid, "pin me").unwrap();
    next(&mut b.rx, |e| matches!(e, Event::Message { .. })).await;
    let conv = a.node.conversation_for(&bid);
    let id = a.node.history(conv).unwrap().entries()[0].message_id();

    // B goes away; A pins the message, sealed for R to hold, then A goes
    // too, so only the mailbox can tell B.
    a.node.disconnect(&bid);
    r.node.disconnect(&bid);
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    assert!(a.node.pin(&bid, id, true));
    wait_for(|| r.node.held_messages() == 1).await;
    a.node.shutdown();

    b.node.connect(&r.addr, None).await.unwrap();
    wait_for(|| b.node.pinned(b.node.conversation_for(&aid)).len() == 1).await;
}

#[tokio::test]
async fn a_mailbox_copy_of_a_change_does_not_undo_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid)).await;
    a.node.send_text(&bid, "pin me").unwrap();
    next(&mut b.rx, |e| matches!(e, Event::Message { .. })).await;
    let conv = a.node.conversation_for(&bid);
    let id = a.node.history(conv).unwrap().entries()[0].message_id();
    let pinned = |n: &Node| n.pinned(n.conversation_for(&aid)).len();

    // B goes away; A pins the message, held for B and sealed for R.
    a.node.disconnect(&bid);
    r.node.disconnect(&bid);
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    assert!(a.node.pin(&bid, id, true));
    wait_for(|| r.node.held_messages() == 1).await;

    // B meets A first and takes the held pin; A then unpins.
    b.node.connect(&a.addr, None).await.unwrap();
    wait_for(|| pinned(&b.node) == 1).await;
    assert!(a.node.pin(&bid, id, false));
    wait_for(|| pinned(&b.node) == 0).await;

    // The older copy R still holds must not pin it again.
    b.node.connect(&r.addr, None).await.unwrap();
    wait_for(|| r.node.held_messages() == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(pinned(&b.node), 0, "a stale mailbox copy pinned it again");
}

/// Nothing from `rx` matching `pred` for a while.
async fn quiet(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> bool {
    timeout(Duration::from_millis(800), async {
        loop {
            match rx.recv().await {
                Some(e) if pred(&e) => return,
                Some(_) => {}
                None => std::future::pending().await,
            }
        }
    })
    .await
    .is_err()
}

#[tokio::test]
async fn a_blocked_contact_cannot_write_through_a_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid)).await;

    // B blocks A, who still holds B's prekeys and seals through R.
    b.node.block_contact(&aid);
    wait_for(|| a.node.sessions().iter().all(|s| s.peer != bid)).await;
    a.node.send_offline(&bid, &text("let me in")).unwrap();
    next(&mut a.rx, |e| matches!(e, Event::DepositReceipt { .. })).await;
    assert!(
        quiet(&mut b.rx, |e| matches!(
            e,
            Event::OfflineMessage { .. }
                | Event::MessageRequest { .. }
                | Event::TimerChanged { .. }
        ))
        .await,
        "a blocked contact's sealed message got through"
    );
    assert!(
        b.node
            .history(b.node.conversation_for(&aid))
            .map_or(true, |h| h.entries().is_empty()),
        "recorded in history"
    );
    let _ = &mut r.rx;
}

#[tokio::test]
async fn a_mailbox_drops_what_it_holds_for_a_contact_it_deletes() {
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
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.send_offline(&bid, &text("for b")).unwrap();
    wait_for(|| r.node.held_messages() == 1).await;
    r.node.delete_conversation(&bid);
    assert_eq!(r.node.held_messages(), 0);
    let _ = (&mut a.rx, &mut r.rx);
}

#[tokio::test]
async fn a_mailbox_keeps_a_large_message_until_the_recipient_acknowledges_it() {
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
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.set_prefer_onion(false);
    let big = "x".repeat(90_000);
    a.node.send_offline(&bid, &text(&big)).unwrap();
    a.node.send_offline(&bid, &text("small")).unwrap();
    wait_for(|| r.node.held_messages() == 2).await;

    // B comes back; R delivers both, and lets the large one go only
    // once B acknowledges it (a restart before then must not lose it).
    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&r.addr, None).await.unwrap();
    for want in [text(&big), text("small")] {
        next(
            &mut b2rx,
            |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == want),
        )
        .await;
    }
    wait_for(|| r.node.held_messages() == 0).await;
    let _ = (&mut a.rx, &mut r.rx);
}

#[tokio::test]
async fn a_message_every_mailbox_declines_waits_for_the_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await; // r never approves b: it declines
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    b.node.shutdown();
    wait_for(|| a.node.sessions().iter().all(|s| s.peer != bid)).await;

    let sent = a.node.send_text(&bid, "declined but not lost").unwrap();
    assert_eq!(sent.sealed, 1);
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

    // B comes back to A directly, and gets it.
    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&a.addr, None).await.unwrap();
    next(&mut b2rx, |e| {
        matches!(e, Event::Message { msg: AppMessage::Text { body, .. }, .. } if body == "declined but not lost")
    })
    .await;
    let _ = &mut r.rx;
}

/// A TCP stream that stops passing on what we write once `cut` is set,
/// or once more than `cut_after` bytes have come in: the link looks alive
/// to us, but the other end hears nothing more.
struct Cut {
    inner: tokio::net::TcpStream,
    read: usize,
    cut_after: usize,
    cut: Arc<AtomicBool>,
}

impl tokio::io::AsyncRead for Cut {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        self.read += buf.filled().len() - before;
        if self.read > self.cut_after {
            self.cut.store(true, Ordering::SeqCst);
        }
        r
    }
}

impl tokio::io::AsyncWrite for Cut {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.cut.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(buf.len()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// `x` connects to `addr` over a [`Cut`] stream; returns the switch.
async fn connect_cut(x: &Node, addr: &str, cut_after: usize) -> (PublicIdentity, Arc<AtomicBool>) {
    let cut = Arc::new(AtomicBool::new(false));
    let stream = Cut {
        inner: tokio::net::TcpStream::connect(addr).await.unwrap(),
        read: 0,
        cut_after,
        cut: cut.clone(),
    };
    let peer = x
        .connect_stream(stream, "tcp", addr.to_owned(), None)
        .await
        .unwrap();
    (peer, cut)
}

/// [`link`], with `x`'s side of the link a [`Cut`] stream.
async fn link_cut(x: &mut N, y: &mut N) -> Arc<AtomicBool> {
    let (yid, cut) = connect_cut(&x.node, &y.addr, usize::MAX).await;
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
    cut
}

/// A, B and mailbox M, all linked, A's link to M a [`Cut`] one; B has
/// gone away and A deposits directly.
async fn stalled_mailbox(dir: &tempfile::TempDir) -> (N, N, N, Arc<AtomicBool>) {
    let (mut a, mut b, mut m) = (
        spawn(dir, "a").await,
        spawn(dir, "b").await,
        spawn(dir, "m").await,
    );
    link(&mut a, &mut b).await;
    link(&mut b, &mut m).await;
    let cut = link_cut(&mut a, &mut m).await;
    let bid = b.node.identity();
    wait_for(|| a.node.can_send_offline(&bid)).await;
    b.node.shutdown();
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && m.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.set_prefer_onion(false);
    (a, b, m, cut)
}

#[tokio::test]
async fn a_deposit_whose_receipt_never_comes_is_withdrawn() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, b, m, cut) = stalled_mailbox(&dir).await;
    let bid = b.node.identity();
    a.node.set_receipt_wait(Duration::from_millis(300));
    // M never hears of it: A's link to M has gone quiet.
    cut.store(true, Ordering::SeqCst);
    assert_eq!(a.node.send_text(&bid, "only once").unwrap().sealed, 1);
    tokio::time::sleep(Duration::from_secs(1)).await;

    // M comes back. A's queue to it no longer has the deposit, as A now
    // keeps the message for B itself.
    m.node.shutdown();
    let (m2, mut m2rx) = open(&dir, "m", false);
    let maddr = m2.listen("127.0.0.1:0").await.unwrap().to_string();
    a.node.connect(&maddr, None).await.unwrap();
    a.node.send_offline(&bid, &text("probe")).unwrap();
    next(&mut a.rx, |e| {
        matches!(
            e,
            Event::DepositReceipt {
                status: DepositStatus::Held,
                ..
            }
        )
    })
    .await;
    assert_eq!(m2.held_messages(), 1, "only the probe");

    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&a.addr, None).await.unwrap();
    next(&mut b2rx, |e| {
        matches!(e, Event::Message { msg: AppMessage::Text { body, .. }, .. } if body == "only once")
    })
    .await;
    let _ = &mut m2rx;
}

#[tokio::test]
async fn a_placement_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, _m, cut) = stalled_mailbox(&dir).await;
    let bid = b.node.identity();
    cut.store(true, Ordering::SeqCst);
    assert_eq!(
        a.node.send_text(&bid, "across a restart").unwrap().sealed,
        1
    );
    // A stops before any receipt; nothing came of the deposit.
    a.node.shutdown();

    let (a2, _a2rx) = open(&dir, "a", false);
    let addr = a2.listen("127.0.0.1:0").await.unwrap().to_string();
    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&addr, None).await.unwrap();
    next(&mut b2rx, |e| {
        matches!(e, Event::Message { msg: AppMessage::Text { body, .. }, .. } if body == "across a restart")
    })
    .await;
}

#[tokio::test]
async fn a_text_that_comes_twice_shows_once() {
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
    wait_for(|| {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;
    a.node.set_prefer_onion(false);
    let twice = AppMessage::Text {
        sent_ms: 0,
        body: "twice".into(),
        expires_in_s: None,
        id: 77,
        quote: None,
        forwarded: false,
    };
    a.node.send_offline(&bid, &twice).unwrap();
    a.node.send_offline(&bid, &text("after")).unwrap();
    wait_for(|| r.node.held_messages() == 2).await;

    // B gets it from A directly first, then from the mailbox.
    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.connect(&a.addr, None).await.unwrap();
    wait_for(|| a.node.sessions().iter().any(|s| s.peer == bid)).await;
    a.node.send(&bid, twice.clone()).unwrap();
    next(
        &mut b2rx,
        |e| matches!(e, Event::Message { msg, .. } if *msg == twice),
    )
    .await;
    b2.connect(&r.addr, None).await.unwrap();
    let mut shown = 0;
    timeout(Duration::from_secs(20), async {
        loop {
            match b2rx.recv().await.unwrap() {
                Event::OfflineMessage { msg, .. } if msg == text("after") => break,
                Event::OfflineMessage { msg, .. } | Event::Message { msg, .. } if msg == twice => {
                    shown += 1;
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(shown, 0, "the mailbox's copy showed again");
    let _ = &mut r.rx;
}

#[tokio::test]
async fn a_large_message_survives_a_mailbox_restart_before_the_ack() {
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
    wait_for(|| r.node.sessions().iter().all(|s| s.peer != bid)).await;
    a.node.set_prefer_onion(false);
    let big = text(&"x".repeat(90_000));
    a.node.send_offline(&bid, &big).unwrap();
    wait_for(|| r.node.held_messages() == 1).await;

    // B gets it, but its acknowledgement never reaches R: the link goes
    // quiet once most of the message is in.
    let (b2, mut b2rx) = open(&dir, "b", false);
    connect_cut(&b2, &r.addr, 60_000).await;
    next(
        &mut b2rx,
        |e| matches!(e, Event::OfflineMessage { msg, .. } if *msg == big),
    )
    .await;
    // R restarts: it still has the message, and B's next session takes
    // it (as a copy B drops) and acknowledges it.
    r.node.shutdown();
    let (r2, _r2rx) = open(&dir, "r", false);
    assert_eq!(r2.held_messages(), 1);
    let raddr = r2.listen("127.0.0.1:0").await.unwrap().to_string();
    wait_for(|| b2.sessions().iter().all(|s| s.peer != r.node.identity())).await;
    b2.connect(&raddr, None).await.unwrap();
    wait_for(|| r2.held_messages() == 0).await;
    assert!(quiet(&mut b2rx, |e| matches!(e, Event::OfflineMessage { .. })).await);
    let _ = (&mut a.rx, &mut r.rx);
}

// ----- Receipts through mailboxes (Appendix H, *Receipts*) -----

/// Nothing from `rx` matching `pred` for `secs` seconds.
async fn quiet_for(
    rx: &mut UnboundedReceiver<Event>,
    secs: u64,
    pred: impl Fn(&Event) -> bool,
) -> bool {
    timeout(Duration::from_secs(secs), async {
        loop {
            match rx.recv().await {
                Some(e) if pred(&e) => return,
                Some(_) => {}
                None => std::future::pending().await,
            }
        }
    })
    .await
    .is_err()
}

/// A, B and mailbox R, all linked, each holding the other's prekeys.
async fn three(dir: &tempfile::TempDir) -> (N, N, N) {
    let (mut a, mut b, mut r) = (
        spawn(dir, "a").await,
        spawn(dir, "b").await,
        spawn(dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid) && b.node.can_send_offline(&aid)).await;
    (a, b, r)
}

fn gone(n: &Node, peer: &PublicIdentity) -> bool {
    n.sessions().iter().all(|s| s.peer != *peer)
}

/// B goes away; A seals "hi" for it through R, then goes away too. B
/// comes back (reopened, with the given receipt delay) to R only and opens
/// it. Returns B's node and events, and A's history id for the message.
async fn sealed_while_away(
    dir: &tempfile::TempDir,
    a: &N,
    b: &N,
    r: &N,
    delay: u64,
) -> (Node, UnboundedReceiver<Event>, u64) {
    let (aid, bid) = (a.node.identity(), b.node.identity());
    b.node.shutdown();
    wait_for(|| gone(&a.node, &bid) && gone(&r.node, &bid)).await;
    assert_eq!(a.node.send_text(&bid, "hi").unwrap().sealed, 1);
    wait_for(|| r.node.held_messages() == 1).await;
    let local_id = a
        .node
        .history(a.node.conversation_for(&bid))
        .unwrap()
        .entries()
        .last()
        .unwrap()
        .local_id;
    a.node.shutdown();
    wait_for(|| gone(&r.node, &aid)).await;

    let (b2, mut b2rx) = open(dir, "b", false);
    b2.set_receipt_delay(delay, delay);
    b2.connect(&r.addr, None).await.unwrap();
    let Event::OfflineMessage {
        msg: AppMessage::Text { id, .. },
        ..
    } = next(&mut b2rx, |e| matches!(e, Event::OfflineMessage { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(id, local_id);
    (b2, b2rx, local_id)
}

/// A comes back (reopened) and connects to R.
async fn back_to(dir: &tempfile::TempDir, r: &N) -> (Node, UnboundedReceiver<Event>) {
    let (a2, a2rx) = open(dir, "a", false);
    a2.connect(&r.addr, None).await.unwrap();
    (a2, a2rx)
}

fn entry(n: &Node, peer: &PublicIdentity, local_id: u64) -> threnody_core::history::Entry {
    n.history(n.conversation_for(peer))
        .unwrap()
        .entries()
        .iter()
        .find(|e| e.outgoing && e.local_id == local_id)
        .cloned()
        .unwrap()
}

#[tokio::test]
async fn mailbox_message_gets_a_delivered_tick_through_mailboxes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let bid = b.node.identity();
    let (_b2, _b2rx, local_id) = sealed_while_away(&dir, &a, &b, &r, 0).await;
    // B, restarted, still knows A takes receipts: one waits at R for A.
    wait_for(|| r.node.held_messages() == 1).await;

    let (a2, mut a2rx) = back_to(&dir, &r).await;
    let Event::Delivered {
        peer,
        local_id: got,
        ..
    } = next(&mut a2rx, |e| matches!(e, Event::Delivered { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((peer, got), (bid, local_id));
    let e = entry(&a2, &bid, local_id);
    assert!(e.delivered && e.read_ms == 0);
}

#[tokio::test]
async fn mailbox_read_receipt_reaches_an_offline_sender() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    let (b2, _b2rx, local_id) = sealed_while_away(&dir, &a, &b, &r, 60_000).await;
    assert_eq!(b2.pending_receipts(), 1);
    // No session with A: the read waits, with the delivery, as one.
    b2.report_read(&aid, &[local_id]).unwrap();
    assert_eq!(b2.pending_receipts(), 1);
    b2.flush_receipts_now();
    wait_for(|| r.node.held_messages() == 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(r.node.held_messages(), 1, "one sealed receipt for both");
    assert_eq!(b2.pending_receipts(), 0);

    let (a2, mut a2rx) = back_to(&dir, &r).await;
    next(&mut a2rx, |e| matches!(e, Event::Delivered { .. })).await;
    next(&mut a2rx, |e| matches!(e, Event::Read { .. })).await;
    let e = entry(&a2, &bid, local_id);
    assert!(e.delivered && e.read_ms != 0);
}

#[tokio::test]
async fn read_receipts_off_send_no_read_through_mailboxes() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    let (b2, _b2rx, local_id) = sealed_while_away(&dir, &a, &b, &r, 60_000).await;
    b2.set_send_read_receipts(false);
    b2.report_read(&aid, &[local_id]).unwrap();
    assert_eq!(b2.pending_receipts(), 1, "the delivery only");
    b2.flush_receipts_now();
    wait_for(|| r.node.held_messages() == 1).await;

    let (a2, mut a2rx) = back_to(&dir, &r).await;
    next(&mut a2rx, |e| matches!(e, Event::Delivered { .. })).await;
    assert!(quiet_for(&mut a2rx, 3, |e| matches!(e, Event::Read { .. })).await);
    assert_eq!(entry(&a2, &bid, local_id).read_ms, 0);
}

#[tokio::test]
async fn turning_read_receipts_off_drops_queued_reads() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let aid = a.node.identity();
    let (b2, _b2rx, local_id) = sealed_while_away(&dir, &a, &b, &r, 60_000).await;
    // The delivery goes; then the read waits.
    b2.flush_receipts_now();
    wait_for(|| r.node.held_messages() == 1).await;
    b2.report_read(&aid, &[local_id]).unwrap();
    assert_eq!(b2.pending_receipts(), 1);
    b2.set_send_read_receipts(false);
    assert_eq!(b2.pending_receipts(), 0);

    // Still off after a restart, before any app says so.
    b2.shutdown();
    let (b3, _b3rx) = open(&dir, "b", false);
    assert!(!b3.send_read_receipts());
    b3.connect(&r.addr, None).await.unwrap();
    b3.flush_receipts_now();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(r.node.held_messages(), 1, "nothing new for A");
}

#[tokio::test]
async fn queued_reads_wait_for_a_mailbox_without_spending_prekeys() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (spawn(&dir, "a").await, spawn(&dir, "b").await);
    link(&mut a, &mut b).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| b.node.can_send_offline(&aid)).await;
    a.node.send_text(&bid, "read me later").unwrap();
    let Event::Message {
        msg: AppMessage::Text { id, .. },
        ..
    } = next(&mut b.rx, |e| {
        matches!(
            e,
            Event::Message {
                msg: AppMessage::Text { .. },
                ..
            }
        )
    })
    .await
    else {
        unreachable!()
    };
    a.node.shutdown();
    wait_for(|| gone(&b.node, &aid)).await;

    // No mailbox anywhere: the read waits, and no prekey is spent on it.
    let opks = b.node.opks_for(&aid);
    assert!(opks > 0);
    b.node.report_read(&aid, &[id]).unwrap();
    assert_eq!(b.node.pending_receipts(), 1);
    for _ in 0..3 {
        b.node.flush_receipts_now();
    }
    assert_eq!(b.node.pending_receipts(), 1);
    assert_eq!(b.node.opks_for(&aid), opks);
    assert!(b.node.can_send_offline(&aid));
}

#[tokio::test]
async fn upgrade_backlog_is_not_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    a.node.send_text(&bid, "from before").unwrap();
    let Event::Message {
        msg: AppMessage::Text { id, .. },
        ..
    } = next(&mut b.rx, |e| {
        matches!(
            e,
            Event::Message {
                msg: AppMessage::Text { .. },
                ..
            }
        )
    })
    .await
    else {
        unreachable!()
    };
    // As if this version first ran after the message came.
    b.node.set_receipts_since(threnody_core::now_ms() + 60_000);
    a.node.shutdown();
    wait_for(|| gone(&b.node, &aid)).await;
    assert!(
        b.node.report_read(&aid, &[id]).is_err(),
        "NoRoute, as before"
    );
    assert_eq!(b.node.pending_receipts(), 0);
    let _ = &mut r.rx;
}

#[tokio::test]
async fn blocking_after_queueing_drops_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let aid = a.node.identity();
    let (b2, _b2rx, _) = sealed_while_away(&dir, &a, &b, &r, 60_000).await;
    assert_eq!(b2.pending_receipts(), 1);
    b2.block_contact(&aid);
    b2.flush_receipts_now();
    assert_eq!(b2.pending_receipts(), 0);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(r.node.held_messages(), 0);
}

#[tokio::test]
async fn queued_receipts_go_live_when_the_sender_returns() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, r) = three(&dir).await;
    let bid = b.node.identity();
    let (b2, _b2rx, local_id) = sealed_while_away(&dir, &a, &b, &r, 60_000).await;
    assert_eq!(b2.pending_receipts(), 1);
    let b2addr = b2.listen("127.0.0.1:0").await.unwrap().to_string();

    // A comes back straight to B.
    let (a2, mut a2rx) = open(&dir, "a", false);
    a2.connect(&b2addr, None).await.unwrap();
    let Event::Delivered { local_id: got, .. } = timeout(
        Duration::from_secs(5),
        next(&mut a2rx, |e| matches!(e, Event::Delivered { .. })),
    )
    .await
    .unwrap() else {
        unreachable!()
    };
    assert_eq!(got, local_id);
    assert!(entry(&a2, &bid, local_id).delivered);
    assert_eq!(b2.pending_receipts(), 0);
    assert_eq!(r.node.held_messages(), 0);
}

#[tokio::test]
async fn duplicate_mailbox_copies_give_one_receipt() {
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
    let (aid, bid) = (a.node.identity(), b.node.identity());
    wait_for(|| a.node.can_send_offline(&bid) && b.node.can_send_offline(&aid)).await;
    b.node.shutdown();
    wait_for(|| gone(&a.node, &bid) && gone(&r1.node, &bid) && gone(&r2.node, &bid)).await;
    assert_eq!(a.node.send_text(&bid, "twice").unwrap().sealed, 1);
    wait_for(|| r1.node.held_messages() == 1 && r2.node.held_messages() == 1).await;
    let local_id = a
        .node
        .history(a.node.conversation_for(&bid))
        .unwrap()
        .entries()
        .last()
        .unwrap()
        .local_id;
    a.node.shutdown();
    wait_for(|| gone(&r1.node, &aid) && gone(&r2.node, &aid)).await;

    let (b2, mut b2rx) = open(&dir, "b", false);
    b2.set_receipt_delay(60_000, 60_000);
    b2.connect(&r1.addr, None).await.unwrap();
    b2.connect(&r2.addr, None).await.unwrap();
    next(&mut b2rx, |e| matches!(e, Event::OfflineMessage { .. })).await;
    wait_for(|| r1.node.held_messages() == 0 && r2.node.held_messages() == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b2.pending_receipts(), 1);
    b2.flush_receipts_now();
    wait_for(|| r1.node.held_messages() == 1 && r2.node.held_messages() == 1).await;

    let (a2, mut a2rx) = open(&dir, "a", false);
    a2.connect(&r1.addr, None).await.unwrap();
    a2.connect(&r2.addr, None).await.unwrap();
    let Event::Delivered { local_id: got, .. } =
        next(&mut a2rx, |e| matches!(e, Event::Delivered { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(got, local_id);
    assert!(quiet_for(&mut a2rx, 3, |e| matches!(e, Event::Delivered { .. })).await);
}

#[tokio::test]
async fn no_receipts_for_unaccepted_senders() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, b, r) = three(&dir).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    b.node.block_contact(&aid);
    wait_for(|| gone(&a.node, &bid)).await;
    let mut m = text("anyone there");
    if let AppMessage::Text { id, .. } = &mut m {
        *id = 77;
    }
    a.node.send_offline(&bid, &m).unwrap();
    next(&mut a.rx, |e| matches!(e, Event::DepositReceipt { .. })).await;
    wait_for(|| r.node.held_messages() == 0).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.node.pending_receipts(), 0);
    b.node.report_read(&aid, &[77]).unwrap();
    b.node.flush_receipts_now();
    assert_eq!(b.node.pending_receipts(), 0);
    assert_eq!(r.node.held_messages(), 0);
}

#[tokio::test]
async fn reported_reads_are_not_reported_twice() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (spawn(&dir, "a").await, spawn(&dir, "b").await);
    link(&mut a, &mut b).await;
    let (aid, bid) = (a.node.identity(), b.node.identity());
    a.node.send_text(&bid, "once").unwrap();
    let Event::Message {
        msg: AppMessage::Text { id, .. },
        ..
    } = next(&mut b.rx, |e| {
        matches!(
            e,
            Event::Message {
                msg: AppMessage::Text { .. },
                ..
            }
        )
    })
    .await
    else {
        unreachable!()
    };
    a.node.shutdown();
    wait_for(|| gone(&b.node, &aid)).await;
    b.node.set_receipt_delay(60_000, 60_000);
    b.node.report_read(&aid, &[id]).unwrap();
    assert_eq!(b.node.pending_receipts(), 1);
    b.node.shutdown();

    let (b2, _b2rx) = open(&dir, "b", false);
    assert_eq!(b2.pending_receipts(), 1, "kept across the restart");
    b2.report_read(&aid, &[id]).unwrap();
    assert_eq!(b2.pending_receipts(), 1, "not queued again");
    let reported = b2
        .history(b2.conversation_for(&aid))
        .unwrap()
        .entries()
        .iter()
        .any(|e| !e.outgoing && e.remote_id == id && e.read_ms != 0);
    assert!(reported);
}
