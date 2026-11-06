//! Groups across the devices of one account, on running nodes.

#![cfg(feature = "node")]

use std::time::Duration;

use threnody_core::history::ConversationId;
use threnody_core::store::Home;
use threnody_core::{AppMessage, PublicIdentity};
use threnody_groups::GroupWire;
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
    /// Every group message that arrived, as sent.
    wires: Vec<(PublicIdentity, GroupWire)>,
}

async fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    start(home, identity).await
}

/// Starts `name` again from what it saved.
async fn restart(dir: &tempfile::TempDir, name: &str) -> N {
    let home = Home::new(dir.path().join(name));
    let identity = home.load_identity(None).unwrap();
    start(home, identity).await
}

async fn start(home: Home, identity: threnody_core::Identity) -> N {
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
        wires: Vec::new(),
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
                    if let Ok(w) = GroupWire::decode(&b) {
                        self.wires.push((peer, w));
                    }
                    if let Ok(u) = self.groups.incoming(&self.node, peer, &b) {
                        self.updates.extend(u);
                    }
                }
                Event::Connected { peer, .. } => self.groups.connected(&self.node, &peer),
                Event::DeviceLinked { device } => self.groups.linked(&self.node, &device),
                Event::AccountChanged { removed, .. } => {
                    let u = self.groups.revoked(&self.node, &removed);
                    self.updates.extend(u);
                }
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

/// The laptop and phone of one account, both in Bob's group `g`.
async fn our_devices_in_a_group(dir: &tempfile::TempDir) -> (N, N, N, threnody_groups::GroupId) {
    let mut laptop = spawn(dir, "laptop").await;
    let mut phone = spawn(dir, "phone").await;
    let mut bob = spawn(dir, "bob").await;
    let lid = approve(&mut bob, &mut laptop).await;
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &lid).unwrap();
    until(&mut [&mut bob, &mut laptop], |ns| ns[1].in_group(&g)).await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    phone.node.connect(&bob.addr, None).await.unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[2].in_group(&g) && ns[0].groups.list()[0].3.len() == 3
    })
    .await;
    (laptop, phone, bob, g)
}

/// `n`'s entry for its group message `text`.
fn group_entry(
    n: &N,
    g: &threnody_groups::GroupId,
    text: &str,
) -> Option<threnody_core::history::Entry> {
    n.node
        .history(ConversationId::Group(*g))
        .ok()?
        .entries()
        .iter()
        .find(|e| e.text == text)
        .cloned()
}

#[tokio::test]
async fn group_receipts_reach_our_other_devices() {
    let dir = tempfile::tempdir().unwrap();
    let (mut laptop, mut phone, mut bob, g) = our_devices_in_a_group(&dir).await;
    laptop
        .groups
        .send_text(&laptop.node, &g, "to the team")
        .unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        group_entry(ns[1], &g, "to the team").is_some_and(|e| e.delivered)
            && group_entry(ns[2], &g, "to the team").is_some()
    })
    .await;
    let on_laptop = group_entry(&laptop, &g, "to the team").unwrap();
    assert_eq!(on_laptop.recipients, 2);
    // The phone received the message itself, as a member; who else has
    // it only the laptop hears, and passes on.
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        group_entry(ns[2], &g, "to the team").is_some_and(|e| {
            e.outgoing && e.delivered && e.delivered_to.len() == 2 && e.recipients == 2
        })
    })
    .await;
}

