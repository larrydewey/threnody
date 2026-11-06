use std::time::Duration;

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

async fn wait_for(cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
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

#[tokio::test]
async fn link_a_device_sync_contacts_and_show_the_account_to_peers() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await; // laptop already knows and approves bob
    let account = laptop.node.account().id();

    let code = laptop.node.create_link_code(laptop.addr.clone());
    let joined = phone.node.link_with(&code).await.unwrap();
    assert_eq!(joined, account);
    next(&mut laptop.rx, |e| matches!(e, Event::DeviceLinked { .. })).await;
    assert_eq!(laptop.node.account().state().devices.len(), 2);
    assert_eq!(phone.node.account(), laptop.node.account());
    assert!(phone.node.is_own_device(&laptop.node.identity()));

    // The phone inherits the laptop's contacts and approvals.
    let bid = bob.node.identity();
    wait_for(|| {
        phone
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| c.local_approved)
    })
    .await;
    // Own devices end up mutually approved.
    let lid = laptop.node.identity();
    wait_for(|| {
        phone
            .node
            .contacts()
            .get(&lid)
            .is_some_and(|c| c.mutually_approved())
    })
    .await;

    // Bob learns the account (pushed by the laptop) and recognises the phone.
    let pid = phone.node.identity();
    wait_for(|| bob.node.account_of(&pid).is_some_and(|a| a.id() == account)).await;
    phone.node.connect(&bob.addr, None).await.unwrap();
    wait_for(|| {
        bob.node
            .contacts()
            .get(&pid)
            .is_some_and(|c| c.account == Some(account))
    })
    .await;
    let _ = &mut phone.rx;
}

#[tokio::test]
async fn our_other_devices_hear_of_a_newly_linked_one() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut tablet = spawn(&dir, "tablet").await;
    let mut phone = spawn(&dir, "phone").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    tablet.node.link_with(&code).await.unwrap();
    wait_for(|| tablet.node.is_own_device(&laptop.node.identity())).await;

    // The laptop links the phone; the tablet learns of it as a new device
    // of our own account, so it can tell the user too.
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let (pid, account) = (phone.node.identity(), laptop.node.account().id());
    next(&mut tablet.rx, |e| {
        matches!(e, Event::AccountChanged { account: a, added, .. }
            if *a == account && added.contains(&pid))
    })
    .await;
    let _ = (&mut laptop.rx, &mut phone.rx);
}

#[tokio::test]
async fn link_codes_are_single_use_and_must_match() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let tablet = spawn(&dir, "tablet").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());

    let mut forged = code.clone();
    forged.secret[0] ^= 1;
    assert!(
        tablet.node.link_with(&forged).await.is_err(),
        "wrong secret accepted"
    );
    next(&mut laptop.rx, |e| matches!(e, Event::LinkRejected { .. })).await;

    phone.node.link_with(&code).await.unwrap();
    assert!(tablet.node.link_with(&code).await.is_err(), "code reused");
    assert_eq!(laptop.node.account().state().devices.len(), 2);
}

#[tokio::test]
async fn removing_a_device_revokes_it_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();
    wait_for(|| bob.node.account_of(&pid).is_some()).await;
    wait_for(|| bob.node.can_send_offline(&pid)).await;

    // Not a device of ours: refused.
    assert!(laptop.node.remove_device(&bob.node.identity()).is_err());
    // Lost phone: the laptop removes it.
    laptop.node.remove_device(&pid).unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::ThisDeviceRemoved)).await;
    next(
        &mut bob.rx,
        |e| matches!(e, Event::AccountChanged { removed, .. } if removed.contains(&pid)),
    )
    .await;
    assert!(laptop.node.account().state().removed.contains(&pid));
    // The laptop's own groups hear of it as its siblings would.
    next(
        &mut laptop.rx,
        |e| matches!(e, Event::AccountChanged { removed, .. } if removed.contains(&pid)),
    )
    .await;
    // Nothing more is sealed to it: its prekeys are gone.
    assert!(!bob.node.can_send_offline(&pid));
    assert!(!laptop.node.can_send_offline(&pid));
    let note = threnody_core::AppMessage::Text {
        sent_ms: 0,
        body: "still there?".into(),
        expires_in_s: None,
        id: 0,
        quote: None,
        forwarded: false,
    };
    assert!(bob.node.send_offline(&pid, &note).is_err());
    // The laptop is now the account's only device: it can't remove itself.
    assert!(laptop.node.remove_device(&laptop.node.identity()).is_err());

    // Bob now refuses the removed phone.
    let _ = phone.node.connect(&bob.addr, None).await;
    next(
        &mut bob.rx,
        |e| matches!(e, Event::Rejected { reason, .. } if reason.contains("removed")),
    )
    .await;
    assert!(bob.node.sessions().iter().all(|s| s.peer != pid));
}

