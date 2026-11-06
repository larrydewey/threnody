//! Scheduled messages: sent by the node when due, once, to a live device
//! of the contact, and only after the app has applied its settings.

use std::time::Duration;

use threnody_core::history::Entry;
use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity, now_ms};
use threnody_net::schedule::{Failure, MAX_AHEAD_MS, MAX_SCHEDULED_TEXT, Status};
use threnody_net::{AcceptPolicy, Event, NetError, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
}

fn open(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = if home.has_identity() {
        home.load_identity(None).unwrap()
    } else {
        home.create_identity(None).unwrap()
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
    let (node, rx) = open(dir, name);
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

/// Whether no event `pred` takes arrives within `d`.
async fn none_within(
    rx: &mut UnboundedReceiver<Event>,
    d: Duration,
    pred: impl Fn(&Event) -> bool,
) -> bool {
    timeout(d, async {
        loop {
            match rx.recv().await {
                Some(e) if pred(&e) => return,
                Some(_) => {}
                None => std::future::pending::<()>().await,
            }
        }
    })
    .await
    .is_err()
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

fn is_text(e: &Event, text: &str) -> bool {
    matches!(e, Event::Message { msg: AppMessage::Text { body, .. }, .. } if body == text)
}

/// `a` and `b` connected, each accepting the other.
async fn pair(a: &mut N, b: &mut N) -> PublicIdentity {
    let bid = a.node.connect(&b.addr, None).await.unwrap();
    next(&mut b.rx, |e| matches!(e, Event::Connected { .. })).await;
    b.node.accept_contact(&a.node.identity());
    a.node.accept_contact(&bid);
    // Its chat is its account's from here on.
    wait_for(|| a.node.account_of(&bid).is_some()).await;
    bid
}

/// `x` and `y` connected and mutually approved.
async fn link(x: &mut N, y: &mut N) -> PublicIdentity {
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
    yid
}

/// Our entry with local id `id` in the chat with `peer`, once recorded
/// (the message can arrive before it is). The chat is the one `peer` is
/// in by then: its account may have become known since.
async fn sent_entry(n: &Node, peer: &PublicIdentity, id: u64) -> Entry {
    let find = || {
        n.history(n.conversation_for(peer))
            .unwrap()
            .entries()
            .iter()
            .find(|e| e.outgoing && e.local_id == id)
            .cloned()
    };
    wait_for(|| find().is_some()).await;
    find().unwrap()
}

fn in_ms(ms: u64) -> u64 {
    now_ms() + ms
}

#[tokio::test]
async fn nothing_is_sent_until_sending_is_started() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let conv = a.node.conversation_for(&bid);

    let s = a
        .node
        .schedule_text(&bid, "later", None, in_ms(300))
        .unwrap();
    assert!(
        none_within(&mut b.rx, Duration::from_millis(1500), |e| is_text(
            e, "later"
        ))
        .await
    );
    assert_eq!(a.node.scheduled_in(conv)[0].status, Status::Waiting);

    // The app applies its settings, then opens the gate.
    a.node.set_default_timer(Some(60));
    a.node.start_scheduled_sending();
    next(&mut b.rx, |e| is_text(e, "later")).await;
    let e = sent_entry(&a.node, &bid, s.id).await;
    assert_eq!(e.expires_at_ms.map(|x| x - e.at_ms), Some(60_000));
}

#[tokio::test]
async fn scheduled_text_waits_then_arrives_like_any_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    a.node.start_scheduled_sending();
    let conv = a.node.conversation_for(&bid);

    let s = a
        .node
        .schedule_text(&bid, "see you", None, in_ms(1500))
        .unwrap();
    next(
        &mut a.rx,
        |e| matches!(e, Event::ScheduledChanged { peer: Some(p), .. } if *p == bid),
    )
    .await;
    assert!(dir.path().join("a").join("scheduled.state").exists());
    assert!(
        none_within(&mut b.rx, Duration::from_millis(800), |e| is_text(
            e, "see you"
        ))
        .await
    );
    next(&mut b.rx, |e| is_text(e, "see you")).await;
    let e = sent_entry(&a.node, &bid, s.id).await;
    assert!(e.at_ms >= s.due_ms);
    wait_for(|| a.node.scheduled_in(conv).is_empty()).await;
    assert!(!dir.path().join("a").join("scheduled.state").exists());
}

#[tokio::test]
async fn edit_reschedule_and_cancel_before_sending() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    a.node.start_scheduled_sending();

    let edited = a
        .node
        .schedule_text(&bid, "draft", None, in_ms(1000))
        .unwrap();
    assert!(a.node.edit_scheduled(edited.id, "final").unwrap());
    next(&mut b.rx, |e| is_text(e, "final")).await;

    let moved = a
        .node
        .schedule_text(&bid, "moved", None, in_ms(1000))
        .unwrap();
    assert!(a.node.reschedule(moved.id, in_ms(3_600_000)).unwrap());
    assert!(
        none_within(&mut b.rx, Duration::from_millis(1500), |e| is_text(
            e, "moved"
        ))
        .await
    );
    assert!(a.node.send_scheduled_now(moved.id));
    timeout(
        Duration::from_secs(5),
        next(&mut b.rx, |e| is_text(e, "moved")),
    )
    .await
    .unwrap();

    let gone = a
        .node
        .schedule_text(&bid, "gone", None, in_ms(500))
        .unwrap();
    assert!(a.node.cancel_scheduled(gone.id));
    assert!(!a.node.cancel_scheduled(gone.id));
    assert!(
        none_within(&mut b.rx, Duration::from_millis(1500), |e| is_text(
            e, "gone"
        ))
        .await
    );
}