#[tokio::test]
async fn a_member_offline_during_an_add_still_gets_the_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut carol = spawn(&dir, "carol").await;
    let mut dave = spawn(&dir, "dave").await;
    let cid = approve(&mut alice, &mut carol).await;
    // Dave isn't Alice's contact: she has no mailbox route to him.
    let aid = dave.node.connect(&alice.addr, None).await.unwrap();
    let did = dave.node.identity();
    let g = alice.groups.create("team").unwrap();
    alice.groups.invite(&alice.node, &g, &did).unwrap();
    until(&mut [&mut alice, &mut dave], |ns| {
        !ns[1].groups.invites().is_empty()
    })
    .await;
    dave.groups.accept_from(&dave.node, &g, &aid).unwrap();
    until(&mut [&mut alice, &mut dave], |ns| ns[1].in_group(&g)).await;

    // Dave goes away; Alice adds Carol meanwhile. Carol, just invited,
    // doesn't know the group yet: she can't pass Dave the commit.
    dave.node.shutdown();
    drop(dave);
    alice.node.disconnect(&did);
    alice.groups.invite(&alice.node, &g, &cid).unwrap();
    until(&mut [&mut alice, &mut carol], |ns| ns[1].in_group(&g)).await;
    assert_eq!(alice.groups.held_for(&did), 1, "the commit waits for Dave");

    // Back, Dave catches up and reads what's said after.
    let mut dave = restart(&dir, "dave").await;
    dave.node.connect(&alice.addr, None).await.unwrap();
    until(&mut [&mut alice, &mut carol, &mut dave], |ns| {
        ns[2].groups.list()[0].3.len() == 3
    })
    .await;
    alice.groups.send_text(&alice.node, &g, "hi all").unwrap();
    until(&mut [&mut alice, &mut carol, &mut dave], |ns| {
        ns[2].texts(&g) == ["hi all"] && ns[1].texts(&g) == ["hi all"]
    })
    .await;
}

#[tokio::test]
async fn a_linked_device_takes_only_the_owners_invitation() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let mut mallory = spawn(&dir, "mallory").await;
    let lid = approve(&mut bob, &mut laptop).await;
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &lid).unwrap();
    until(&mut [&mut bob, &mut laptop], |ns| ns[1].in_group(&g)).await;

    // Mallory knows the group's id and invites the phone to it first.
    let pid = mallory.node.connect(&phone.addr, None).await.unwrap();
    let request = GroupWire::KeyPackageRequest {
        group: g,
        name: "team".into(),
    };
    mallory
        .node
        .send(&pid, AppMessage::Group(request.encode().unwrap()))
        .unwrap();
    let mid = mallory.node.identity();
    until(&mut [&mut phone, &mut mallory], |ns| {
        ns[0].groups.invites().iter().any(|i| i.from == mid)
    })
    .await;

    // The laptop says to join the group: the phone takes Bob's
    // invitation, not Mallory's.
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    phone.node.connect(&bob.addr, None).await.unwrap();
    until(
        &mut [&mut bob, &mut laptop, &mut phone, &mut mallory],
        |ns| ns[2].in_group(&g),
    )
    .await;
    for _ in 0..25 {
        for n in [&mut bob, &mut laptop, &mut phone, &mut mallory] {
            n.pump();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(phone.groups.list()[0].2, bob.node.identity());
    assert!(phone.groups.invites().iter().any(|i| i.from == mid));
    assert!(
        !mallory
            .wires
            .iter()
            .any(|(_, w)| matches!(w, GroupWire::KeyPackage { .. })),
        "Mallory got a key package"
    );
}

#[tokio::test]
async fn invitations_from_strangers_are_few_and_from_the_blocked_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut mallory = spawn(&dir, "mallory").await;
    let aid = mallory.node.connect(&alice.addr, None).await.unwrap();
    let mid = mallory.node.identity();
    let request = |i: u8| {
        let r = GroupWire::KeyPackageRequest {
            group: [i; 16],
            name: format!("Free money {i}"),
        };
        r.encode().unwrap()
    };
    for i in 0..10 {
        mallory
            .node
            .send(&aid, AppMessage::Group(request(i)))
            .unwrap();
    }
    // The newest few stay.
    until(&mut [&mut alice, &mut mallory], |ns| {
        ns[0].groups.invites().iter().any(|i| i.group == [9; 16])
    })
    .await;
    let groups: Vec<_> = alice.groups.invites().iter().map(|i| i.group[0]).collect();
    assert_eq!(groups, [7, 8, 9]);

    // Once blocked, whatever still reaches us (from a mailbox, say) asks
    // nothing.
    alice.node.block_contact(&mid);
    let updates = alice
        .groups
        .incoming(&alice.node, mid, &request(20))
        .unwrap();
    assert!(updates.is_empty());
    assert_eq!(alice.groups.invites().len(), 3);
}

