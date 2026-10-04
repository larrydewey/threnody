use std::collections::HashMap;

use threnody_core::{Identity, PublicIdentity};
use threnody_groups::{GroupError, GroupEvent, GroupWire, Groups, Output};

/// In-memory delivery: routes every outgoing message until quiescent,
/// collecting each node's events.
struct Net {
    nodes: HashMap<PublicIdentity, Groups>,
    events: HashMap<PublicIdentity, Vec<GroupEvent>>,
    /// Auto-accept invitations (simulates user consent).
    accept: bool,
}

impl Net {
    fn new(ids: &[&Identity]) -> Self {
        Self {
            nodes: ids.iter().map(|i| (i.public(), Groups::new(i))).collect(),
            events: HashMap::new(),
            accept: true,
        }
    }

    fn node(&mut self, id: &PublicIdentity) -> &mut Groups {
        self.nodes.get_mut(id).unwrap()
    }

    fn deliver(&mut self, from: PublicIdentity, out: Output) {
        self.events.entry(from).or_default().extend(out.events);
        for o in out.send {
            // Wire round trip, as over a real session.
            let wire = GroupWire::decode(&o.wire.encode().unwrap()).unwrap();
            let res = self.node(&o.to).handle(from, wire).unwrap();
            for e in &res.events {
                if let (true, GroupEvent::InviteRequested { group, peer, .. }) = (self.accept, e) {
                    let reply = self.node(&o.to).accept_invite(group, *peer).unwrap();
                    self.deliver(o.to, reply);
                }
            }
            self.deliver(o.to, res);
        }
    }

    fn take(&mut self, id: &PublicIdentity) -> Vec<GroupEvent> {
        self.events.remove(id).unwrap_or_default()
    }
}

#[test]
fn create_invite_chat_and_remove() {
    let (a, b, c) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let (pa, pb, pc) = (a.public(), b.public(), c.public());
    let mut net = Net::new(&[&a, &b, &c]);

    let g = net.node(&pa).create("team").unwrap();
    let out = net.node(&pa).invite(&g, pb).unwrap();
    net.deliver(pa, out);
    assert!(net.take(&pb).contains(&GroupEvent::Joined {
        group: g,
        name: "team".into(),
        owner: pa
    }));
    assert!(net.take(&pa).contains(&GroupEvent::MemberAdded {
        group: g,
        member: pb
    }));

    let out = net.node(&pa).invite(&g, pc).unwrap();
    net.deliver(pa, out);
    assert!(
        net.take(&pc)
            .iter()
            .any(|e| matches!(e, GroupEvent::Joined { .. }))
    );
    assert!(net.take(&pb).contains(&GroupEvent::MemberAdded {
        group: g,
        member: pc
    }));

    // Everyone agrees on the membership.
    for p in [pa, pb, pc] {
        let (_, _, owner, mut members) = net.node(&p).list().pop().unwrap();
        members.sort_by_key(|m| *m.as_bytes());
        let mut want = vec![pa, pb, pc];
        want.sort_by_key(|m| *m.as_bytes());
        assert_eq!((owner, members), (pa, want));
    }

    let out = net.node(&pb).send_text(&g, "hello group").unwrap();
    assert_eq!(out.send.len(), 2, "fan-out to the two other members");
    net.deliver(pb, out);
    for p in [pa, pc] {
        assert!(net.take(&p).contains(&GroupEvent::Text {
            group: g,
            from: pb,
            text: "hello group".into()
        }));
    }

    // Only the owner changes membership.
    assert!(matches!(
        net.node(&pb).remove(&g, &pc),
        Err(GroupError::NotOwner)
    ));
    assert!(matches!(
        net.node(&pb).invite(&g, pa),
        Err(GroupError::NotOwner)
    ));

    let out = net.node(&pa).remove(&g, &pc).unwrap();
    net.deliver(pa, out);
    assert!(net.take(&pc).contains(&GroupEvent::Left { group: g }));
    assert!(net.take(&pb).contains(&GroupEvent::MemberRemoved {
        group: g,
        member: pc
    }));
    assert!(net.node(&pc).list().is_empty());

    // Post-removal traffic reaches only remaining members, and carol cannot read it.
    let out = net.node(&pa).send_text(&g, "after").unwrap();
    assert_eq!(out.send.iter().map(|o| o.to).collect::<Vec<_>>(), vec![pb]);
    let msg = out.send[0].wire.clone();
    assert!(net.node(&pc).handle(pa, msg).is_err());
    net.deliver(pa, out);
    assert!(net.take(&pb).contains(&GroupEvent::Text {
        group: g,
        from: pa,
        text: "after".into()
    }));
}