#[tokio::test]
async fn disappearing_timer_is_the_one_at_send_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    a.node.start_scheduled_sending();

    let s = a
        .node
        .schedule_text(&bid, "brief", None, in_ms(1000))
        .unwrap();
    a.node.set_timer(&bid, Some(60)).unwrap();
    next(&mut b.rx, |e| is_text(e, "brief")).await;
    let e = sent_entry(&a.node, &bid, s.id).await;
    assert_eq!(e.expires_at_ms.map(|x| x - e.at_ms), Some(60_000));
}

#[tokio::test]
async fn scheduled_messages_survive_a_restart_and_fire_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let s = a
        .node
        .schedule_text(&bid, "once", None, in_ms(2000))
        .unwrap();
    a.node.shutdown();
    drop(a);

    let (a2, _a2rx) = open(&dir, "a");
    let conv = a2.conversation_for(&bid);
    assert_eq!(a2.scheduled_in(conv).len(), 1);
    a2.listen("127.0.0.1:0").await.unwrap();
    a2.connect(&b.addr, None).await.unwrap();
    a2.start_scheduled_sending();
    let mut got = 0;
    let _ = timeout(Duration::from_millis(5000), async {
        loop {
            if let Some(e) = b.rx.recv().await
                && is_text(&e, "once")
            {
                got += 1;
            }
        }
    })
    .await;
    assert_eq!(got, 1);
    sent_entry(&a2, &bid, s.id).await;
    assert!(a2.scheduled_in(conv).is_empty());
}

#[tokio::test]
async fn already_sent_items_are_not_sent_twice() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let conv = a.node.conversation_for(&bid);
    let s = a
        .node
        .schedule_text(&bid, "twice?", None, in_ms(1000))
        .unwrap();
    // As if it went out just before a crash, and the removal wasn't saved.
    a.node.append(
        conv,
        Entry {
            at_ms: now_ms(),
            outgoing: true,
            device: *a.node.identity().as_bytes(),
            text: "twice?".into(),
            offline: false,
            expires_at_ms: None,
            file: None,
            local_id: s.id,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
            remote_id: 0,
            edited_ms: 0,
            read_ms: 0,
            reactions: Vec::new(),
            quote: None,
            mentions: Vec::new(),
            poll: None,
            forwarded: false,
            list: None,
        },
    );
    a.node.start_scheduled_sending();
    assert!(
        none_within(&mut b.rx, Duration::from_millis(2500), |e| is_text(
            e, "twice?"
        ))
        .await
    );
    assert!(a.node.scheduled_in(conv).is_empty());
}

#[tokio::test]
async fn blocking_or_deleting_a_contact_drops_its_scheduled_messages() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let mut c = spawn(&dir, "c").await;
    let bid = pair(&mut a, &mut b).await;
    let cid = pair(&mut a, &mut c).await;
    let (bconv, cconv) = (a.node.conversation_for(&bid), a.node.conversation_for(&cid));
    a.node
        .schedule_text(&bid, "to b", None, in_ms(60_000))
        .unwrap();
    a.node
        .schedule_text(&cid, "to c", None, in_ms(60_000))
        .unwrap();
    assert_eq!(a.node.all_scheduled().len(), 2);

    a.node.block_contact(&bid);
    assert!(a.node.scheduled_in(bconv).is_empty());
    assert_eq!(a.node.scheduled_in(cconv).len(), 1);
    a.node.delete_conversation(&cid);
    assert!(a.node.scheduled_in(cconv).is_empty());
    assert!(a.node.all_scheduled().is_empty());
    assert!(!dir.path().join("a").join("scheduled.state").exists());
}

