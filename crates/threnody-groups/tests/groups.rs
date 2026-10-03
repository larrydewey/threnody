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