#[tokio::test]
async fn contacts_can_seal_to_a_sibling_they_never_met() {
    use threnody_core::AppMessage;
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();

    // Bob never met the phone, but gets its shared bundle via the laptop.
    wait_for(|| bob.node.can_send_offline(&pid)).await;

    // The phone goes away; Bob seals to it; the laptop holds it.
    phone.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != pid)).await;
    let text = AppMessage::Text {
        sent_ms: 0,
        body: "hi phone".into(),
        expires_in_s: None,
        id: 0,
        quote: None,
        forwarded: false,
    };
    bob.node.send_offline(&pid, &text).unwrap();
    wait_for(|| laptop.node.held_messages() == 1).await;

    // The phone comes back (same home), connects to the laptop, and reads it.
    let home = threnody_core::store::Home::new(dir.path().join("phone"));
    let identity = home.load_identity(None).unwrap();
    let (phone2, mut prx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    phone2.connect(&laptop.addr, None).await.unwrap();
    let Event::OfflineMessage { from, msg, .. } =
        next(&mut prx, |e| matches!(e, Event::OfflineMessage { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((from, msg), (bob.node.identity(), text));
    let _ = (&mut laptop.rx, &mut phone.rx);
}

fn texts(n: &Node, peer: &threnody_core::PublicIdentity) -> Vec<(bool, String)> {
    n.history(n.conversation_for(peer))
        .unwrap()
        .entries()
        .iter()
        .map(|e| (e.outgoing, e.text.clone()))
        .collect()
}

#[tokio::test]
async fn history_follows_us_across_our_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let bid = bob.node.identity();
    laptop
        .node
        .send_text(&bid, "before the phone existed")
        .unwrap();
    next(&mut bob.rx, |e| matches!(e, Event::Message { .. })).await;
    wait_for(|| laptop.node.unacked(&bid) == 0).await;

    // A newly linked device gets the history so far.
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    wait_for(|| texts(&phone.node, &bid) == [(true, "before the phone existed".into())]).await;

    // New messages from either device reach the other.
    laptop.node.send_text(&bid, "from the laptop").unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    // Bob may not have met the phone; it reaches him sealed or relayed.
    phone.node.connect(&bob.addr, None).await.unwrap();
    phone.node.send_text(&bid, "from the phone").unwrap();
    next(&mut laptop.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    let want = |extra: &[(bool, &str)]| {
        let mut v: Vec<(bool, String)> = vec![
            (true, "before the phone existed".into()),
            (true, "from the laptop".into()),
            (true, "from the phone".into()),
        ];
        v.extend(extra.iter().map(|(o, t)| (*o, (*t).to_owned())));
        v
    };
    wait_for(|| texts(&laptop.node, &bid) == want(&[])).await;
    wait_for(|| texts(&phone.node, &bid) == want(&[])).await;

    // Bob's reply reaches both devices directly; nothing is doubled.
    bob.node
        .send_text(&laptop.node.identity(), "hi both")
        .unwrap();
    wait_for(|| texts(&laptop.node, &bid) == want(&[(false, "hi both")])).await;
    wait_for(|| texts(&phone.node, &bid) == want(&[(false, "hi both")])).await;

    // Nothing arrives twice later on.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(texts(&phone.node, &bid).len(), 4);
    assert_eq!(texts(&laptop.node, &bid).len(), 4);

    // Deleting "for me" on one device deletes on the others too.
    let conv = laptop.node.conversation_for(&bid);
    let id = laptop
        .node
        .history(conv)
        .unwrap()
        .entries()
        .iter()
        .find(|e| e.text == "from the laptop")
        .unwrap()
        .message_id();
    laptop.node.delete_messages(&bid, &[id], false);
    wait_for(|| texts(&phone.node, &bid).len() == 3).await;
    assert!(
        texts(&phone.node, &bid)
            .iter()
            .all(|(_, t)| t != "from the laptop")
    );
    // Bob keeps it: that was only for us.
    assert_eq!(texts(&bob.node, &laptop.node.identity()).len(), 4);
}

#[tokio::test]
async fn renaming_a_device_reaches_siblings_and_contacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    next(&mut laptop.rx, |e| matches!(e, Event::DeviceLinked { .. })).await;
    let pid = phone.node.identity();

    // The laptop names the phone; the phone and Bob learn it.
    laptop.node.rename_device(&pid, "Pixel 8a").unwrap();
    let name = |n: &Node| n.account().state().name_of(&pid).map(str::to_owned);
    wait_for(|| name(&phone.node).as_deref() == Some("Pixel 8a")).await;
    let account = laptop.node.account().id();
    wait_for(|| {
        bob.node
            .known_accounts()
            .get(&account)
            .and_then(|c| c.state().name_of(&pid).map(str::to_owned))
            .as_deref()
            == Some("Pixel 8a")
    })
    .await;
    // Only devices of the account, and real names.
    assert!(
        laptop
            .node
            .rename_device(&bob.node.identity(), "x")
            .is_err()
    );
    assert!(laptop.node.rename_device(&pid, " \n ").is_err());
    let _ = (&mut bob.rx, &mut phone.rx);
}

#[tokio::test]
async fn a_verified_contacts_new_device_starts_unverified() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let lid = laptop.node.identity();
    bob.node
        .update_contacts(|c| c.get_mut(&lid).unwrap().verified = true);

    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();
    next(
        &mut bob.rx,
        |e| matches!(e, Event::AccountChanged { added, .. } if added.contains(&pid)),
    )
    .await;
    let contacts = bob.node.contacts();
    let new = contacts.get(&pid).expect("bob knows the new device");
    assert!(!new.verified, "a new device is never verified for us");
    assert!(
        new.local_approved,
        "approval carries over to the account's devices"
    );
    assert!(
        contacts.get(&lid).unwrap().verified,
        "the old device stays verified"
    );
}

#[tokio::test]
async fn deleting_in_a_chat_between_our_own_devices() {
    let dir = tempfile::tempdir().unwrap();
    let laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    // As on the user's devices: the phone's account, the laptop joins.
    let code = phone.node.create_link_code(phone.addr.clone());
    laptop.node.link_with(&code).await.unwrap();
    let (lid, pid) = (laptop.node.identity(), phone.node.identity());
    tokio::time::sleep(Duration::from_millis(300)).await;
    laptop.node.send_text(&pid, "self-destruct").unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::Message { .. })).await;
    let on = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .iter()
            .filter(|e| e.text == "self-destruct")
            .count()
    };
    assert_eq!(on(&phone.node, &lid), 1);
    let id = laptop
        .node
        .history(laptop.node.conversation_for(&pid))
        .unwrap()
        .entries()
        .iter()
        .find(|e| e.text == "self-destruct")
        .unwrap()
        .message_id();
    eprintln!(
        "supports: {}",
        laptop
            .node
            .supports(&pid, threnody_core::message::FEATURE_DELETE)
    );
    eprintln!(
        "phone entry ids: {:?}",
        phone
            .node
            .history(phone.node.conversation_for(&lid))
            .unwrap()
            .entries()
            .iter()
            .map(|e| (e.message_id(), e.remote_id, e.local_id))
            .collect::<Vec<_>>()
    );
    // Edits reach the sibling too.
    assert!(laptop.node.edit_message(&pid, id, "self-destruct, edited"));
    let text = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .last()
            .map(|e| e.text.clone())
    };
    wait_for(|| text(&phone.node, &lid).as_deref() == Some("self-destruct, edited")).await;
    assert_eq!(laptop.node.delete_messages(&pid, &[id], true), 1);
    wait_for(|| text(&phone.node, &lid).is_none()).await;
}

