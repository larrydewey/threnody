//! Group calls (Appendix Q, *Group calls*): their signalling in group
//! messages, on running nodes.

#![cfg(feature = "node")]
#![allow(dead_code)]

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
    /// Leg signalling the node reported.
    legs: Vec<(PublicIdentity, Vec<u8>)>,
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
        legs: Vec::new(),
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
                Event::GroupLegSignal { from, data, .. } => self.legs.push((from, data)),
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
async fn group_calls_ring_gather_and_hand_over_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let mut alice = spawn(&dir, "alice").await;
    let mut bob = spawn(&dir, "bob").await;
    let mut carol = spawn(&dir, "carol").await;
    // Alice knows both; Bob and Carol never meet.
    let bid = approve(&mut alice, &mut bob).await;
    let cid = approve(&mut alice, &mut carol).await;
    let aid = alice.node.identity();
    let g = alice.groups.create("team").unwrap();
    alice.groups.invite(&alice.node, &g, &bid).unwrap();
    alice.groups.invite(&alice.node, &g, &cid).unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[1].in_group(&g) && ns[2].in_group(&g)
    })
    .await;
    // Let every member's view of the group settle.
    alice.groups.send_text(&alice.node, &g, "call?").unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[1].texts(&g).len() == 1 && ns[2].texts(&g).len() == 1
    })
    .await;

    // Alice starts: it rings for Bob and Carol, with Alice as the hub.
    let call = alice.groups.start_call(&alice.node, &g, false).unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[1].node.group_call().is_some() && ns[2].node.group_call().is_some()
    })
    .await;
    for n in [&bob, &carol] {
        let c = n.node.group_call().unwrap();
        assert!(c.call == call && c.group == g && c.hub == aid && !c.joined);
        assert_eq!(c.participants, vec![(aid, false)]);
    }

    // Both join; everyone ends up with the same roster.
    bob.groups.join_call(&bob.node, call, false).unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[0].node.group_call().unwrap().participants.len() == 1
            && ns[2].node.group_call().unwrap().participants.len() == 2
    })
    .await;
    carol.groups.join_call(&carol.node, call, true).unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns.iter().all(|n| {
            n.node
                .group_call()
                .is_some_and(|c| c.joined && c.participants.len() == 2)
        })
    })
    .await;
    assert!(
        alice
            .node
            .group_call()
            .unwrap()
            .participants
            .contains(&(cid, true))
    );

    // Leg signalling: over the session where there is one (Bob to Alice),
    // in a group message to that member only where there isn't (Bob to
    // Carol, carried by Alice).
    bob.groups
        .send_call_leg(&bob.node, call, &aid, b"o to alice".to_vec())
        .unwrap();
    bob.groups
        .send_call_leg(&bob.node, call, &cid, b"o to carol".to_vec())
        .unwrap();
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        ns[0].legs.contains(&(bid, b"o to alice".to_vec()))
            && ns[2].updates.contains(&Update::CallLeg {
                call,
                from: bid,
                data: b"o to carol".to_vec(),
            })
    })
    .await;
    assert!(
        !alice
            .updates
            .iter()
            .any(|u| matches!(u, Update::CallLeg { .. })),
        "a leg message is for its member only"
    );

    // Alice leaves: the one of Bob and Carol with the smaller key relays.
    alice.groups.leave_call(&alice.node, call);
    assert!(alice.node.group_call().is_none());
    let next = if bid.as_bytes() < cid.as_bytes() {
        bid
    } else {
        cid
    };
    until(&mut [&mut alice, &mut bob, &mut carol], |ns| {
        [&ns[1], &ns[2]].iter().all(|n| {
            n.node
                .group_call()
                .is_some_and(|c| c.hub == next && c.participants.len() == 1)
        })
    })
    .await;
}
