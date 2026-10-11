//! Groups across the devices of one account, on running nodes.

#![cfg(feature = "node")]

use std::time::Duration;

use threnody_core::history::ConversationId;
use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity};
use threnody_groups::node::{GroupNode, Update};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
    groups: GroupNode,
    /// What incoming group traffic produced, for the app to show.
    updates: Vec<Update>,
}

async fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    let groups = GroupNode::load(&home, &identity).unwrap();
    let (node, rx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    let addr = node.listen("127.0.0.1:0").await.unwrap().to_string();
    N {
        node,
        rx,
        addr,
        groups,
        updates: Vec::new(),
    }
}

impl N {
    /// Handles waiting events as an app would.
    fn pump(&mut self) {
        while let Ok(e) = self.rx.try_recv() {
            match e {
                Event::Message {
                    peer,
                    msg: AppMessage::Group(b),
                }
                | Event::OfflineMessage {
                    from: peer,
                    msg: AppMessage::Group(b),
                    ..
                } => {
                    if let Ok(u) = self.groups.incoming(&self.node, peer, &b) {
                        self.updates.extend(u);
                    }
                }
                Event::Connected { peer, .. } => self.groups.connected(&self.node, &peer),
                Event::DeviceLinked { device } => self.groups.linked(&self.node, &device),
                _ => {}
            }
        }
    }

    fn texts(&self, group: &threnody_groups::GroupId) -> Vec<String> {
        self.node
            .history(ConversationId::Group(*group))
            .map(|h| h.entries().iter().map(|e| e.text.clone()).collect())
            .unwrap_or_default()
    }

    fn in_group(&self, group: &threnody_groups::GroupId) -> bool {
        self.groups.list().iter().any(|(g, ..)| g == group)
    }
}

/// Pumps every node until `cond` holds.
async fn until(ns: &mut [&mut N], cond: impl Fn(&[&mut N]) -> bool) {
    timeout(Duration::from_secs(20), async {
        loop {
            for n in ns.iter_mut() {
                n.pump();
            }
            if cond(ns) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

async fn approve(x: &mut N, y: &mut N) -> PublicIdentity {
    let yid = x.node.connect(&y.addr, None).await.unwrap();
    x.node.set_approval(&yid, true).unwrap();
    y.node.set_approval(&x.node.identity(), true).unwrap();
    let (xid, yid2) = (x.node.identity(), yid);
    until(&mut [x, y], |ns| {
        ns[0]
            .node
            .contacts()
            .get(&yid2)
            .is_some_and(|c| c.mutually_approved())
            && ns[1]
                .node
                .contacts()
                .get(&xid)
                .is_some_and(|c| c.mutually_approved())
    })
    .await;
    yid
}

#[tokio::test]
async fn a_linked_device_joins_our_groups_with_their_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let lid = approve(&mut bob, &mut laptop).await;

    // Bob's group, with the laptop in it, and some talk.
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &lid).unwrap();
    until(&mut [&mut bob, &mut laptop], |ns| ns[1].in_group(&g)).await;
    bob.groups.send_text(&bob.node, &g, "hello team").unwrap();
    until(&mut [&mut bob, &mut laptop], |ns| {
        ns[1].texts(&g) == ["hello team"]
    })
    .await;
    laptop.groups.send_text(&laptop.node, &g, "hi bob").unwrap();
    until(&mut [&mut bob, &mut laptop], |ns| {
        ns[0].texts(&g) == ["hello team", "hi bob"]
    })
    .await;

    // The phone is linked: it gets the group's history from the laptop,
    // and Bob (the owner) adds it at the laptop's request.
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    phone.node.connect(&bob.addr, None).await.unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[2].in_group(&g) && ns[2].texts(&g) == ["hello team", "hi bob"]
    })
    .await;

    // From now on it reads the group itself.
    bob.groups
        .send_text(&bob.node, &g, "welcome, phone")
        .unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[2].texts(&g).last().map(String::as_str) == Some("welcome, phone")
            && ns[1].texts(&g).last().map(String::as_str) == Some("welcome, phone")
    })
    .await;
    assert_eq!(phone.texts(&g).len(), 3, "no duplicates");
}

#[tokio::test]
async fn an_owner_adds_its_own_linked_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut laptop, &mut bob).await;

    let g = laptop.groups.create("ours").unwrap();
    laptop.groups.invite(&laptop.node, &g, &bid).unwrap();
    until(&mut [&mut laptop, &mut bob], |ns| ns[1].in_group(&g)).await;
    bob.groups.send_text(&bob.node, &g, "from bob").unwrap();
    until(&mut [&mut laptop, &mut bob], |ns| {
        ns[0].texts(&g) == ["from bob"]
    })
    .await;

    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    until(&mut [&mut laptop, &mut bob, &mut phone], |ns| {
        ns[2].in_group(&g) && ns[2].texts(&g) == ["from bob"]
    })
    .await;
}