#[tokio::test]
async fn a_crowd_of_strangers_neither_hides_invitations_nor_floods_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    // `from` invites Alice to group `i`: whether that is quiet.
    let invite = |alice: &mut N, from: PublicIdentity, i: u8| {
        let r = GroupWire::KeyPackageRequest {
            group: [i; 16],
            name: format!("Group {i}"),
        };
        let r = r.encode().unwrap();
        let mut updates = alice.groups.incoming(&alice.node, from, &r).unwrap();
        let Some(Update::Invited { invite, quiet }) = updates.pop() else {
            panic!("no invitation");
        };
        assert_eq!((invite.from, invite.group), (from, [i; 16]));
        quiet
    };
    // Carol, whom Alice accepted but who hasn't approved her back.
    let carol = threnody_core::Identity::generate().public();
    alice.node.set_approval(&carol, true).unwrap();
    assert!(!invite(&mut alice, carol, 1));
    // Then fresh identities, more than are kept: only the first notifies.
    let quiet: Vec<bool> = (0..40)
        .map(|i| {
            invite(
                &mut alice,
                threnody_core::Identity::generate().public(),
                100 + i,
            )
        })
        .collect();
    assert!(!quiet[0] && quiet[1..].iter().all(|q| *q));
    let invites = alice.groups.invites();
    assert_eq!(invites.len(), threnody_groups::node::MAX_INVITES);
    assert!(invites.iter().any(|i| i.from == carol), "Carol's stays");
    assert_eq!(invites.last().unwrap().group, [139; 16], "the newest stay");
    // Someone we know still notifies.
    assert!(!invite(&mut alice, carol, 2));
}

#[tokio::test]
async fn a_trusted_invitation_that_could_not_be_saved_waits_for_consent() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut alice, &mut bob).await;
    // Alice's group state can't be written, so her key package for Bob's
    // invitation can't be kept or sent: the invitation is kept instead.
    let blocker = dir.path().join("alice").join("groups.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let request = GroupWire::KeyPackageRequest {
        group: [5; 16],
        name: "team".into(),
    };
    let updates = alice
        .groups
        .incoming(&alice.node, bid, &request.encode().unwrap())
        .unwrap();
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, Update::Invited { invite, quiet: false } if invite.from == bid))
    );
    assert_eq!(alice.groups.invites().len(), 1);
}

#[tokio::test]
async fn declining_one_inviter_keeps_anothers_invitation_to_the_same_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let (bob, mallory) = (
        threnody_core::Identity::generate().public(),
        threnody_core::Identity::generate().public(),
    );
    let request = GroupWire::KeyPackageRequest {
        group: [3; 16],
        name: "team".into(),
    };
    for from in [mallory, bob] {
        alice
            .groups
            .incoming(&alice.node, from, &request.encode().unwrap())
            .unwrap();
    }
    alice.groups.decline_from(&[3; 16], &mallory);
    let invites = alice.groups.invites();
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0].from, bob);
}

