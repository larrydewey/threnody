//! Volunteer relays and directories (Appendix P), and credentials between
//! peers (Appendix O), end to end over loopback.

use std::time::Duration;

use threnody_core::AppMessage;
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
    timeout(Duration::from_secs(30), async {
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

fn text(s: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
        id: 0,
    }
}

/// A directory with two volunteer relays listed, and a client subscribed.
async fn network(dir: &tempfile::TempDir) -> (N, N, N, N) {
    let d = spawn(dir, "directory").await;
    let link = d.node.serve_directory(&d.addr, false).unwrap();
    let mut relays = Vec::new();
    for name in ["v1", "v2"] {
        let mut v = spawn(dir, name).await;
        v.node.subscribe_directory(&link).await.unwrap();
        v.node.set_volunteer(Some(vec![v.addr.clone()])).unwrap();
        next(&mut v.rx, |e| {
            matches!(e, Event::VolunteerNote { note } if note.contains("listed as a volunteer relay"))
        })
        .await;
        relays.push(v);
    }
    assert_eq!(
        d.node
            .directory_relays()
            .iter()
            .filter(|r| r.listed)
            .count(),
        2
    );
    let a = spawn(dir, "client").await;
    let info = a.node.subscribe_directory(&link).await.unwrap();
    assert_eq!(info.relays, 2);
    // Today's and tomorrow's tokens.
    assert_eq!(info.tokens, 2);
    assert_eq!(a.node.volunteer_relays().len(), 2);
    let v2 = relays.pop().unwrap();
    let v1 = relays.pop().unwrap();
    (d, v1, v2, a)
}

#[tokio::test]
async fn volunteer_circuit_carries_a_session_without_making_contacts() {
    let dir = tempfile::tempdir().unwrap();
    let (d, v1, v2, a) = network(&dir).await;
    let mut c = spawn(&dir, "dest").await;
    let cfp = c.node.identity().fingerprint();
    let peer = a.node.connect_volunteer(cfp, &c.addr).await.unwrap();
    assert_eq!(peer, c.node.identity());
    let ev = next(&mut c.rx, |e| matches!(e, Event::Connected { .. })).await;
    let Event::Connected { peer: from, .. } = ev else {
        unreachable!()
    };
    assert_eq!(from, a.node.identity());
    // The session runs over the circuit.
    a.node.send(&peer, text("through volunteers")).unwrap();
    let ev = next(&mut c.rx, |e| {
        matches!(e, Event::Message { .. } | Event::MessageRequest { .. })
    })
    .await;
    match ev {
        Event::Message { msg, .. } | Event::MessageRequest { msg, .. } => {
            assert!(matches!(msg, AppMessage::Text { body, .. } if body == "through volunteers"));
        }
        _ => unreachable!(),
    }
    // Nobody on the path became anyone's contact, and the relays never
    // learned the client's identity.
    for n in [&v1.node, &v2.node, &d.node] {
        assert!(
            n.contacts().iter().next().is_none(),
            "a relay or directory gained a contact"
        );
    }
    assert!(c.node.contacts().iter().all(|x| x.key == a.node.identity()));
    assert!(v1.node.onion_hops() + v2.node.onion_hops() >= 2);
}

#[tokio::test]
async fn relays_refuse_unpaid_circuits() {
    let dir = tempfile::tempdir().unwrap();
    let (_d, v1, v2, a) = network(&dir).await;
    let c = spawn(&dir, "dest").await;
    let cfp = c.node.identity().fingerprint();
    // Tokens that don't pay these relays: the entry relay carries nothing
    // further.
    a.node.set_test_pay(Some(threnody_net::TestPay::Unpaid));
    assert!(a.node.connect_volunteer(cfp, &c.addr).await.is_err());
    assert!(c.node.sessions().is_empty());
    // A slot used once pays once: replaying it is refused.
    a.node.set_test_pay(Some(threnody_net::TestPay::Slot(7)));
    a.node.connect_volunteer(cfp, &c.addr).await.unwrap();
    let c2 = spawn(&dir, "dest2").await;
    assert!(
        a.node
            .connect_volunteer(c2.node.identity().fingerprint(), &c2.addr)
            .await
            .is_err()
    );
    assert!(c2.node.sessions().is_empty());
    a.node.set_test_pay(None);
    let _ = (&v1, &v2);
    // Spend every slot of today's tokens for both relays.
    for r in a.node.volunteer_relays() {
        a.node.exhaust_tokens(&r.identity);
    }
    let r = a
        .node
        .connect_volunteer(c.node.identity().fingerprint(), &c.addr)
        .await;
    assert!(r.is_err());
    // A client with no subscription has no route at all.
    let lone = spawn(&dir, "lone").await;
    assert!(
        lone.node
            .connect_volunteer(c.node.identity().fingerprint(), &c.addr)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn directories_review_relays_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let d = spawn(&dir, "directory").await;
    let link = d.node.serve_directory(&d.addr, true).unwrap();
    let mut v = spawn(&dir, "v").await;
    v.node.subscribe_directory(&link).await.unwrap();
    v.node.set_volunteer(Some(vec![v.addr.clone()])).unwrap();
    next(
        &mut v.rx,
        |e| matches!(e, Event::VolunteerNote { note } if note.contains("review")),
    )
    .await;
    let listed = d.node.directory_relays();
    assert_eq!(listed.len(), 1);
    assert!(!listed[0].listed);
    // Unreviewed relays aren't in the document.
    let a = spawn(&dir, "client").await;
    assert_eq!(a.node.subscribe_directory(&link).await.unwrap().relays, 0);
    assert!(
        d.node
            .set_relay_listed(&v.node.identity().fingerprint(), true)
    );
    a.node.refresh_directories().await;
    assert_eq!(a.node.directories()[0].relays, 1);
}

#[tokio::test]
async fn a_link_pinning_another_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let d = spawn(&dir, "directory").await;
    let other = spawn(&dir, "other").await;
    let link = d.node.serve_directory(&d.addr, false).unwrap();
    other.node.serve_directory(&other.addr, false).unwrap();
    // The real directory's id, at the other one's address.
    let forged = link.replace(&d.addr, &other.addr);
    let a = spawn(&dir, "client").await;
    assert!(a.node.subscribe_directory(&forged).await.is_err());
    assert!(a.node.directories().is_empty());
}

#[tokio::test]
async fn credentials_are_issued_and_selectively_presented() {
    let dir = tempfile::tempdir().unwrap();
    let mut issuer = spawn(&dir, "issuer").await;
    let mut holder = spawn(&dir, "holder").await;
    let mut verifier = spawn(&dir, "verifier").await;
    let hid = issuer.node.connect(&holder.addr, None).await.unwrap();
    next(&mut holder.rx, |e| matches!(e, Event::Connected { .. })).await;
    // Features are exchanged in the first messages.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let attrs = vec![
        ("name".to_owned(), "Ada".to_owned()),
        ("over18".to_owned(), "yes".to_owned()),
    ];
    let today = threnody_core::credential::day(threnody_core::now_ms());
    issuer
        .node
        .offer_credential(&hid, "example/member/1", attrs.clone(), today + 30)
        .unwrap();
    let ev = next(&mut holder.rx, |e| {
        matches!(e, Event::CredentialOffered { .. })
    })
    .await;
    let Event::CredentialOffered { offer } = ev else {
        unreachable!()
    };
    assert_eq!(offer.attributes, attrs);
    assert_eq!(offer.issuer, issuer.node.identity().fingerprint());
    holder.node.accept_credential_offer(offer.id).unwrap();
    next(&mut holder.rx, |e| {
        matches!(e, Event::CredentialReceived { .. })
    })
    .await;
    let held = holder.node.credentials();
    assert_eq!(held.len(), 1);
    let _ = &mut issuer.rx;

    let vid = holder.node.connect(&verifier.addr, None).await.unwrap();
    let hid2 = holder.node.identity();
    next(&mut verifier.rx, |e| matches!(e, Event::Connected { .. })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ask = verifier
        .node
        .ask_credential(&hid2, "example/member/1", vec!["over18".into()])
        .unwrap();
    let ev = next(&mut holder.rx, |e| {
        matches!(e, Event::CredentialAsked { .. })
    })
    .await;
    let Event::CredentialAsked { ask: req } = ev else {
        unreachable!()
    };
    assert_eq!(req.peer, vid);
    // Offering more than was asked shows only what was asked.
    holder
        .node
        .present_credential(req.id, held[0].id, &["over18".into(), "name".into()])
        .unwrap();
    let ev = next(&mut verifier.rx, |e| {
        matches!(
            e,
            Event::CredentialPresented { .. } | Event::CredentialFailed { .. }
        )
    })
    .await;
    let Event::CredentialPresented { id, verified, .. } = ev else {
        panic!("presentation failed: {ev:?}");
    };
    assert_eq!(id, ask);
    assert_eq!(
        verified.attributes,
        vec![("over18".to_owned(), "yes".to_owned())]
    );
    assert_eq!(verified.issuer, issuer.node.identity());
    assert_eq!(verified.schema, "example/member/1");
}