#[tokio::test]
async fn replies_quote_what_they_answer() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut alice, &mut bob).await;
    let aid = alice.node.identity();
    let quotes = |n: &N, conv: ConversationId| -> Vec<Option<(String, [u8; 32])>> {
        n.node
            .history(conv)
            .map(|h| {
                h.entries()
                    .iter()
                    .map(|e| e.quote.as_ref().map(|q| (q.text.clone(), q.author)))
                    .collect()
            })
            .unwrap_or_default()
    };

    // 1:1: Bob answers Alice's question, quoting it.
    alice.node.send_text(&bid, "lunch?").unwrap();
    let bconv = bob.node.conversation_for(&aid);
    until(&mut [&mut alice, &mut bob], |ns| {
        ns[1]
            .node
            .history(bconv)
            .is_ok_and(|h| h.entries().len() == 1)
    })
    .await;
    let asked = bob.node.history(bconv).unwrap().entries()[0].message_id();
    let q = bob.node.quote_of(bconv, |e| e.message_id() == asked);
    bob.node.send_reply(&aid, "yes!", q).unwrap();
    let aconv = alice.node.conversation_for(&bid);
    until(&mut [&mut alice, &mut bob], |ns| {
        quotes(ns[0], aconv).len() == 2
    })
    .await;
    let want = vec![None, Some(("lunch?".to_owned(), *aid.as_bytes()))];
    assert_eq!(quotes(&alice, aconv), want);
    assert_eq!(quotes(&bob, bconv), want);

    // A group: Alice quotes Bob's message.
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &aid).unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[0].in_group(&g)).await;
    bob.groups.send_text(&bob.node, &g, "ship it?").unwrap();
    until(&mut [&mut alice, &mut bob], |ns| {
        ns[0].texts(&g) == ["ship it?"]
    })
    .await;
    let gc = ConversationId::Group(g);
    let id = alice.node.history(gc).unwrap().entries()[0].message_id();
    let q = alice.node.quote_of(gc, |e| e.message_id() == id);
    alice
        .groups
        .send_reply(&alice.node, &g, "shipped", q)
        .unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[1].texts(&g).len() == 2).await;
    let want = vec![None, Some(("ship it?".to_owned(), *bid.as_bytes()))];
    assert_eq!(quotes(&bob, gc), want);
    assert_eq!(quotes(&alice, gc), want);
}

#[tokio::test]
async fn pins_reach_both_sides_and_only_admins_pin_in_groups() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut alice, &mut bob).await;
    let aid = alice.node.identity();
    let pins = |n: &N, conv: ConversationId| -> Vec<String> {
        n.node.pinned(conv).into_iter().map(|e| e.text).collect()
    };

    // 1:1: either side pins, and both see it.
    alice.node.send_text(&bid, "the address").unwrap();
    let (aconv, bconv) = (
        alice.node.conversation_for(&bid),
        bob.node.conversation_for(&aid),
    );
    until(&mut [&mut alice, &mut bob], |ns| {
        ns[1]
            .node
            .history(bconv)
            .is_ok_and(|h| h.entries().len() == 1)
    })
    .await;
    let id = bob.node.history(bconv).unwrap().entries()[0].message_id();
    assert!(bob.node.pin(&aid, id, true));
    until(&mut [&mut alice, &mut bob], |ns| {
        pins(ns[0], aconv) == ["the address"]
    })
    .await;
    assert_eq!(pins(&bob, bconv), ["the address"]);
    assert!(alice.node.pin(&bid, id, false));
    until(&mut [&mut alice, &mut bob], |ns| {
        pins(ns[1], bconv).is_empty()
    })
    .await;

    // A group: the owner pins for everyone; a member can't.
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &aid).unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[0].in_group(&g)).await;
    alice.groups.send_text(&alice.node, &g, "agenda").unwrap();
    until(&mut [&mut alice, &mut bob], |ns| {
        ns[1].texts(&g) == ["agenda"]
    })
    .await;
    let gc = ConversationId::Group(g);
    let id = bob.node.history(gc).unwrap().entries()[0].message_id();
    assert!(!alice.groups.may_pin(&g, &aid));
    assert!(alice.groups.pin(&alice.node, &g, id, true).is_err());
    assert!(bob.groups.pin(&bob.node, &g, id, true).unwrap());
    until(&mut [&mut alice, &mut bob], |ns| {
        pins(ns[0], gc) == ["agenda"]
    })
    .await;
    assert_eq!(pins(&bob, gc), ["agenda"]);
}

