use std::time::Duration;

use threnody_core::persona::{LinkProof, Personas};
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
    run(home, identity).await
}

async fn run(home: Home, identity: threnody_core::Identity) -> N {
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

fn kv(k: &str, v: &str) -> (String, String) {
    (k.into(), v.into())
}

#[tokio::test]
async fn contacts_see_only_what_is_shared_with_them() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (spawn(&dir, "a").await, spawn(&dir, "b").await);
    let bid = a.node.connect(&b.addr, None).await.unwrap();
    let aid = a.node.identity();
    next(&mut b.rx, |e| matches!(e, Event::Connected { .. })).await;
    b.node.accept_contact(&aid);

    a.node
        .set_profile(vec![kv("name", "Alice"), kv("email", "alice@example.org")])
        .unwrap();
    // Nothing is shared by default.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = |n: &Node| n.contacts().get(&aid).unwrap().profile.clone();
    assert!(seen(&b.node).is_empty());
    assert!(
        !b.node
            .contacts()
            .get(&aid)
            .unwrap()
            .label()
            .contains("Alice")
    );

    a.node.set_shared_with(&bid, &["name".into()]).unwrap();
    next(&mut b.rx, |e| matches!(e, Event::ProfileChanged { .. })).await;
    assert_eq!(seen(&b.node), vec![kv("name", "Alice")]);
    assert!(
        b.node
            .contacts()
            .get(&aid)
            .unwrap()
            .label()
            .starts_with("Alice (")
    );

    // A profile change reaches them, still filtered.
    a.node
        .set_profile(vec![kv("name", "Alice B."), kv("email", "new@example.org")])
        .unwrap();
    next(&mut b.rx, |e| matches!(e, Event::ProfileChanged { .. })).await;
    assert_eq!(seen(&b.node), vec![kv("name", "Alice B.")]);

    // Unsharing takes it back from their view.
    a.node.set_shared_with(&bid, &[]).unwrap();
    next(&mut b.rx, |e| matches!(e, Event::ProfileChanged { .. })).await;
    assert!(seen(&b.node).is_empty());

    // Shared again, it is sent at the start of the next session.
    a.node.set_shared_with(&bid, &["email".into()]).unwrap();
    next(&mut b.rx, |e| matches!(e, Event::ProfileChanged { .. })).await;
    a.node.disconnect(&bid);
    wait_for(|| b.node.sessions().is_empty()).await;
    b.node
        .update_contacts(|c| c.get_mut(&aid).unwrap().profile.clear());
    a.node.connect(&b.addr, None).await.unwrap();
    wait_for(|| seen(&b.node) == vec![kv("email", "new@example.org")]).await;
    let _ = &mut a.rx;
}

#[tokio::test]
async fn a_persona_is_unlinkable_until_it_reveals_its_identity() {
    let dir = tempfile::tempdir().unwrap();
    let main_home = Home::new(dir.path().join("me"));
    let me = main_home.create_identity(None).unwrap();
    let main_id = me.public();
    let personas = Personas::new(&main_home, &me);
    let (_, home, pid) = personas.create("market", None, None, 0).unwrap();
    let mut p = run(home, pid).await;
    let mut b = spawn(&dir, "b").await;
    assert!(p.node.is_persona());

    let bid = p.node.connect(&b.addr, None).await.unwrap();
    next(&mut b.rx, |e| matches!(e, Event::Connected { .. })).await;
    let pkey = p.node.identity();
    assert_ne!(pkey, main_id);
    // Its account names no machine, and can't be renamed or linked.
    let chain = p.node.account();
    assert_eq!(chain.state().name_of(&pkey), Some("device"));
    assert!(p.node.rename_device(&pkey, "Larry's phone").is_err());
    let code = b.node.create_link_code(b.addr.clone());
    assert!(p.node.link_with(&code).await.is_err());

    wait_for(|| {
        p.node
            .supports(&bid, threnody_core::message::FEATURE_IDENTITY)
    })
    .await;
    // A proof for some other persona, or by another identity, is refused.
    let other = threnody_core::Identity::generate();
    assert!(
        p.node
            .reveal(&bid, LinkProof::sign(&me, &other.public()), None)
            .is_err()
    );

    p.node
        .reveal(
            &bid,
            LinkProof::sign(&me, &pkey),
            Some("threnody://ME@10.0.0.1:7450".into()),
        )
        .unwrap();
    let Event::IdentityRevealed {
        peer,
        identity,
        invite,
    } = next(&mut b.rx, |e| matches!(e, Event::IdentityRevealed { .. })).await
    else {
        unreachable!()
    };
    assert_eq!((peer, identity), (pkey, main_id));
    assert_eq!(invite.as_deref(), Some("threnody://ME@10.0.0.1:7450"));
    // Kept with the contact, for when the app wasn't watching.
    let kept = b.node.contacts().get(&pkey).unwrap().revealed.clone();
    assert_eq!(kept, Some((main_id, invite)));
    // A persona manages no personas of its own.
    assert!(p.node.personas().is_err() && p.node.link_proof(&bid).is_err());
    let _ = &mut p.rx;
}

#[tokio::test]
async fn the_main_node_manages_its_personas() {
    let dir = tempfile::tempdir().unwrap();
    let main = spawn(&dir, "main").await;
    let (info, home, key) = main
        .node
        .create_persona("forum", Some(threnody_core::now_ms() - 1), Some(b"pw"))
        .unwrap();
    assert!(home.exists() && main.node.persona_home(&info.id).unwrap() == home);
    let opened = Home::new(&home).load_identity(Some(b"pw")).unwrap();
    assert_eq!(opened.public(), key);
    main.node.link_proof(&key).unwrap().verify(&key).unwrap();
    main.node.rename_persona(&info.id, "old forum").unwrap();
    assert_eq!(main.node.personas().unwrap()[0].label, "old forum");
    assert_eq!(main.node.burn_expired_personas().unwrap(), vec![info.id]);
    assert!(!home.exists() && main.node.personas().unwrap().is_empty());
}