#[tokio::test]
async fn deleting_a_chat_deletes_it_on_our_other_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    wait_for(|| phone.node.contacts().get(&bid).is_some()).await;
    laptop.node.send_text(&bid, "before deleting").unwrap();
    let conv = laptop.node.conversation_for(&bid);
    wait_for(|| {
        phone
            .node
            .history(phone.node.conversation_for(&bid))
            .is_ok_and(|h| !h.entries().is_empty())
    })
    .await;

    phone.node.delete_conversation(&bid);
    assert!(phone.node.contacts().get(&bid).is_none());
    // The laptop follows: contact and history gone, not resurrected.
    wait_for(|| laptop.node.contacts().get(&bid).is_none()).await;
    wait_for(|| {
        laptop
            .node
            .history(conv)
            .is_ok_and(|h| h.entries().is_empty())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(phone.node.contacts().get(&bid).is_none());
    assert!(laptop.node.contacts().get(&bid).is_none());

    // Bob writes again: a new message request, on both devices.
    bob.node.connect(&phone.addr, None).await.unwrap();
    let pid = phone.node.identity();
    bob.node.send_text(&pid, "hello again").unwrap();
    wait_for(|| phone.node.contacts().get(&bid).is_some_and(|c| !c.accepted)).await;
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

/// Blocking on one device is a revocation on all of them: our other
/// devices stop approving the contact, drop its discovery keys and end
/// their sessions with it.
#[tokio::test]
async fn blocking_revokes_on_our_other_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    laptop.node.set_tunnel_port(Some(51821)).unwrap();
    bob.node.set_tunnel_port(Some(51822)).unwrap();
    link(&mut laptop, &mut bob).await;
    let bid = bob.node.identity();
    wait_for(|| laptop.node.tunnel_peers().contains(&bid)).await;
    wait_for(|| {
        laptop
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| c.discovery_key.is_some())
    })
    .await;
    wait_for(|| saved(&dir, "laptop").exists()).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    wait_for(|| {
        phone
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| c.local_approved)
    })
    .await;

    phone.node.block_contact(&bid);
    let c = phone.node.contacts().get(&bid).cloned().unwrap();
    assert!(c.blocked && !c.local_approved && c.discovery_key.is_none());
    wait_for(|| {
        laptop
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| c.blocked && !c.local_approved && c.discovery_key.is_none())
    })
    .await;
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != bid)).await;
    // The laptop's tunnel with Bob goes too, from memory and from disk.
    assert!(!laptop.node.tunnel_peers().contains(&bid));
    assert!(!saved(&dir, "laptop").exists());
    // Bob hears that the laptop no longer approves him.
    let lid = laptop.node.identity();
    wait_for(|| {
        bob.node
            .contacts()
            .get(&lid)
            .is_some_and(|c| !c.mutually_approved())
    })
    .await;
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

#[tokio::test]
async fn clearing_a_chat_keeps_the_contact_on_every_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    wait_for(|| phone.node.contacts().get(&bid).is_some()).await;
    laptop.node.send_text(&bid, "old news").unwrap();
    let entries = |n: &Node| {
        n.history(n.conversation_for(&bid))
            .map(|h| h.entries().len())
            .unwrap_or(0)
    };
    wait_for(|| entries(&phone.node) == 1).await;

    phone.node.clear_conversation(&bid);
    assert_eq!(entries(&phone.node), 0);
    wait_for(|| entries(&laptop.node) == 0).await;
    // Still a contact, still approved, still reachable.
    for n in [&phone.node, &laptop.node] {
        assert!(n.contacts().get(&bid).is_some_and(|c| c.local_approved));
    }
    laptop.node.send_text(&bid, "new news").unwrap();
    wait_for(|| entries(&phone.node) == 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let h = laptop
        .node
        .history(laptop.node.conversation_for(&bid))
        .unwrap();
    assert_eq!(h.entries()[0].text, "new news", "old messages stay cleared");
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

#[tokio::test]
async fn blocking_and_unblocking_follow_us_across_our_devices() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    let blocked = |n: &Node| n.contacts().get(&bid).is_some_and(|c| c.blocked);
    wait_for(|| phone.node.contacts().get(&bid).is_some()).await;
    bob.node
        .send_text(&laptop.node.identity(), "hello")
        .unwrap();
    wait_for(|| !texts(&laptop.node, &bid).is_empty()).await;

    // Blocking on the phone reaches the connected laptop at once, and the
    // laptop drops Bob's session and the conversation as the phone did.
    phone.node.block_contact(&bid);
    wait_for(|| blocked(&laptop.node)).await;
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != bid)).await;
    assert!(
        texts(&laptop.node, &bid).is_empty(),
        "the laptop kept the blocked conversation"
    );

    // Approving again on the laptop unblocks on the phone, and the phone's
    // older block doesn't come back when the two next meet.
    laptop.node.set_approval(&bid, true).unwrap();
    wait_for(|| !blocked(&phone.node)).await;
    let lid = laptop.node.identity();
    phone.node.disconnect(&lid);
    wait_for(|| {
        laptop
            .node
            .sessions()
            .iter()
            .all(|s| s.peer != phone.node.identity())
    })
    .await;
    phone.node.connect(&laptop.addr, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!blocked(&laptop.node), "unblocking stuck on the laptop");
    assert!(!blocked(&phone.node), "unblocking stuck on the phone");
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