#[tokio::test]
async fn mentions_reach_who_they_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let mut carol = spawn(&dir, "carol").await;
    let bid = approve(&mut alice, &mut bob).await;
    let cid = approve(&mut alice, &mut carol).await;
    let g = alice.groups.create("team").unwrap();
    alice.groups.invite(&alice.node, &g, &bid).unwrap();
    alice.groups.invite(&alice.node, &g, &cid).unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[1].in_group(&g) && ns[2].in_group(&g)
    })
    .await;
    let mention = threnody_core::message::Mention {
        start: 0,
        len: 4,
        who: alice.node.reactor_of(&bid),
    };
    alice
        .groups
        .send_message(&alice.node, &g, "@Bob look", None, vec![mention])
        .unwrap();
    let mentioned = |n: &N| {
        n.updates.iter().find_map(|u| match u {
            Update::Text { mentioned, .. } => Some(*mentioned),
            _ => None,
        })
    };
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        mentioned(ns[1]).is_some() && mentioned(ns[2]).is_some()
    })
    .await;
    assert_eq!(mentioned(&bob), Some(true));
    assert_eq!(mentioned(&carol), Some(false));
    let stored = bob
        .node
        .history(ConversationId::Group(g))
        .unwrap()
        .entries()[0]
        .mentions
        .clone();
    assert_eq!(stored, [mention]);
}

#[tokio::test]
async fn polls_collect_votes_until_closed() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut alice, &mut bob).await;
    let g = alice.groups.create("team").unwrap();
    alice.groups.invite(&alice.node, &g, &bid).unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[1].in_group(&g)).await;
    alice
        .groups
        .send_poll(
            &alice.node,
            &g,
            "Lunch?",
            vec!["Pizza".into(), "Tacos".into()],
            false,
        )
        .unwrap();
    let gc = ConversationId::Group(g);
    let poll = |n: &N| {
        n.node
            .history(gc)
            .ok()
            .and_then(|h| h.entries().first().and_then(|e| e.poll.clone()))
    };
    until(&mut [&mut alice, &mut bob], |ns| poll(ns[1]).is_some()).await;
    let id = bob.node.history(gc).unwrap().entries()[0].message_id();
    assert!(bob.groups.vote(&bob.node, &g, id, &[1]).unwrap());
    until(&mut [&mut alice, &mut bob], |ns| {
        poll(ns[0]).is_some_and(|p| p.counts() == [0, 1])
    })
    .await;
    // Bob can't close Alice's poll; she can, and then it takes no votes.
    assert!(!bob.groups.close_poll(&bob.node, &g, id).unwrap());
    assert!(alice.groups.close_poll(&alice.node, &g, id).unwrap());
    until(&mut [&mut alice, &mut bob], |ns| {
        poll(ns[1]).is_some_and(|p| p.closed)
    })
    .await;
    assert!(!bob.groups.vote(&bob.node, &g, id, &[0]).unwrap());
}

#[tokio::test]
async fn forwards_drafts_and_notes_to_self() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut laptop, &mut bob).await;
    let lid = laptop.node.identity();

    // A forwarded message arrives marked.
    laptop.node.send_message(&bid, "fwd", None, true).unwrap();
    let bconv = bob.node.conversation_for(&lid);
    until(&mut [&mut laptop, &mut bob], |ns| {
        ns[1]
            .node
            .history(bconv)
            .is_ok_and(|h| h.entries().len() == 1)
    })
    .await;
    assert!(bob.node.history(bconv).unwrap().entries()[0].forwarded);

    // Drafts stay, encrypted, until cleared.
    let lconv = laptop.node.conversation_for(&bid);
    laptop.node.set_draft(lconv, "half a thought").unwrap();
    assert_eq!(laptop.node.history(lconv).unwrap().draft, "half a thought");
    laptop.node.set_draft(lconv, "").unwrap();
    assert_eq!(laptop.node.history(lconv).unwrap().draft, "");

    // Notes to self reach a device linked later, and new ones follow.
    laptop.node.write_note("buy milk", None, None);
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let notes = |n: &N| -> Vec<String> {
        n.node
            .history(n.node.notes())
            .map(|h| h.entries().iter().map(|e| e.text.clone()).collect())
            .unwrap_or_default()
    };
    until(&mut [&mut laptop, &mut phone], |ns| {
        notes(ns[1]) == ["buy milk"]
    })
    .await;
    assert_eq!(
        laptop.node.notes(),
        phone.node.notes(),
        "one account, one chat"
    );
    phone.node.write_note("and eggs", None, None);
    until(&mut [&mut laptop, &mut phone], |ns| {
        notes(ns[0]) == ["buy milk", "and eggs"]
    })
    .await;
}