#[tokio::test]
async fn nothing_leaves_that_could_not_be_saved() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut alice, &mut bob).await;
    let g = alice.groups.create("team").unwrap();
    alice.groups.invite(&alice.node, &g, &bid).unwrap();
    until(&mut [&mut alice, &mut bob], |ns| ns[1].in_group(&g)).await;

    // Alice's group state can't be written (here: its temporary file's
    // name is taken), so her removing Bob must not reach him.
    let blocker = dir.path().join("alice").join("groups.tmp");
    std::fs::create_dir(&blocker).unwrap();
    assert!(alice.groups.remove(&alice.node, &g, &bid).is_err());
    assert!(alice.groups.send_text(&alice.node, &g, "lost").is_err());
    for _ in 0..25 {
        for n in [&mut alice, &mut bob] {
            n.pump();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(bob.in_group(&g) && bob.texts(&g).is_empty());
    assert_eq!(alice.groups.list()[0].3.len(), 2, "back to what was saved");

    // Once it can be, the group carries on where both left it.
    std::fs::remove_dir(&blocker).unwrap();
    alice
        .groups
        .send_text(&alice.node, &g, "still here")
        .unwrap();
    until(&mut [&mut alice, &mut bob], |ns| {
        ns[1].texts(&g) == ["still here"]
    })
    .await;
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

#[tokio::test]
async fn folders_sync_and_archived_chats_come_back() {
    use threnody_core::folders::{Folder, Folders};
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let bid = approve(&mut laptop, &mut bob).await;
    let conv = laptop.node.conversation_for(&bid);

    // A folder and an archived chat, then a newly linked device gets them.
    laptop.node.set_folders(Folders {
        folders: vec![Folder {
            id: 1,
            name: "Family".into(),
            chats: vec![conv],
            ..Folder::default()
        }],
        archived: vec![conv],
        ..Folders::default()
    });
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    until(&mut [&mut laptop, &mut phone], |ns| {
        ns[1]
            .node
            .folders()
            .folders
            .first()
            .is_some_and(|f| f.name == "Family")
    })
    .await;
    assert_eq!(phone.node.folders().archived, [conv]);

    // A change on the phone reaches the laptop: newest wins.
    phone.node.update_folders(|f| {
        f.folders[0].name = "Fam".into();
        true
    });
    until(&mut [&mut laptop, &mut phone], |ns| {
        ns[0].node.folders().folders[0].name == "Fam"
    })
    .await;

    // Bob writes: the chat leaves the archive, and is unread until seen.
    bob.node
        .send_text(&laptop.node.identity(), "dinner?")
        .unwrap();
    until(&mut [&mut laptop, &mut bob], |ns| {
        ns[0].node.folders().archived.is_empty()
    })
    .await;
    let unread = |n: &N| {
        n.node
            .history(conv)
            .map(|h| h.unread().count())
            .unwrap_or(0)
    };
    assert_eq!(unread(&laptop), 1);
    laptop.node.mark_seen(conv);
    assert_eq!(unread(&laptop), 0);
}

#[tokio::test]
async fn lists_are_shared_and_edited_together() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let mut carol = spawn(&dir, "carol").await;
    let bid = approve(&mut alice, &mut bob).await;
    let cid = approve(&mut alice, &mut carol).await;
    let aid = alice.node.identity();

    // Alice's shopping list, shared with Bob.
    let id = alice.node.create_list("Groceries").unwrap();
    alice.node.edit_list(&id, |l, s| {
        let milk = l.add(None, "milk", false, s).unwrap();
        l.add(Some(milk), "eggs", false, s);
        true
    });
    alice
        .node
        .share_list(&id, alice.node.conversation_for(&bid));
    let texts = |n: &N| -> Vec<(String, bool)> {
        n.node
            .list(&id)
            .map(|l| {
                l.list
                    .items()
                    .iter()
                    .map(|i| (i.text.clone(), i.done))
                    .collect()
            })
            .unwrap_or_default()
    };
    until(&mut [&mut alice, &mut bob], |ns| texts(ns[1]).len() == 2).await;
    // Bob's chat with Alice says so, and opens the list.
    let bconv = bob.node.conversation_for(&aid);
    let h = bob.node.history(bconv).unwrap();
    assert_eq!(h.entries().last().unwrap().list, Some(id));

    // Both change it at once: Bob ticks milk, Alice adds bread.
    let milk = bob.node.list(&id).unwrap().list.items()[0].id;
    bob.node
        .edit_list(&id, |l, s| l.change(milk, s, |i| i.done = true));
    alice.node.edit_list(&id, |l, s| {
        let last = l.items().last().map(|i| i.id);
        l.add(last, "bread", false, s);
        true
    });
    let want = vec![
        ("milk".to_owned(), true),
        ("eggs".to_owned(), false),
        ("bread".to_owned(), false),
    ];
    until(&mut [&mut alice, &mut bob], |ns| {
        texts(ns[0]) == want && texts(ns[1]) == want
    })
    .await;

    // Shared with a group too: Carol gets it through MLS, and her change
    // reaches Alice.
    let g = alice.groups.create("family").unwrap();
    alice.groups.invite(&alice.node, &g, &cid).unwrap();
    until(&mut [&mut alice, &mut carol], |ns| ns[1].in_group(&g)).await;
    if let Some((bytes, groups)) = alice.node.share_list(&id, ConversationId::Group(g)) {
        for gg in groups {
            alice
                .groups
                .send_list(&alice.node, &gg, bytes.clone())
                .unwrap();
        }
    }
    until(&mut [&mut alice, &mut carol], |ns| texts(ns[1]).len() == 3).await;
    if let Some((bytes, groups)) = carol.node.edit_list(&id, |l, s| {
        let eggs = l.items()[1].id;
        l.change(eggs, s, |i| i.done = true)
    }) {
        for gg in groups {
            carol
                .groups
                .send_list(&carol.node, &gg, bytes.clone())
                .unwrap();
        }
    }
    until(&mut [&mut alice, &mut carol], |ns| {
        texts(ns[0]).get(1).is_some_and(|(_, done)| *done)
    })
    .await;
}

/// Pumps every node for `d`, letting traffic settle.
async fn settle(ns: &mut [&mut N], d: Duration) {
    let end = tokio::time::Instant::now() + d;
    while tokio::time::Instant::now() < end {
        for n in ns.iter_mut() {
            n.pump();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_device_that_leaves_a_group_stays_out() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let lid = approve(&mut bob, &mut laptop).await;
    let pid = phone.node.identity();
    phone.node.connect(&bob.addr, None).await.unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[0].node.account_of(&pid).is_some()
    })
    .await;

    // Bob invites the account: both devices join directly.
    let g = bob.groups.create("team").unwrap();
    bob.groups.invite(&bob.node, &g, &lid).unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[1].in_group(&g) && ns[2].in_group(&g)
    })
    .await;

    // The phone leaves; everyone sees it go.
    phone.groups.leave(&phone.node, &g).unwrap();
    let member = |n: &N| {
        n.groups
            .list()
            .iter()
            .any(|(x, .., m)| *x == g && m.contains(&pid))
    };
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        !ns[2].in_group(&g) && !member(ns[0]) && !member(ns[1])
    })
    .await;

    // The phone and the laptop meet again: the phone isn't put back.
    phone.node.disconnect(&laptop.node.identity());
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[1].node.sessions().iter().all(|s| s.peer != pid)
    })
    .await;
    phone.node.connect(&laptop.addr, None).await.unwrap();
    settle(
        &mut [&mut bob, &mut laptop, &mut phone],
        Duration::from_secs(2),
    )
    .await;
    assert!(!phone.in_group(&g), "the phone rejoined a group it left");
    assert!(!member(&bob) && !member(&laptop));
}