#[tokio::test]
async fn blocked_on_another_device_fails_at_fire_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let conv = a.node.conversation_for(&bid);
    let s = a
        .node
        .schedule_text(&bid, "nope", None, in_ms(500))
        .unwrap();
    // As contact sync from another of our devices would.
    a.node
        .update_contacts(|c| c.get_mut(&bid).unwrap().blocked = true);
    a.node.start_scheduled_sending();
    let Event::ScheduledFailed { id, reason, .. } =
        next(&mut a.rx, |e| matches!(e, Event::ScheduledFailed { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((id, reason), (s.id, Failure::Contact));
    assert_eq!(
        a.node.scheduled_in(conv)[0].status,
        Status::Failed(Failure::Contact)
    );
    assert!(
        none_within(&mut b.rx, Duration::from_millis(1000), |e| is_text(
            e, "nope"
        ))
        .await
    );
}

#[tokio::test]
async fn contact_deleted_elsewhere_fails_and_is_not_re_added() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let s = a
        .node
        .schedule_text(&bid, "ghost", None, in_ms(500))
        .unwrap();
    // Gone with no record of when (as after the remembered window).
    a.node.update_contacts(|c| c.remove(&bid));
    a.node.start_scheduled_sending();
    let Event::ScheduledFailed { id, reason, .. } =
        next(&mut a.rx, |e| matches!(e, Event::ScheduledFailed { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((id, reason), (s.id, Failure::Contact));
    assert!(a.node.contacts().get(&bid).is_none());
    assert!(
        none_within(&mut b.rx, Duration::from_millis(1000), |e| is_text(
            e, "ghost"
        ))
        .await
    );
}

#[tokio::test]
async fn a_device_removed_from_its_account_is_routed_around() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b1 = spawn(&dir, "b1").await;
    let mut b2 = spawn(&dir, "b2").await;
    let b1id = link(&mut a, &mut b1).await;
    let code = b1.node.create_link_code(b1.addr.clone());
    b2.node.link_with(&code).await.unwrap();
    let b2id = b2.node.identity();
    wait_for(|| a.node.account_of(&b2id).is_some()).await;
    a.node.connect(&b2.addr, None).await.unwrap();
    b2.node.accept_contact(&a.node.identity());
    let account = a.node.conversation_for(&b1id);
    assert_eq!(account, a.node.conversation_for(&b2id));

    let s = a
        .node
        .schedule_text(&b1id, "to b", None, in_ms(1500))
        .unwrap();
    b2.node.remove_device(&b1id).unwrap();
    wait_for(|| a.node.is_revoked(&b1id)).await;
    assert_eq!(a.node.scheduled_in(account).len(), 1);
    a.node.start_scheduled_sending();
    next(&mut b2.rx, |e| is_text(e, "to b")).await;
    sent_entry(&a.node, &b1id, s.id).await;
    wait_for(|| a.node.scheduled_in(account).is_empty()).await;
}

#[tokio::test]
async fn this_device_removed_fails_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut a1 = spawn(&dir, "a1").await;
    let a2 = spawn(&dir, "a2").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a1, &mut b).await;
    let code = a1.node.create_link_code(a1.addr.clone());
    a2.node.link_with(&code).await.unwrap();
    let a1id = a1.node.identity();
    wait_for(|| a2.node.account().state().has(&a1id)).await;
    a1.node.start_scheduled_sending();
    let s = a1
        .node
        .schedule_text(&bid, "orphan", None, in_ms(1500))
        .unwrap();

    a2.node.remove_device(&a1id).unwrap();
    let Event::ScheduledFailed { id, reason, .. } =
        next(&mut a1.rx, |e| matches!(e, Event::ScheduledFailed { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((id, reason), (s.id, Failure::Removed));
    assert!(
        none_within(&mut b.rx, Duration::from_millis(2500), |e| is_text(
            e, "orphan"
        ))
        .await
    );
}

#[tokio::test]
async fn scheduled_state_is_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    a.node
        .schedule_text(&bid, "the secret plan 4711", None, in_ms(3_600_000))
        .unwrap();
    let raw = std::fs::read(dir.path().join("a").join("scheduled.state")).unwrap();
    assert!(!raw.windows(4).any(|w| w == b"4711"));
    assert!(!raw.windows(6).any(|w| w == b"secret"));
    // Another identity can't read it.
    let other = Home::new(dir.path().join("other"));
    let id = other.create_identity(None).unwrap();
    std::fs::write(dir.path().join("other").join("scheduled.state"), &raw).unwrap();
    assert!(other.load_state(&id, "scheduled").is_err());
}

#[tokio::test]
async fn schedule_rejects_past_far_future_overlong_and_unknown_contacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = spawn(&dir, "a").await;
    let mut b = spawn(&dir, "b").await;
    let bid = pair(&mut a, &mut b).await;
    let refused = |r: threnody_net::Result<_>| matches!(r, Err(NetError::NotAllowed(_)));
    assert!(refused(a.node.schedule_text(&bid, "x", None, now_ms() - 1)));
    assert!(refused(a.node.schedule_text(
        &bid,
        "x",
        None,
        now_ms() + MAX_AHEAD_MS + 60_000
    )));
    let long = "x".repeat(MAX_SCHEDULED_TEXT + 1);
    assert!(refused(a.node.schedule_text(
        &bid,
        &long,
        None,
        in_ms(60_000)
    )));
    assert!(refused(a.node.schedule_text(
        &bid,
        "  ",
        None,
        in_ms(60_000)
    )));
    let stranger = Home::new(dir.path().join("s"))
        .create_identity(None)
        .unwrap()
        .public();
    assert!(refused(a.node.schedule_text(
        &stranger,
        "x",
        None,
        in_ms(60_000)
    )));
    a.node.block_contact(&bid);
    assert!(refused(a.node.schedule_text(
        &bid,
        "x",
        None,
        in_ms(60_000)
    )));
    assert!(a.node.all_scheduled().is_empty());
}
