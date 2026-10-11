//! Groups across the devices of one account, on running nodes.

#![cfg(feature = "node")]

use std::time::Duration;

use threnody_core::history::ConversationId;
use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity};
use threnody_groups::node::GroupNode;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
    groups: GroupNode,
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
                    let _ = self.groups.incoming(&self.node, peer, &b);
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
    let q = bob.node.quote_of(bconv, asked);
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
    let q = alice.node.quote_of(gc, id);
    alice
        .groups
        .send_reply(&alice.node, &g, "shipped", q)
        .unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[1].texts(&g).len() == 2).await;
    let want = vec![None, Some(("ship it?".to_owned(), *bid.as_bytes()))];
    assert_eq!(quotes(&bob, gc), want);
    assert_eq!(quotes(&alice, gc), want);
}