#[test]
fn unsolicited_and_forged_inputs_are_rejected() {
    let (a, b, m) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let (pa, pb, pm) = (a.public(), b.public(), m.public());
    let mut net = Net::new(&[&a, &b, &m]);
    net.accept = false;
    let g = net.node(&pa).create("x").unwrap();

    // A key package alice never asked for is refused.
    let kp = net
        .node(&pm)
        .accept_invite(&g, pa)
        .unwrap()
        .send
        .remove(0)
        .wire;
    assert!(matches!(
        net.node(&pa).handle(pm, kp.clone()),
        Err(GroupError::Unexpected(_))
    ));

    // Invited bob, but mallory answers with her key package: refused.
    let _ = net.node(&pa).invite(&g, pb).unwrap();
    assert!(net.node(&pa).handle(pm, kp).is_err());
    // Mallory relays bob's key package as if it were hers: credential mismatch.
    let bob_kp = net
        .node(&pb)
        .accept_invite(&g, pa)
        .unwrap()
        .send
        .remove(0)
        .wire;
    let _ = net.node(&pa).invite(&g, pm).unwrap();
    assert!(matches!(
        net.node(&pa).handle(pm, bob_kp),
        Err(GroupError::BadCredential)
    ));

    // Messages for unknown groups and garbage are errors, not panics.
    assert!(
        net.node(&pb)
            .handle(
                pa,
                GroupWire::Message {
                    group: g,
                    message: vec![1, 2, 3]
                }
            )
            .is_err()
    );
    let mut ok = Groups::new(&b);
    let g2 = ok.create("y").unwrap();
    assert!(
        ok.handle(
            pa,
            GroupWire::Message {
                group: g2,
                message: vec![0; 64]
            }
        )
        .is_err()
    );
    assert!(
        ok.handle(
            pa,
            GroupWire::Welcome {
                group: g,
                name: String::new(),
                welcome: vec![0; 10]
            }
        )
        .is_err()
    );
}

#[test]
fn group_uses_the_xwing_ciphersuite() {
    assert_eq!(
        threnody_groups::CIPHERSUITE,
        openmls::prelude::Ciphersuite::MLS_128_MLKEM768X25519_AES128GCM_SHA256_Ed25519
    );
}

#[test]
fn groups_survive_export_and_restore() {
    let (a, b) = (Identity::generate(), Identity::generate());
    let (pa, pb) = (a.public(), b.public());
    let mut net = Net::new(&[&a, &b]);
    let g = net.node(&pa).create("persist").unwrap();
    let out = net.node(&pa).invite(&g, pb).unwrap();
    net.deliver(pa, out);
    net.take(&pa);
    net.take(&pb);

    // Both sides restart from their exported state.
    for (id, ident) in [(pa, &a), (pb, &b)] {
        let state = net.node(&id).export().unwrap();
        let restored = Groups::restore(ident, &state).unwrap();
        net.nodes.insert(id, restored);
    }
    let out = net.node(&pb).send_text(&g, "still here").unwrap();
    net.deliver(pb, out);
    assert!(net.take(&pa).contains(&GroupEvent::Text {
        group: g,
        from: pb,
        text: "still here".into()
    }));
    let (_, name, owner, members) = net.node(&pb).list().pop().unwrap();
    assert_eq!((name.as_str(), owner, members.len()), ("persist", pa, 2));

    assert!(Groups::restore(&a, b"garbage").is_err());
}