#[tokio::test]
async fn deleting_and_editing_reach_a_sibling_that_was_away() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    for t in ["to delete", "to edit"] {
        laptop.node.send_text(&bid, t).unwrap();
    }
    let want = |v: &[&str]| {
        v.iter()
            .map(|t| (true, (*t).to_owned()))
            .collect::<Vec<_>>()
    };
    wait_for(|| texts(&phone.node, &bid) == want(&["to delete", "to edit"])).await;
    let id_of = |t: &str| {
        let conv = laptop.node.conversation_for(&bid);
        laptop
            .node
            .history(conv)
            .unwrap()
            .entries()
            .iter()
            .find(|e| e.text == t)
            .unwrap()
            .message_id()
    };

    // The phone goes away; the laptop deletes one message (only for us)
    // and edits the other.
    let pid = phone.node.identity();
    phone.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != pid)).await;
    assert_eq!(
        laptop
            .node
            .delete_messages(&bid, &[id_of("to delete")], false),
        1
    );
    assert!(laptop.node.edit_message(&bid, id_of("to edit"), "edited"));

    // The phone comes back and follows.
    let home = Home::new(dir.path().join("phone"));
    let identity = home.load_identity(None).unwrap();
    let (phone2, _prx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    phone2.connect(&laptop.addr, None).await.unwrap();
    wait_for(|| texts(&phone2, &bid) == want(&["edited"])).await;
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

#[tokio::test]
async fn strangers_learn_our_devices_only_once_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let mut stranger = spawn(&dir, "stranger").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let lid = laptop.node.identity();

    // A stranger dials us: our chain, with our devices' names, isn't sent.
    let sid = stranger.node.identity();
    stranger.node.connect(&laptop.addr, None).await.unwrap();
    next(&mut laptop.rx, |e| matches!(e, Event::Connected { .. })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        stranger.node.account_of(&lid).is_none(),
        "a stranger saw our account"
    );

    // Once we accept them, they learn it (on the session they have).
    laptop.node.accept_contact(&sid);
    wait_for(|| stranger.node.account_of(&lid).is_some()).await;
    let _ = &mut stranger.rx;
}

#[tokio::test]
async fn a_device_that_was_away_cant_undo_deleting_or_unsharing_a_list() {
    use threnody_core::history::ConversationId;
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    next(&mut laptop.rx, |e| matches!(e, Event::DeviceLinked { .. })).await;
    let pid = phone.node.identity();

    // Two lists, one shared with a group, on both devices.
    let group = ConversationId::Group([5; 16]);
    let gifts = laptop.node.create_list("Gifts").unwrap();
    let old = laptop.node.create_list("Old").unwrap();
    laptop.node.share_list(&gifts, group);
    wait_for(|| {
        phone.node.list(&old).is_some()
            && phone.node.list(&gifts).is_some_and(|l| l.shared == [group])
    })
    .await;

    // With the phone away, the laptop unshares one and deletes the other.
    phone.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != pid)).await;
    laptop.node.unshare_list(&gifts, group);
    laptop.node.delete_list(&old);

    // The phone comes back with its old copies: both changes stand, on
    // both devices.
    let home = threnody_core::store::Home::new(dir.path().join("phone"));
    let identity = home.load_identity(None).unwrap();
    let (phone2, _prx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    phone2.connect(&laptop.addr, None).await.unwrap();
    wait_for(|| {
        phone2.list(&old).is_none() && phone2.list(&gifts).is_some_and(|l| l.shared.is_empty())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        laptop.node.list(&old).is_none(),
        "the deleted list stays gone"
    );
    assert!(laptop.node.list(&gifts).unwrap().shared.is_empty());
    assert!(phone2.list(&old).is_none());
    assert!(phone2.list(&gifts).unwrap().shared.is_empty());
}

#[tokio::test]
async fn a_removed_device_cannot_write_through_a_mailbox() {
    use threnody_core::AppMessage;
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let mut m = spawn(&dir, "m").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let (pid, bid) = (phone.node.identity(), bob.node.identity());
    link(&mut phone, &mut bob).await;
    link(&mut phone, &mut m).await;
    link(&mut bob, &mut m).await;
    wait_for(|| phone.node.can_send_offline(&bid)).await;
    bob.node.accept_contact(&laptop.node.identity());

    // The phone is stolen and removed; it still holds Bob's prekeys and
    // seals to him through a mailbox that never heard of the removal.
    laptop.node.remove_device(&pid).unwrap();
    next(
        &mut bob.rx,
        |e| matches!(e, Event::AccountChanged { removed, .. } if removed.contains(&pid)),
    )
    .await;
    wait_for(|| phone.node.sessions().iter().all(|s| s.peer != bid)).await;
    let text = AppMessage::Text {
        sent_ms: 0,
        body: "pay 1 BTC to X".into(),
        expires_in_s: None,
        id: 0,
        quote: None,
        forwarded: false,
    };
    phone.node.send_offline(&bid, &text).unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::DepositReceipt { .. })).await;
    let got = timeout(Duration::from_millis(800), async {
        loop {
            if let Some(Event::OfflineMessage { .. } | Event::MessageRequest { .. }) =
                bob.rx.recv().await
            {
                return;
            }
        }
    })
    .await;
    assert!(
        got.is_err(),
        "a removed device's sealed message got through"
    );
    assert!(
        texts(&bob.node, &pid).is_empty(),
        "recorded in the account's chat"
    );
    let _ = (&mut laptop.rx, &mut m.rx);
}

