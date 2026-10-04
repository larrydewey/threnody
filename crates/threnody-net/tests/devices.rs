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

    // Lost phone: the laptop removes it.
    laptop.node.remove_device(&pid).unwrap();
    next(&mut phone.rx, |e| matches!(e, Event::ThisDeviceRemoved)).await;
    next(
        &mut bob.rx,
        |e| matches!(e, Event::AccountChanged { removed, .. } if removed.contains(&pid)),
    )
    .await;
    assert!(laptop.node.account().state().removed.contains(&pid));

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
    assert_eq!(laptop.node.delete_messages(&pid, &[id], true), 1);
    wait_for(|| on(&phone.node, &lid) == 0).await;
}