#[test]
fn members_forward_messages_for_each_other() {
    let (a, b, c, x) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let (pa, pb, pc, px) = (a.public(), b.public(), c.public(), x.public());
    let mut net = Net::new(&[&a, &b, &c, &x]);
    let g = net.node(&pa).create("team").unwrap();
    for p in [pb, pc] {
        let out = net.node(&pa).invite(&g, p).unwrap();
        net.deliver(pa, out);
    }
    net.take(&pa);
    net.take(&pb);

    // Carol can't reach Bob: she hands Alice his copy.
    let out = net.node(&pc).send_text(&g, "via alice").unwrap();
    let for_bob = out.send.into_iter().find(|o| o.to == pb).unwrap();
    let GroupWire::Message { message, .. } = for_bob.wire else {
        panic!("expected an MLS message")
    };
    let fwd = GroupWire::Forward {
        group: g,
        to: *pb.as_bytes(),
        message,
        reference: 1,
    };
    let wire = GroupWire::decode(&fwd.encode().unwrap()).unwrap();
    let relayed = net.node(&pa).handle(pc, wire.clone()).unwrap();
    assert!(relayed.events.is_empty(), "the forwarder sees nothing new");
    assert_eq!(relayed.send.len(), 1);
    assert_eq!(relayed.send[0].to, pb);
    assert!(matches!(relayed.send[0].wire, GroupWire::Message { .. }));
    net.deliver(pa, relayed);
    assert!(net.take(&pb).contains(&GroupEvent::Text {
        group: g,
        from: pc,
        text: "via alice".into()
    }));

    // Only members forward, only to members, and never to themselves.
    assert!(
        net.node(&pa).handle(px, wire.clone()).is_err(),
        "sender outside the group"
    );
    let to = |t: PublicIdentity| GroupWire::Forward {
        group: g,
        to: *t.as_bytes(),
        message: vec![1],
        reference: 0,
    };
    assert!(
        net.node(&pa).handle(pc, to(px)).is_err(),
        "target outside the group"
    );
    assert!(
        net.node(&pa).handle(pc, to(pa)).is_err(),
        "forward to the forwarder"
    );
    assert!(
        net.node(&pa).handle(pc, to(pc)).is_err(),
        "forward back to the sender"
    );
    // Unknown group.
    let mut bad = to(pb);
    if let GroupWire::Forward { group, .. } = &mut bad {
        *group = [0; 16];
    }
    assert!(matches!(
        net.node(&pa).handle(pc, bad),
        Err(GroupError::UnknownGroup)
    ));
}

#[test]
fn members_leave_and_owners_delete() {
    let (a, b, c) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let (pa, pb, pc) = (a.public(), b.public(), c.public());
    let mut net = Net::new(&[&a, &b, &c]);
    let g = net.node(&pa).create("team").unwrap();
    for p in [pb, pc] {
        let out = net.node(&pa).invite(&g, p).unwrap();
        net.deliver(pa, out);
    }
    net.take(&pa);
    net.take(&pb);

    // The owner can't just leave; a member can.
    assert!(net.node(&pa).leave(&g).is_err());
    let out = net.node(&pc).leave(&g).unwrap();
    assert!(out.events.contains(&GroupEvent::Left { group: g }));
    assert!(net.node(&pc).list().is_empty(), "forgotten at once");
    net.deliver(pc, out);
    assert!(net.take(&pb).contains(&GroupEvent::MemberRemoved {
        group: g,
        member: pc
    }));
    let members = |n: &mut Net, p: &PublicIdentity| n.node(p).list()[0].3.len();
    assert_eq!(members(&mut net, &pa), 2);
    assert_eq!(members(&mut net, &pb), 2);
    // A leave request only works on the owner, from a member.
    assert!(
        net.node(&pb)
            .handle(pa, GroupWire::Leave { group: g })
            .is_err()
    );
    assert!(
        net.node(&pa)
            .handle(pc, GroupWire::Leave { group: g })
            .is_err()
    );

    // Deleting removes everyone; nothing survives a restart.
    let out = net.node(&pa).disband(&g).unwrap();
    net.deliver(pa, out);
    assert!(net.take(&pb).contains(&GroupEvent::Left { group: g }));
    assert!(net.node(&pa).list().is_empty() && net.node(&pb).list().is_empty());
    let saved = net.node(&pa).export().unwrap();
    assert!(Groups::restore(&a, &saved).unwrap().list().is_empty());
}