/// A contact's device that leaves its account loses its tunnel with us at
/// once, though no `Approval` message says so.
#[tokio::test]
async fn a_device_leaving_its_account_loses_its_tunnel() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    phone.node.set_tunnel_port(Some(51821)).unwrap();
    bob.node.set_tunnel_port(Some(51822)).unwrap();
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();
    link(&mut phone, &mut bob).await;
    wait_for(|| bob.node.tunnel_peers().contains(&pid)).await;

    laptop.node.remove_device(&pid).unwrap();
    next(
        &mut bob.rx,
        |e| matches!(e, Event::TunnelDown { peer, wg_public: Some(_) } if *peer == pid),
    )
    .await;
    assert!(!bob.node.tunnel_peers().contains(&pid));
    assert!(!saved(&dir, "bob").exists());
    let _ = (&mut laptop.rx, &mut phone.rx);
}

/// Where a node keeps its tunnel peers (Appendix D, *Persistence*).
fn saved(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().join(name).join("tunnels.state")
}

/// Revoking a contact on one device reaches our others by contact sync
/// alone, and each takes its tunnel with that contact down, saved peers
/// included.
#[tokio::test]
async fn revoking_on_another_device_takes_our_tunnel_down() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    laptop.node.set_tunnel_port(Some(51821)).unwrap();
    bob.node.set_tunnel_port(Some(51822)).unwrap();
    link(&mut laptop, &mut bob).await;
    let bid = bob.node.identity();
    wait_for(|| laptop.node.tunnel_peers().contains(&bid)).await;
    wait_for(|| saved(&dir, "laptop").exists()).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    wait_for(|| {
        phone
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| c.local_approved)
    })
    .await;

    phone.node.set_approval(&bid, false).unwrap();
    wait_for(|| {
        laptop
            .node
            .contacts()
            .get(&bid)
            .is_some_and(|c| !c.local_approved && !c.blocked)
    })
    .await;
    wait_for(|| !laptop.node.tunnel_peers().contains(&bid)).await;
    assert!(!saved(&dir, "laptop").exists());
    let _ = (&mut laptop.rx, &mut phone.rx, &mut bob.rx);
}

/// A node again from the home `name` an earlier one used.
async fn restart(dir: &tempfile::TempDir, name: &str) -> N {
    let home = Home::new(dir.path().join(name));
    let identity = home.load_identity(None).unwrap();
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

/// The outgoing entry with text `t` in `n`'s chat with `peer`.
fn sent(
    n: &Node,
    peer: &threnody_core::PublicIdentity,
    t: &str,
) -> Option<threnody_core::history::Entry> {
    n.history(n.conversation_for(peer))
        .unwrap()
        .entries()
        .iter()
        .find(|e| e.outgoing && e.text == t)
        .cloned()
}

#[tokio::test]
async fn delivery_and_read_ticks_reach_a_connected_sibling() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    laptop.node.send_text(&bid, "tick me").unwrap();
    wait_for(|| sent(&phone.node, &bid, "tick me").is_some()).await;
    wait_for(|| sent(&laptop.node, &bid, "tick me").unwrap().delivered).await;
    // The phone hears of the delivery from the laptop, not from bob.
    next(&mut phone.rx, |e| matches!(e, Event::DeviceSync { .. })).await;
    wait_for(|| sent(&phone.node, &bid, "tick me").unwrap().delivered).await;
    let _ = (&mut laptop.rx, &mut bob.rx);
}

#[tokio::test]
async fn delivery_and_read_ticks_reach_a_sibling_that_was_away() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    // Bob is away while the laptop sends, so the phone has the entry
    // (pushed live) before there is anything to tick.
    bob.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != bid)).await;
    drop(bob);
    let _ = laptop.node.send_text(&bid, "while you were out");
    wait_for(|| sent(&phone.node, &bid, "while you were out").is_some()).await;
    let pid = phone.node.identity();
    phone.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != pid)).await;
    drop(phone);

    // Bob comes back, receives and reads it.
    let mut bob = restart(&dir, "bob").await;
    bob.node.connect(&laptop.addr, None).await.unwrap();
    next(&mut bob.rx, |e| matches!(e, Event::Message { .. })).await;
    let id = sent(&laptop.node, &bid, "while you were out")
        .unwrap()
        .message_id();
    wait_for(|| {
        sent(&laptop.node, &bid, "while you were out")
            .unwrap()
            .delivered
    })
    .await;
    bob.node
        .report_read(&laptop.node.identity(), &[id])
        .unwrap();
    wait_for(|| {
        sent(&laptop.node, &bid, "while you were out")
            .unwrap()
            .read_ms
            != 0
    })
    .await;

    // The phone comes back and catches up on both ticks.
    let mut phone = restart(&dir, "phone").await;
    let e = sent(&phone.node, &bid, "while you were out").unwrap();
    assert!(!e.delivered && e.read_ms == 0, "not yet");
    phone.node.connect(&laptop.addr, None).await.unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::DeviceSync { .. })).await;
    wait_for(|| {
        let e = sent(&phone.node, &bid, "while you were out").unwrap();
        e.delivered && e.read_ms != 0
    })
    .await;
    let read_ms = sent(&phone.node, &bid, "while you were out")
        .unwrap()
        .read_ms;

    // Nothing goes back on a further reconnect.
    phone.node.disconnect(&laptop.node.identity());
    wait_for(|| phone.node.sessions().is_empty()).await;
    phone.node.connect(&laptop.addr, None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let e = sent(&phone.node, &bid, "while you were out").unwrap();
    assert!(e.delivered);
    assert_eq!(e.read_ms, read_ms);
    let _ = (&mut laptop.rx, &mut bob.rx);
}

