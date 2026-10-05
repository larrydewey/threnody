use std::time::Duration;

use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

async fn spawn(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>, String) {
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
    (node, rx, addr)
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

#[tokio::test]
async fn history_records_both_sides_and_disappearing_messages_vanish() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx, _) = spawn(&dir, "a").await;
    let (b, mut brx, baddr) = spawn(&dir, "b").await;
    let bid = a.connect(&baddr, None).await.unwrap();
    let aid = a.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    b.accept_contact(&aid); // else a's messages are requests

    a.send_text(&bid, "stays").unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;

    a.set_timer(&bid, Some(1)).unwrap();
    a.send_text(&bid, "vanishes").unwrap();
    let Event::TimerChanged { secs, .. } =
        next(&mut brx, |e| matches!(e, Event::TimerChanged { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(secs, Some(1), "receiver adopts the sender's timer");

    let texts = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .iter()
            .map(|e| (e.outgoing, e.text.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(&a, &bid),
        [(true, "stays".into()), (true, "vanishes".into())]
    );
    timeout(Duration::from_secs(5), async {
        while texts(&b, &aid).len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        texts(&b, &aid),
        [(false, "stays".into()), (false, "vanishes".into())]
    );

    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(texts(&a, &bid), [(true, "stays".into())]);
    assert_eq!(texts(&b, &aid), [(false, "stays".into())]);
    assert_eq!(
        b.history(b.conversation_for(&aid)).unwrap().timer_s,
        Some(1)
    );
}

#[tokio::test]
async fn messages_stay_by_default_and_disappear_when_chosen() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx, _) = spawn(&dir, "alice").await;
    let (bob, mut brx, addr) = spawn(&dir, "bob").await;
    let bob_id = alice.connect(&addr, None).await.unwrap();
    let a_id = alice.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    bob.accept_contact(&a_id); // else alice's messages are requests
    let week = threnody_net::history::WEEK_S;
    assert_eq!(alice.timer(&bob_id), None, "messages stay by default");
    // A default timer the user chooses applies to chats without their own.
    alice.set_default_timer(Some(week));
    assert_eq!(alice.timer(&bob_id), Some(week));

    alice.send_text(&bob_id, "goes in a week").unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;
    let last = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .last()
            .cloned()
            .unwrap()
    };
    let mine = last(&alice, &bob_id);
    let left = mine.expires_at_ms.unwrap() - mine.at_ms;
    assert_eq!(left, u64::from(week) * 1000);
    assert!(
        last(&bob, &a_id).expires_at_ms.is_some(),
        "the receiver applies it too"
    );

    // Turned off for this chat: the next message stays, and Bob follows.
    alice.set_timer(&bob_id, None).unwrap();
    assert_eq!(alice.timer(&bob_id), None);
    alice.send_text(&bob_id, "stays").unwrap();
    next(&mut brx, |e| {
        matches!(e, Event::TimerChanged { secs: None, .. })
    })
    .await;
    assert_eq!(bob.timer(&a_id), None);
    assert!(last(&bob, &a_id).expires_at_ms.is_none());

    // A custom timer, and a default turned off for chats without one.
    alice.set_timer(&bob_id, Some(30)).unwrap();
    assert_eq!(alice.timer(&bob_id), Some(30));
    bob.set_default_timer(None);
    let carol = threnody_core::Identity::generate().public();
    assert_eq!(bob.timer(&carol), None);
}

#[tokio::test]
async fn strangers_write_requests_until_accepted_and_blocked_ones_stay_out() {
    let dir = tempfile::tempdir().unwrap();
    let (stranger, _srx, _) = spawn(&dir, "stranger").await;
    let (me, mut rx, addr) = spawn(&dir, "me").await;
    let my_id = stranger.connect(&addr, None).await.unwrap();
    let sid = stranger.identity();

    // We never contacted them: a request, kept in history.
    stranger.send_text(&my_id, "hi, it's me").unwrap();
    let e = next(&mut rx, |e| {
        matches!(e, Event::MessageRequest { .. } | Event::Message { .. })
    })
    .await;
    assert!(matches!(e, Event::MessageRequest { peer, .. } if peer == sid));
    assert!(!me.is_accepted(&sid));
    assert_eq!(
        me.history(me.conversation_for(&sid))
            .unwrap()
            .entries()
            .len(),
        1
    );
    // They, having written to us, accepted us.
    assert!(stranger.is_accepted(&my_id));

    me.accept_contact(&sid);
    stranger.send_text(&my_id, "now a conversation").unwrap();
    let e = next(&mut rx, |e| {
        matches!(e, Event::MessageRequest { .. } | Event::Message { .. })
    })
    .await;
    assert!(matches!(e, Event::Message { .. }));

    // Blocked: the conversation goes and they can't come back.
    me.block_contact(&sid);
    assert!(
        me.history(me.conversation_for(&sid))
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(!me.is_accepted(&sid));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = stranger.connect(&addr, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(me.sessions().iter().all(|s| s.peer != sid), "refused");
    // Approving them again lifts the block.
    me.set_approval(&sid, true).unwrap();
    assert!(me.is_accepted(&sid));
    stranger.connect(&addr, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(me.sessions().iter().any(|s| s.peer == sid));
}

#[tokio::test]
async fn messages_can_be_deleted_for_me_or_for_everyone() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx, _) = spawn(&dir, "alice").await;
    let (bob, mut brx, addr) = spawn(&dir, "bob").await;
    let bob_id = alice.connect(&addr, None).await.unwrap();
    let a_id = alice.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    bob.accept_contact(&a_id);
    let texts = |n: &Node, p| -> Vec<String> {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.text.clone())
            .collect()
    };
    for t in ["keep", "oops", "also mine"] {
        alice.send_text(&bob_id, t).unwrap();
        next(&mut brx, |e| matches!(e, Event::Message { .. })).await;
    }
    let id_of = |n: &Node, p, t: &str| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .iter()
            .find(|e| e.text == t)
            .unwrap()
            .message_id()
    };
    // Both sides know the message by the same id.
    assert_eq!(id_of(&alice, &bob_id, "oops"), id_of(&bob, &a_id, "oops"));

    // For everyone: gone on both sides.
    let oops = id_of(&alice, &bob_id, "oops");
    assert_eq!(alice.delete_messages(&bob_id, &[oops], true), 1);
    next(&mut brx, |e| {
        matches!(e, Event::MessagesDeleted { count: 1, .. })
    })
    .await;
    assert_eq!(texts(&bob, &a_id), ["keep", "also mine"]);

    // For me: only here.
    let mine = id_of(&alice, &bob_id, "also mine");
    alice.delete_messages(&bob_id, &[mine], false);
    assert_eq!(texts(&alice, &bob_id), ["keep"]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(texts(&bob, &a_id), ["keep", "also mine"]);

    // Bob can't delete Alice's messages for her.
    let keep = id_of(&bob, &a_id, "keep");
    bob.delete_messages(&a_id, &[keep], true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(texts(&alice, &bob_id), ["keep"]);
    assert!(
        texts(&bob, &a_id).iter().all(|t| t != "keep"),
        "but gone for him"
    );
}

#[tokio::test]
async fn senders_can_edit_their_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx, _) = spawn(&dir, "alice").await;
    let (bob, mut brx, addr) = spawn(&dir, "bob").await;
    let bob_id = alice.connect(&addr, None).await.unwrap();
    let a_id = alice.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    bob.accept_contact(&a_id);
    alice.send_text(&bob_id, "see you at 7").unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;
    let last = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .last()
            .cloned()
            .unwrap()
    };
    let id = last(&alice, &bob_id).message_id();

    assert!(alice.edit_message(&bob_id, id, "see you at 8"));
    next(&mut brx, |e| matches!(e, Event::MessageEdited { .. })).await;
    let theirs = last(&bob, &a_id);
    assert_eq!(theirs.text, "see you at 8");
    assert!(theirs.edited_ms != 0 && last(&alice, &bob_id).edited_ms != 0);

    // Bob can't rewrite what Alice said, nor can anyone edit a message
    // that isn't theirs.
    assert!(!bob.edit_message(&a_id, id, "see you never"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(last(&alice, &bob_id).text, "see you at 8");
}

#[tokio::test]
async fn reactions_several_per_person_both_ways() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx, _) = spawn(&dir, "alice").await;
    let (bob, mut brx, addr) = spawn(&dir, "bob").await;
    let b = alice.connect(&addr, None).await.unwrap();
    let a = alice.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;
    bob.accept_contact(&a);
    alice.send_text(&b, "lunch?").unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;
    let last = |n: &Node, p: &threnody_core::PublicIdentity| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .last()
            .cloned()
            .unwrap()
    };
    let id = last(&bob, &a).message_id();
    assert_eq!(id, last(&alice, &b).message_id());

    // Bob reacts twice; Alice sees both, by Bob.
    assert!(bob.react(&a, id, "👍", true));
    assert!(bob.react(&a, id, "🍕", true));
    for _ in 0..2 {
        next(&mut arx, |e| matches!(e, Event::Reacted { .. })).await;
    }
    let bob_as = alice.reactor_of(&b);
    let mut seen = last(&alice, &b).reactions;
    seen.sort();
    let mut want = vec![(bob_as, "👍".to_owned()), (bob_as, "🍕".to_owned())];
    want.sort();
    assert_eq!(seen, want);

    // Alice reacts to her own message too; Bob takes one of his back.
    assert!(alice.react(&b, id, "👍", true));
    next(&mut brx, |e| matches!(e, Event::Reacted { .. })).await;
    assert_eq!(last(&bob, &a).reactions.len(), 3);
    assert!(bob.react(&a, id, "🍕", false));
    next(&mut arx, |e| matches!(e, Event::Reacted { .. })).await;
    let r = last(&alice, &b).reactions;
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|(_, e)| e == "👍"));
    // No such message, or not an emoji: nothing happens.
    assert!(!alice.react(&b, 12345, "👍", true));
    assert!(!alice.react(&b, id, "", true));
}