#[tokio::test]
async fn a_removed_device_is_dropped_from_our_groups() {
    let dir = tempfile::tempdir().unwrap();
    let mut laptop = spawn(&dir, "laptop").await;
    let mut phone = spawn(&dir, "phone").await;
    let mut bob = spawn(&dir, "bob").await;
    let code = laptop.node.create_link_code(laptop.addr.clone());
    phone.node.link_with(&code).await.unwrap();
    let lid = approve(&mut bob, &mut laptop).await;
    let pid = phone.node.identity();
    phone.node.connect(&bob.addr, None).await.unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[0].node.account_of(&pid).is_some()
    })
    .await;
    let g = bob.groups.create("club").unwrap();
    bob.groups.invite(&bob.node, &g, &lid).unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[1].in_group(&g) && ns[2].in_group(&g)
    })
    .await;

    // The phone is lost: the laptop removes it from the account. Bob, who
    // owns the group, removes it from the group as soon as he hears.
    laptop.node.remove_device(&pid).unwrap();
    let member = |n: &N| {
        n.groups
            .list()
            .iter()
            .any(|(x, .., m)| *x == g && m.contains(&pid))
    };
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        !member(ns[0]) && !member(ns[1])
    })
    .await;
    assert!(!bob.node.can_send_offline(&pid), "its prekeys are gone");

    // What Bob says next reaches the laptop and never the phone.
    bob.groups
        .send_text(&bob.node, &g, "after removal")
        .unwrap();
    until(&mut [&mut bob, &mut laptop, &mut phone], |ns| {
        ns[1].texts(&g).iter().any(|t| t == "after removal")
    })
    .await;
    let _ = phone.node.connect(&bob.addr, None).await;
    settle(
        &mut [&mut bob, &mut laptop, &mut phone],
        Duration::from_secs(1),
    )
    .await;
    assert!(phone.texts(&g).iter().all(|t| t != "after removal"));
    assert_eq!(bob.groups.held_for(&pid), 0, "nothing held for it");
}