#[tokio::test]
async fn ticks_from_before_the_upgrade_are_backfilled() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let bid = bob.node.identity();
    let pid = phone.node.identity();
    bob.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != bid)).await;
    drop(bob);
    let _ = laptop.node.send_text(&bid, "old news");
    wait_for(|| sent(&phone.node, &bid, "old news").is_some()).await;
    phone.node.shutdown();
    wait_for(|| laptop.node.sessions().iter().all(|s| s.peer != pid)).await;
    drop(phone);
    let mut bob = restart(&dir, "bob").await;
    bob.node.connect(&laptop.addr, None).await.unwrap();
    wait_for(|| sent(&laptop.node, &bid, "old news").unwrap().delivered).await;

    // Both stop; the laptop forgets how far the phone had its ticks, as
    // a version from before ticks were synced never knew.
    laptop.node.shutdown();
    drop(laptop);
    let state = dir.path().join("laptop").join("history-sync-ticks.state");
    assert!(state.exists());
    std::fs::remove_file(&state).unwrap();

    let laptop = restart(&dir, "laptop").await;
    let mut phone = restart(&dir, "phone").await;
    assert!(!sent(&phone.node, &bid, "old news").unwrap().delivered);
    phone.node.connect(&laptop.addr, None).await.unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::DeviceSync { .. })).await;
    assert!(sent(&phone.node, &bid, "old news").unwrap().delivered);
    let _ = &mut bob.rx;
}

/// An outgoing file entry from `device`, as history keeps it.
fn file_entry(
    device: &threnody_core::PublicIdentity,
    name: &str,
    size: u64,
    location: Option<String>,
) -> threnody_core::history::Entry {
    threnody_core::history::Entry {
        at_ms: threnody_core::now_ms(),
        outgoing: true,
        device: *device.as_bytes(),
        text: String::new(),
        offline: false,
        expires_at_ms: None,
        file: Some(threnody_core::history::FileNote {
            name: name.into(),
            size,
            location,
            sensitive: false,
            album: 0,
            clip: None,
            forwarded: false,
        }),
        local_id: threnody_net::history::local_id(),
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
        ticked_ms: 0,
    }
}

/// An account message as our own device would send it.
fn account_msg(op: u8, one: Option<&[u8]>, two: &[u8]) -> threnody_core::AppMessage {
    let b = threnody_core::cbor::to_vec(two.len() + 128, |e| {
        e.map_len(2 + usize::from(one.is_some()))?.u8(0)?.u8(op)?;
        if let Some(o) = one {
            e.u8(1)?.bytes(o)?;
        }
        e.u8(2)?.bytes(two)?;
        Ok(())
    })
    .unwrap();
    threnody_core::AppMessage::Account(b)
}

#[tokio::test]
async fn transcripts_never_plant_a_location() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();
    wait_for(|| laptop.node.is_own_device(&pid)).await;
    let bid = bob.node.identity();
    let conv = laptop.node.conversation_for(&bid);
    let planted = file_entry(
        &laptop.node.identity(),
        "id_ed25519",
        400,
        Some("/etc/passwd".into()),
    );
    let h = threnody_core::history::History::transcript(vec![planted])
        .encode()
        .unwrap();
    laptop
        .node
        .send(
            &phone.node.identity(),
            account_msg(8, Some(&conv.to_bytes()), &h),
        )
        .unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    let on_phone = phone
        .node
        .history(phone.node.conversation_for(&bid))
        .unwrap()
        .entries()
        .iter()
        .find_map(|e| e.file.clone())
        .unwrap();
    assert_eq!(on_phone.name, "id_ed25519");
    assert_eq!(on_phone.location, None);
    let _ = (&mut laptop.rx, &mut bob.rx);
}

/// A file store in a directory, counting reads.
struct DirStore {
    dir: std::path::PathBuf,
    reads: std::sync::atomic::AtomicUsize,
}

impl DirStore {
    fn new(dir: &tempfile::TempDir, name: &str) -> std::sync::Arc<Self> {
        let dir = dir.path().join(format!("{name}-files"));
        std::fs::create_dir_all(&dir).unwrap();
        std::sync::Arc::new(Self {
            dir,
            reads: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Puts `data` here as `name`; its location.
    fn put(&self, name: &str, data: &[u8]) -> String {
        let p = self.dir.join(name);
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }
}

impl threnody_net::FileStore for DirStore {
    fn read(&self, location: &str, max: u64) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let m = std::fs::metadata(location).ok()?;
        (m.is_file() && m.len() <= max)
            .then(|| std::fs::read(location).ok())
            .flatten()
    }

    fn keep(&self, name: &str, data: &[u8], _clip: bool) -> Option<String> {
        let p = self
            .dir
            .join(format!("{:x}-{name}", threnody_net::history::local_id()));
        std::fs::write(&p, data).ok()?;
        Some(p.to_string_lossy().into_owned())
    }
}

fn random(n: usize) -> Vec<u8> {
    threnody_core::random_bytes_vec(n)
}

/// `n`'s newest entry for the file `name` in its chat with `peer`.
fn file_named(
    n: &Node,
    peer: &threnody_core::PublicIdentity,
    name: &str,
) -> Option<threnody_core::history::Entry> {
    n.history(n.conversation_for(peer))
        .unwrap()
        .entries()
        .iter()
        .rev()
        .find(|e| e.file.as_ref().is_some_and(|f| f.name == name))
        .cloned()
}

/// The contents `n` has of the file `name`, once it has them.
fn contents(n: &Node, peer: &threnody_core::PublicIdentity, name: &str) -> Option<Vec<u8>> {
    let e = file_named(n, peer, name)?;
    std::fs::read(e.file?.location?).ok()
}

/// Sends `data` as `name` from `n`'s store to `peer`.
fn send_photo(
    n: &Node,
    store: &DirStore,
    peer: &threnody_core::PublicIdentity,
    name: &str,
    data: &[u8],
    sensitive: bool,
) {
    let location = store.put(name, data);
    n.send_file(
        peer,
        threnody_net::history::OutgoingFile {
            name: name.into(),
            data: data.to_vec(),
            location: Some(location),
            sensitive,
            caption: String::new(),
            album: 0,
            clip: None,
            forwarded: false,
        },
    )
    .unwrap();
}

/// The laptop (with its store) linked to Bob, and the phone (with its
/// store) linked to the laptop's account.
async fn linked_with_stores(
    dir: &tempfile::TempDir,
) -> (N, N, N, std::sync::Arc<DirStore>, std::sync::Arc<DirStore>) {
    let mut laptop = spawn(dir, "laptop").await;
    let phone = spawn(dir, "phone").await;
    let mut bob = spawn(dir, "bob").await;
    let (ls, ps) = (DirStore::new(dir, "laptop"), DirStore::new(dir, "phone"));
    laptop.node.set_file_store(ls.clone());
    phone.node.set_file_store(ps.clone());
    link(&mut laptop, &mut bob).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let pid = phone.node.identity();
    wait_for(|| laptop.node.is_own_device(&pid)).await;
    (laptop, phone, bob, ls, ps)
}

async fn wait_long(cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(30), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tokio::test]
async fn a_photo_follows_us_to_a_newly_linked_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let (ls, ps) = (DirStore::new(&dir, "laptop"), DirStore::new(&dir, "phone"));
    laptop.node.set_file_store(ls.clone());
    phone.node.set_file_store(ps.clone());
    link(&mut laptop, &mut bob).await;
    let bid = bob.node.identity();
    let photo = random(5000);
    send_photo(&laptop.node, &ls, &bid, "IMG_1.png", &photo, false);
    next(&mut bob.rx, |e| matches!(e, Event::Message { .. })).await;
    let bob_before = bob
        .node
        .history(bob.node.conversation_for(&laptop.node.identity()))
        .unwrap()
        .entries()
        .to_vec();

    // Linked within a session that started before: the history and
    // then the photo come over.
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    wait_for(|| contents(&phone.node, &bid, "IMG_1.png").as_deref() == Some(&photo[..])).await;
    assert_eq!(ls.reads(), 1);
    assert_eq!(
        bob.node
            .history(bob.node.conversation_for(&laptop.node.identity()))
            .unwrap()
            .entries(),
        &bob_before[..],
        "nothing reaches the contact"
    );
    let _ = &mut laptop.rx;
}

#[tokio::test]
async fn an_outgoing_photo_reaches_a_connected_sibling_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    let photo = random(70_000);
    send_photo(&laptop.node, &ls, &bid, "now.jpg", &photo, false);
    wait_for(|| contents(&phone.node, &bid, "now.jpg").as_deref() == Some(&photo[..])).await;
    assert!(phone.node.file_fetches().is_empty());
}

#[tokio::test]
async fn a_first_sync_does_not_refetch_files_we_already_have() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    wait_for(|| phone.node.contacts().get(&bid).is_some()).await;
    // Bob's photo reached both devices, and each kept it.
    let photo = random(3000);
    let note = |location: String| threnody_core::history::FileNote {
        name: "from-bob.png".into(),
        size: photo.len() as u64,
        location: Some(location),
        sensitive: false,
        album: 0,
        clip: None,
        forwarded: false,
    };
    laptop
        .node
        .record_received_file(&bid, note(ls.put("from-bob.png", &photo)), "", 4242);
    phone
        .node
        .record_received_file(&bid, note(ps.put("from-bob.png", &photo)), "", 4242);
    let phone_copy = file_named(&phone.node, &bid, "from-bob.png").unwrap();

    // The laptop forgets it ever synced the phone: the next session sends
    // the phone everything again.
    laptop.node.shutdown();
    drop(laptop);
    std::fs::remove_file(dir.path().join("laptop").join("history-sync.state")).unwrap();
    let laptop = restart(&dir, "laptop").await;
    laptop.node.set_file_store(ls.clone());
    let _ = laptop.node.send_text(&bid, "something new");
    while phone.rx.try_recv().is_ok() {}
    phone.node.connect(&laptop.addr, None).await.unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    assert_eq!(ls.reads(), 0, "nothing was asked for");
    assert_eq!(
        file_named(&phone.node, &bid, "from-bob.png")
            .unwrap()
            .file
            .unwrap()
            .location,
        phone_copy.file.unwrap().location
    );
    assert!(phone.node.file_fetches().is_empty());
}

#[tokio::test]
async fn sensitive_disappearing_and_non_media_files_wait_for_a_tap() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    let (a, b, c) = (random(1000), random(2000), random(3000));
    send_photo(&laptop.node, &ls, &bid, "private.png", &a, true);
    send_photo(&laptop.node, &ls, &bid, "report.pdf", &c, false);
    laptop.node.set_timer(&bid, Some(3600)).unwrap();
    send_photo(&laptop.node, &ls, &bid, "fleeting.png", &b, false);
    for _ in 0..3 {
        next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    }
    wait_for(|| file_named(&phone.node, &bid, "fleeting.png").is_some()).await;
    let r = |name: &str| {
        threnody_core::history::file_ref(&file_named(&phone.node, &bid, name).unwrap()).unwrap()
    };
    for name in ["private.png", "report.pdf", "fleeting.png"] {
        assert_eq!(
            phone.node.fetch_state(&r(name)),
            threnody_net::FetchState::Idle,
            "{name}"
        );
    }
    assert_eq!(ls.reads(), 0);

    // A tap brings each.
    let conv = phone.node.conversation_for(&bid);
    for (name, data) in [
        ("private.png", &a),
        ("report.pdf", &c),
        ("fleeting.png", &b),
    ] {
        let s = phone
            .node
            .fetch_file(conv, |e| e.file.as_ref().is_some_and(|f| f.name == name))
            .unwrap();
        assert!(
            matches!(
                s,
                threnody_net::FetchState::Fetching { .. } | threnody_net::FetchState::Waiting
            ),
            "{s:?}"
        );
        wait_for(|| contents(&phone.node, &bid, name).as_deref() == Some(&data[..])).await;
    }
}

#[tokio::test]
async fn auto_fetch_waits_while_off_and_starts_when_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    phone.node.set_auto_fetch(false);
    let bid = bob.node.identity();
    let photo = random(4000);
    send_photo(&laptop.node, &ls, &bid, "later.webp", &photo, false);
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    let r = threnody_core::history::file_ref(&file_named(&phone.node, &bid, "later.webp").unwrap())
        .unwrap();
    assert_eq!(
        phone.node.fetch_state(&r),
        threnody_net::FetchState::Waiting
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(ls.reads(), 0);
    phone.node.set_auto_fetch(true);
    wait_for(|| contents(&phone.node, &bid, "later.webp").as_deref() == Some(&photo[..])).await;
}

#[tokio::test]
async fn a_fetch_waits_for_a_sibling_and_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    // 12 parts: three windows.
    let big = random(3 * 1024 * 1024);
    send_photo(&laptop.node, &ls, &bid, "big.bin", &big, false);
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    let lid = laptop.node.identity();
    laptop.node.shutdown();
    drop(laptop);
    wait_for(|| phone.node.sessions().iter().all(|s| s.peer != lid)).await;

    let conv = phone.node.conversation_for(&bid);
    let pick =
        |e: &threnody_core::history::Entry| e.file.as_ref().is_some_and(|f| f.name == "big.bin");
    assert_eq!(
        phone.node.fetch_file(conv, pick).unwrap(),
        threnody_net::FetchState::Waiting
    );

    let laptop = restart(&dir, "laptop").await;
    laptop.node.set_file_store(ls.clone());
    phone.node.connect(&laptop.addr, None).await.unwrap();
    wait_long(|| contents(&phone.node, &bid, "big.bin").as_deref() == Some(&big[..])).await;
    assert!(phone.node.file_fetches().is_empty());
}

#[tokio::test]
async fn a_restarted_requester_keeps_its_queue() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    let doc = random(10_000);
    send_photo(&laptop.node, &ls, &bid, "kept.pdf", &doc, false);
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    let lid = laptop.node.identity();
    laptop.node.shutdown();
    drop(laptop);
    wait_for(|| phone.node.sessions().iter().all(|s| s.peer != lid)).await;
    let conv = phone.node.conversation_for(&bid);
    phone
        .node
        .fetch_file(conv, |e| {
            e.file.as_ref().is_some_and(|f| f.name == "kept.pdf")
        })
        .unwrap();
    phone.node.shutdown();
    drop(phone);

    let phone = restart(&dir, "phone").await;
    phone.node.set_file_store(ps.clone());
    assert_eq!(phone.node.file_fetches().len(), 1, "the queue survived");
    let laptop = restart(&dir, "laptop").await;
    laptop.node.set_file_store(ls.clone());
    phone.node.connect(&laptop.addr, None).await.unwrap();
    wait_for(|| contents(&phone.node, &bid, "kept.pdf").as_deref() == Some(&doc[..])).await;
}

#[tokio::test]
async fn a_sibling_without_the_file_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, mut phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    send_photo(&laptop.node, &ls, &bid, "gone.pdf", &random(500), false);
    next(&mut phone.rx, |e| matches!(e, Event::HistorySynced { .. })).await;
    std::fs::remove_file(ls.dir.join("gone.pdf")).unwrap();
    let conv = phone.node.conversation_for(&bid);
    phone
        .node
        .fetch_file(conv, |e| {
            e.file.as_ref().is_some_and(|f| f.name == "gone.pdf")
        })
        .unwrap();
    let r = threnody_core::history::file_ref(&file_named(&phone.node, &bid, "gone.pdf").unwrap())
        .unwrap();
    wait_for(|| phone.node.fetch_state(&r) == threnody_net::FetchState::Unavailable).await;
    assert_eq!(ls.reads(), 1);
    let _ = laptop;
}

#[tokio::test]
async fn strangers_cannot_fetch_our_files() {
    let dir = tempfile::tempdir().unwrap();
    let (laptop, _phone, bob, ls, _ps) = linked_with_stores(&dir).await;
    let bid = bob.node.identity();
    let photo = random(800);
    // Not a photo, so our own phone doesn't fetch it.
    send_photo(&laptop.node, &ls, &bid, "mine.pdf", &photo, false);
    let e = file_named(&laptop.node, &bid, "mine.pdf").unwrap();
    let want = threnody_core::devsync::FileWant {
        request: 1,
        file: threnody_core::history::file_ref(&e).unwrap(),
        offset: 0,
        hash: None,
    }
    .encode()
    .unwrap();
    let conv = laptop.node.conversation_for(&bid);
    bob.node
        .send(
            &laptop.node.identity(),
            account_msg(11, Some(&conv.to_bytes()), &want),
        )
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(ls.reads(), 0);
}
