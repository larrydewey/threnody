use std::collections::HashMap;

use threnody_core::{Identity, PublicIdentity};
use threnody_groups::{
    GroupError, GroupEvent, GroupId, GroupWire, Groups, MemberRole, Outgoing, Output, RosterChange,
};

/// In-memory delivery: routes every outgoing message until quiescent,
/// collecting each node's events.
struct Net {
    nodes: HashMap<PublicIdentity, Groups>,
    events: HashMap<PublicIdentity, Vec<GroupEvent>>,
    /// Auto-accept invitations (simulates user consent).
    accept: bool,
    /// Unreachable for now: what's sent to them waits in `queue`.
    offline: Vec<PublicIdentity>,
    queue: Vec<(PublicIdentity, Outgoing)>,
    /// Errors are noted rather than fatal (for forks and forgeries).
    lenient: bool,
    errors: Vec<(PublicIdentity, String)>,
}

impl Net {
    fn new(ids: &[&Identity]) -> Self {
        Self {
            nodes: ids.iter().map(|i| (i.public(), Groups::new(i))).collect(),
            events: HashMap::new(),
            accept: true,
            offline: Vec::new(),
            queue: Vec::new(),
            lenient: false,
            errors: Vec::new(),
        }
    }

    fn node(&mut self, id: &PublicIdentity) -> &mut Groups {
        self.nodes.get_mut(id).unwrap()
    }

    fn deliver(&mut self, from: PublicIdentity, out: Output) {
        self.events.entry(from).or_default().extend(out.events);
        for o in out.send {
            self.send(from, o);
        }
    }

    /// Delivers only what `out` sends to `to`; the rest is lost.
    fn deliver_only(&mut self, from: PublicIdentity, out: Output, to: &[PublicIdentity]) {
        self.events.entry(from).or_default().extend(out.events);
        for o in out.send {
            if to.contains(&o.to) {
                self.send(from, o);
            }
        }
    }

    fn send(&mut self, from: PublicIdentity, o: Outgoing) {
        if self.offline.contains(&o.to) {
            self.queue.push((from, o));
            return;
        }
        if !self.nodes.contains_key(&o.to) {
            return;
        }
        // Wire round trip, as over a real session.
        let wire = GroupWire::decode(&o.wire.encode().unwrap()).unwrap();
        let res = match self.node(&o.to).handle(from, wire) {
            Ok(r) => r,
            Err(e) if self.lenient => {
                self.errors.push((o.to, e.to_string()));
                return;
            }
            Err(e) => panic!("{:?} handling from {from:?}: {e}", o.to),
        };
        for e in &res.events {
            if let (true, GroupEvent::InviteRequested { group, peer, .. }) = (self.accept, e) {
                let reply = self.node(&o.to).accept_invite(group, *peer).unwrap();
                self.deliver(o.to, reply);
            }
        }
        self.deliver(o.to, res);
    }

    /// Delivers what waited for members that are back online, in order.
    fn release(&mut self) {
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|(_, o)| !self.offline.contains(&o.to));
        self.queue = later;
        for (from, o) in now {
            self.send(from, o);
        }
    }

    fn take(&mut self, id: &PublicIdentity) -> Vec<GroupEvent> {
        self.events.remove(id).unwrap_or_default()
    }

    /// Restarts `id` from its exported state.
    fn restart(&mut self, ident: &Identity) {
        let id = ident.public();
        let state = self.node(&id).export().unwrap();
        self.nodes
            .insert(id, Groups::restore(ident, &state).unwrap());
    }
}

/// `n` identities, sorted by their bytes.
fn people(n: usize) -> (Vec<Identity>, Vec<PublicIdentity>) {
    let mut ids: Vec<Identity> = (0..n).map(|_| Identity::generate()).collect();
    ids.sort_by_key(|i| *i.public().as_bytes());
    let ps = ids.iter().map(Identity::public).collect();
    (ids, ps)
}

/// A group created by `p[0]`, which invites everyone else.
fn team(ids: &[Identity]) -> (Net, Vec<PublicIdentity>, GroupId) {
    let refs: Vec<&Identity> = ids.iter().collect();
    let mut net = Net::new(&refs);
    let p: Vec<PublicIdentity> = ids.iter().map(Identity::public).collect();
    let g = net.node(&p[0]).create("team").unwrap();
    for q in &p[1..] {
        let out = net.node(&p[0]).invite(&g, *q).unwrap();
        net.deliver(p[0], out);
    }
    for q in &p {
        net.take(q);
    }
    (net, p, g)
}

/// Every one of `who` on the same epoch with the same secrets.
fn in_step(net: &mut Net, g: &GroupId, who: &[PublicIdentity]) -> bool {
    let states: Vec<_> = who.iter().map(|p| net.node(p).epoch_state(g)).collect();
    states.iter().all(|s| s.is_some() && *s == states[0])
}

fn members(net: &mut Net, g: &GroupId, p: &PublicIdentity) -> Vec<PublicIdentity> {
    let mut m = net
        .node(p)
        .list()
        .into_iter()
        .find(|(id, ..)| id == g)
        .map(|(.., m)| m)
        .unwrap_or_default();
    m.sort_by_key(|x| *x.as_bytes());
    m
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
        member: pb,
        by: Some(pa)
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
        member: pc,
        by: Some(pa)
    }));

    // Everyone agrees on the membership.
    for p in [pa, pb, pc] {
        let (_, _, owner, mut members) = net.node(&p).list().pop().unwrap();
        members.sort_by_key(|m| *m.as_bytes());
        let mut want = vec![pa, pb, pc];
        want.sort_by_key(|m| *m.as_bytes());
        assert_eq!((owner, members), (pa, want));
    }

    let file = threnody_groups::Content::File {
        name: "map.png".into(),
        data: vec![0xFF; 5000],
        sensitive: true,
        caption: "the route".into(),
        album: 3,
        clip: None,
        id: 0,
        forwarded: false,
    };
    let out = net.node(&pc).send(&g, &file).unwrap();
    net.deliver(pc, out);
    for p in [pa, pb] {
        assert!(net.take(&p).contains(&GroupEvent::File {
            group: g,
            from: pc,
            name: "map.png".into(),
            data: vec![0xFF; 5000],
            sensitive: true,
            caption: "the route".into(),
            album: 3,
            clip: None,
            id: 0,
            forwarded: false,
        }));
    }

    let out = net.node(&pb).send_text(&g, "hello group").unwrap();
    assert_eq!(out.send.len(), 2, "fan-out to the two other members");
    net.deliver(pb, out);
    for p in [pa, pc] {
        assert!(net.take(&p).contains(&GroupEvent::Text {
            group: g,
            from: pb,
            text: "hello group".into(),
            id: 0,
            quote: None,
            mentions: Vec::new(),
            forwarded: false,
            preview: None,
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
        member: pc,
        by: Some(pa)
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
        text: "after".into(),
        id: 0,
        quote: None,
        mentions: Vec::new(),
        forwarded: false,
        preview: None,
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
            .handle(pa, GroupWire::message(g, vec![1, 2, 3]))
            .is_err()
    );
    let mut ok = Groups::new(&b);
    let g2 = ok.create("y").unwrap();
    assert!(ok.handle(pa, GroupWire::message(g2, vec![0; 64])).is_err());
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
        text: "still here".into(),
        id: 0,
        quote: None,
        mentions: Vec::new(),
        forwarded: false,
        preview: None,
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
        proposals: Vec::new(),
        sync: None,
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
        text: "via alice".into(),
        id: 0,
        quote: None,
        mentions: Vec::new(),
        forwarded: false,
        preview: None,
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
        proposals: Vec::new(),
        sync: None,
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
    // In a group without a roster the owner commits the removal itself,
    // but the commit says Carol left: no one sees the owner removed her.
    for p in [pa, pb] {
        assert!(net.take(&p).contains(&GroupEvent::MemberRemoved {
            group: g,
            member: pc,
            by: Some(pc)
        }));
    }
    let members = |n: &mut Net, p: &PublicIdentity| n.node(p).list()[0].3.len();
    assert_eq!(members(&mut net, &pa), 2);
    assert_eq!(members(&mut net, &pb), 2);
    // A leave request only works on the owner, from a member.
    assert!(
        net.node(&pb)
            .handle(
                pa,
                GroupWire::Leave {
                    group: g,
                    proposal: None,
                    signature: None,
                }
            )
            .is_err()
    );
    assert!(
        net.node(&pa)
            .handle(
                pc,
                GroupWire::Leave {
                    group: g,
                    proposal: None,
                    signature: None,
                }
            )
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

#[test]
fn removed_members_can_be_invited_again() {
    let (a, b) = (Identity::generate(), Identity::generate());
    let (pa, pb) = (a.public(), b.public());
    let mut net = Net::new(&[&a, &b]);
    let g = net.node(&pa).create("club").unwrap();
    let before = net.node(&pb).export().unwrap().len();
    let out = net.node(&pa).invite(&g, pb).unwrap();
    net.deliver(pa, out);
    let out = net.node(&pa).remove(&g, &pb).unwrap();
    net.deliver(pa, out);
    assert!(net.take(&pb).contains(&GroupEvent::Left { group: g }));
    // Nothing of the group is left behind: no old epoch secrets on disk.
    let after = net.node(&pb).export().unwrap().len();
    assert!(after <= before + 64, "{before} -> {after} bytes");

    let out = net.node(&pa).invite(&g, pb).unwrap();
    net.deliver(pa, out);
    assert!(
        net.take(&pb)
            .iter()
            .any(|e| matches!(e, GroupEvent::Joined { .. }))
    );
    let out = net.node(&pa).send_text(&g, "welcome back").unwrap();
    net.deliver(pa, out);
    assert!(net.take(&pb).contains(&GroupEvent::Text {
        group: g,
        from: pa,
        text: "welcome back".into(),
        id: 0,
        quote: None,
        mentions: Vec::new(),
        forwarded: false,
        preview: None,
    }));
}

#[test]
fn key_packages_for_unanswered_invitations_are_bounded() {
    let (a, b) = (Identity::generate(), Identity::generate());
    let (pa, pb) = (a.public(), b.public());
    let mut net = Net::new(&[&a, &b]);
    let size = |net: &mut Net| net.node(&pb).export().unwrap().len();
    let base = size(&mut net);
    net.node(&pb).accept_invite(&[0; 16], pa).unwrap();
    let one = size(&mut net) - base;

    // Asking again for the same group answers with the same key package.
    for _ in 0..10 {
        net.node(&pb).accept_invite(&[0; 16], pa).unwrap();
    }
    assert!(size(&mut net) < base + 2 * one);
    // A flood of requests keeps only the latest few.
    for i in 0..100u8 {
        net.node(&pb).accept_invite(&[i; 16], pa).unwrap();
    }
    let flooded = size(&mut net);
    assert!(
        flooded < base + (threnody_groups::MAX_KEY_PACKAGES + 1) * one,
        "{flooded} bytes"
    );
    // They survive a restart.
    let restored = Groups::restore(&b, &net.node(&pb).export().unwrap()).unwrap();
    assert_eq!(restored.export().unwrap().len(), flooded);

    // An invitation still works, and takes its key packages with it.
    let g = net.node(&pa).create("x").unwrap();
    let out = net.node(&pa).invite(&g, pb).unwrap();
    net.deliver(pa, out);
    assert!(
        net.take(&pb)
            .iter()
            .any(|e| matches!(e, GroupEvent::Joined { .. }))
    );
}

#[test]
fn a_welcome_built_from_the_first_of_several_answers_still_opens() {
    let (a, b) = (Identity::generate(), Identity::generate());
    let (pa, pb) = (a.public(), b.public());
    let mut net = Net::new(&[&a, &b]);
    let g = net.node(&pa).create("team").unwrap();
    // Alice asks twice (two of Bob's devices asked her to add a third,
    // say) and Bob answers both; she adds him with the first answer.
    net.node(&pa).invite(&g, pb).unwrap();
    net.node(&pa).invite(&g, pb).unwrap();
    let first = net.node(&pb).accept_invite(&g, pa).unwrap().send[0]
        .wire
        .clone();
    let second = net.node(&pb).accept_invite(&g, pa).unwrap().send[0]
        .wire
        .clone();
    let out = net.node(&pa).handle(pb, first).unwrap();
    // The second answer, from a member by now, changes nothing.
    let again = net.node(&pa).handle(pb, second).unwrap();
    assert!(again.send.is_empty() && again.events.is_empty());
    let w = out.send.into_iter().find(|o| o.to == pb).unwrap().wire;
    let res = net.node(&pb).handle(pa, w).unwrap();
    assert!(res.events.contains(&GroupEvent::Joined {
        group: g,
        name: "team".into(),
        owner: pa
    }));
}

#[test]
fn a_welcome_must_come_from_whom_we_gave_a_key_package() {
    let (a, b, c) = (
        Identity::generate(),
        Identity::generate(),
        Identity::generate(),
    );
    let (pa, pb, pc) = (a.public(), b.public(), c.public());
    let mut net = Net::new(&[&a, &b, &c]);
    // Alice's Welcome for Bob into `g`, Bob having given key packages to
    // those in `gave`.
    let welcome = |net: &mut Net, gave: &[PublicIdentity]| {
        let g = net.node(&pa).create("team").unwrap();
        net.accept = true;
        let out = net.node(&pa).invite(&g, pc).unwrap();
        net.deliver(pa, out);
        net.accept = false;
        net.node(&pa).invite(&g, pb).unwrap();
        let mut kp = None;
        for p in gave {
            let out = net.node(&pb).accept_invite(&g, *p).unwrap();
            if *p == pa {
                kp = Some(out.send[0].wire.clone());
            }
        }
        let out = net.node(&pa).handle(pb, kp.unwrap()).unwrap();
        let w = out.send.into_iter().find(|o| o.to == pb).unwrap().wire;
        (g, w)
    };

    // Passed on by Carol, another member Bob gave nothing: refused, and
    // Bob can still join from Alice, with Alice as the owner.
    let (g, w) = welcome(&mut net, &[pa]);
    assert!(net.node(&pb).handle(pc, w.clone()).is_err());
    assert!(net.node(&pb).list().is_empty());
    let res = net.node(&pb).handle(pa, w).unwrap();
    assert!(res.events.contains(&GroupEvent::Joined {
        group: g,
        name: "team".into(),
        owner: pa
    }));

    // Bob accepted Carol's invitation to that group too, but she didn't
    // make this Welcome: still refused.
    let (g, w) = welcome(&mut net, &[pa, pc]);
    assert!(net.node(&pb).handle(pc, w).is_err());
    assert!(!net.node(&pb).list().iter().any(|(id, ..)| *id == g));
}

#[test]
fn names_lose_characters_that_reorder_text() {
    let (a, b) = (Identity::generate(), Identity::generate());
    let mut gb = Groups::new(&b);
    let out = gb
        .handle(
            a.public(),
            GroupWire::KeyPackageRequest {
                group: [1; 16],
                name: "Your\u{202E}knab\u{2066} \u{200B}club\u{FEFF}\n".into(),
            },
        )
        .unwrap();
    assert!(matches!(
        &out.events[..],
        [GroupEvent::InviteRequested { name, .. }] if name == "Yourknab club"
    ));
}

#[test]
fn admins_are_known_to_every_member() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids);
    let (a, b, c) = (p[0], p[1], p[2]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    let roster = net.node(&a).roster(&g).unwrap();
    assert_eq!(roster.admins, vec![b]);
    assert_eq!((roster.lead, roster.term), (a, 0));
    for x in [b, c] {
        assert_eq!(net.node(&x).roster(&g).unwrap(), roster);
        assert_eq!(net.node(&x).role_of(&g, &b).unwrap(), MemberRole::Admin);
    }
    assert!(net.take(&c).contains(&GroupEvent::RoleChanged {
        group: g,
        member: b,
        role: MemberRole::Admin
    }));
    // Members still can't, and the admin demoted is a member again.
    assert!(matches!(
        net.node(&c).promote(&g, c),
        Err(GroupError::NotOwner)
    ));
    let out = net.node(&a).demote(&g, b).unwrap();
    net.deliver(a, out);
    assert_eq!(net.node(&c).role_of(&g, &b).unwrap(), MemberRole::Member);
    assert!(in_step(&mut net, &g, &p));
}

#[test]
fn admin_invites_and_removes_through_the_coordinator() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids[..3]);
    let (a, b, c) = (p[0], p[1], p[2]);
    let d_id = &ids[3];
    let d = d_id.public();
    net.nodes.insert(d, Groups::new(d_id));
    for x in [b, c] {
        let out = net.node(&a).promote(&g, x).unwrap();
        net.deliver(a, out);
    }
    net.take(&c);

    // Bob (an admin) invites Dave: the coordinator commits it, Bob passes
    // on the Welcome.
    let out = net.node(&b).invite(&g, d).unwrap();
    net.deliver(b, out);
    assert!(net.take(&d).contains(&GroupEvent::Joined {
        group: g,
        name: "team".into(),
        owner: a
    }));
    assert!(net.take(&c).contains(&GroupEvent::MemberAdded {
        group: g,
        member: d,
        by: Some(b)
    }));
    assert_eq!(net.node(&b).pending_changes(&g), 0);
    let everyone = [a, b, c, d];
    assert!(in_step(&mut net, &g, &everyone));

    // ...and removes him.
    let out = net.node(&b).remove(&g, &d).unwrap();
    net.deliver(b, out);
    assert!(net.take(&d).contains(&GroupEvent::Left { group: g }));
    for x in [a, c] {
        assert!(net.take(&x).contains(&GroupEvent::MemberRemoved {
            group: g,
            member: d,
            by: Some(b)
        }));
    }
    // Admins remove members only.
    assert!(matches!(
        net.node(&b).remove(&g, &c),
        Err(GroupError::NotAllowed(_))
    ));
    assert!(matches!(
        net.node(&b).remove(&g, &a),
        Err(GroupError::NotAllowed(_))
    ));
    assert!(in_step(&mut net, &g, &[a, b, c]));
}

#[test]
fn concurrent_admin_changes_never_fork() {
    for reverse in [false, true] {
        let (ids, _) = people(5);
        let (mut net, p, g) = team(&ids);
        let (a, b, c, d, e) = (p[0], p[1], p[2], p[3], p[4]);
        for x in [b, c] {
            let out = net.node(&a).promote(&g, x).unwrap();
            net.deliver(a, out);
        }
        for x in &p {
            net.take(x);
        }
        // Both propose at the same epoch.
        let ob = net.node(&b).remove(&g, &d).unwrap();
        let oc = net.node(&c).remove(&g, &e).unwrap();
        if reverse {
            net.deliver(c, oc);
            net.deliver(b, ob);
        } else {
            net.deliver(b, ob);
            net.deliver(c, oc);
        }
        assert_eq!(members(&mut net, &g, &a), {
            let mut m = vec![a, b, c];
            m.sort_by_key(|x| *x.as_bytes());
            m
        });
        assert!(in_step(&mut net, &g, &[a, b, c]));
        for x in [a, b, c] {
            assert!(
                !net.take(&x)
                    .iter()
                    .any(|e| matches!(e, GroupEvent::ChangeFailed { .. }))
            );
            assert_eq!(net.node(&x).pending_changes(&g), 0);
        }
    }
}

#[test]
fn duplicate_removals_apply_once() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a, b, c, d) = (p[0], p[1], p[2], p[3]);
    for x in [b, c] {
        let out = net.node(&a).promote(&g, x).unwrap();
        net.deliver(a, out);
    }
    net.take(&a);
    let ob = net.node(&b).remove(&g, &d).unwrap();
    let oc = net.node(&c).remove(&g, &d).unwrap();
    net.deliver(b, ob);
    net.deliver(c, oc);
    let removed = net
        .take(&a)
        .iter()
        .filter(|e| matches!(e, GroupEvent::MemberRemoved { member, .. } if *member == d))
        .count();
    assert_eq!(removed, 1);
    assert_eq!(net.node(&b).pending_changes(&g), 0);
    assert_eq!(net.node(&c).pending_changes(&g), 0);
    assert!(in_step(&mut net, &g, &[a, b, c]));
}

#[test]
fn admin_changes_wait_for_an_offline_coordinator() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids[..3]);
    let (a, b, c) = (p[0], p[1], p[2]);
    let d = ids[3].public();
    net.nodes.insert(d, Groups::new(&ids[3]));
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);

    net.offline = vec![a];
    let out = net.node(&b).invite(&g, d).unwrap();
    net.deliver(b, out);
    assert_eq!(net.node(&b).pending_changes(&g), 1);
    assert_eq!(members(&mut net, &g, &b).len(), 3);
    // Bob still talks, reacts and pins while his change waits.
    let out = net.node(&b).send_text(&g, "waiting").unwrap();
    net.deliver(b, out);
    assert!(net.take(&c).iter().any(
        |e| matches!(e, GroupEvent::Text { text, from, .. } if text == "waiting" && *from == b)
    ));
    let out = net
        .node(&b)
        .send(&g, &threnody_groups::Content::Pin { id: 7, pin: true })
        .unwrap();
    net.deliver(b, out);
    assert!(
        net.take(&c)
            .iter()
            .any(|e| matches!(e, GroupEvent::Pin { from, .. } if *from == b))
    );
    assert_eq!(net.node(&b).stored_proposals(&g), 1, "still pending");

    // The coordinator comes back: the add completes.
    net.offline.clear();
    net.release();
    assert_eq!(net.node(&b).pending_changes(&g), 0);
    assert!(net.node(&d).list().iter().any(|(id, ..)| *id == g));
    assert!(in_step(&mut net, &g, &[a, b, c, d]));
}

#[test]
fn future_epoch_messages_wait_for_their_commit() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids[..3]);
    let (a, c) = (p[0], p[2]);
    let d = ids[3].public();
    net.nodes.insert(d, Groups::new(&ids[3]));
    // Carol misses the add for now; Dave's first message reaches her first.
    net.offline = vec![c];
    let out = net.node(&a).invite(&g, d).unwrap();
    net.deliver(a, out);
    let out = net.node(&d).send_text(&g, "hi all").unwrap();
    net.deliver(d, out);
    net.offline.clear();
    net.queue.reverse();
    net.release();
    assert!(net.take(&c).iter().any(
        |e| matches!(e, GroupEvent::Text { text, from, .. } if text == "hi all" && *from == d)
    ));
    assert_eq!(net.node(&c).buffered(&g), 0);
}

#[test]
fn legacy_groups_upgrade_before_admins() {
    let (ids, _) = people(4);
    let refs: Vec<&Identity> = ids.iter().collect();
    let mut net = Net::new(&refs);
    let p: Vec<PublicIdentity> = ids.iter().map(Identity::public).collect();
    let (a, b, c, e) = (p[0], p[1], p[2], p[3]);
    // Bob runs a version without roles.
    net.nodes.insert(b, Groups::legacy_for_tests(&ids[1]));
    let g = net.node(&a).create("team").unwrap();
    for x in [b, c, e] {
        let out = net.node(&a).invite(&g, x).unwrap();
        net.deliver(a, out);
    }
    net.take(&a);
    let out = net.node(&a).promote(&g, c).unwrap();
    assert!(out.events.contains(&GroupEvent::UpgradePending {
        group: g,
        outdated: vec![b]
    }));
    net.deliver(a, out);
    assert!(!net.node(&c).has_roster(&g).unwrap());
    // Meanwhile Alice's commits are plain owner commits Bob takes.
    let out = net.node(&a).remove(&g, &e).unwrap();
    net.deliver(a, out);
    assert!(in_step(&mut net, &g, &[a, b, c]));

    // Bob updates: his leaf is upgraded, then the roster goes in.
    let state = net.node(&b).export().unwrap();
    net.nodes
        .insert(b, Groups::restore(&ids[1], &state).unwrap());
    let out = net.node(&b).resume(&g).unwrap();
    net.deliver(b, out);
    assert!(in_step(&mut net, &g, &[a, b, c]));
    for x in [a, b, c] {
        assert!(net.node(&x).has_roster(&g).unwrap());
        assert_eq!(net.node(&x).role_of(&g, &c).unwrap(), MemberRole::Admin);
        assert!(net.node(&x).outdated(&g).unwrap().is_empty());
    }
}

#[test]
fn old_clients_cannot_join_groups_with_admins() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids[..2]);
    let (a, b) = (p[0], p[1]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    let old = Groups::legacy_for_tests(&ids[2]);
    let l = ids[2].public();
    net.nodes.insert(l, old);
    net.node(&a).invite(&g, l).unwrap();
    let kp = net
        .node(&l)
        .accept_invite(&g, a)
        .unwrap()
        .send
        .remove(0)
        .wire;
    assert!(matches!(
        net.node(&a).handle(l, kp),
        Err(GroupError::NeedsUpdate(_))
    ));
}

#[test]
fn owner_transfers_and_leaves() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids);
    let (a, b, c) = (p[0], p[1], p[2]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::Transfer(vec![b])])
        .unwrap();
    net.deliver(a, out);
    for x in [a, b, c] {
        let r = net.node(&x).roster(&g).unwrap();
        assert_eq!((r.owners.clone(), r.lead, r.term), (vec![b], b, 1));
        assert_eq!(r.admins, vec![a]);
    }
    for x in &p {
        net.take(x);
    }
    let out = net.node(&a).leave(&g).unwrap();
    net.deliver(a, out);
    assert!(net.take(&c).contains(&GroupEvent::MemberRemoved {
        group: g,
        member: a,
        by: Some(a)
    }));
    assert!(net.node(&b).roster(&g).unwrap().admins.is_empty());
    assert!(in_step(&mut net, &g, &[b, c]));
}

#[test]
fn last_owner_cannot_just_leave() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids);
    let (a, b) = (p[0], p[1]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    assert!(matches!(
        net.node(&a).leave(&g),
        Err(GroupError::NotAllowed(_))
    ));
    // Admins can't delete it; the owner can.
    assert!(matches!(
        net.node(&b).disband(&g),
        Err(GroupError::NotOwner)
    ));
    let out = net.node(&a).disband(&g).unwrap();
    net.deliver(a, out);
    for x in &p {
        assert!(net.node(x).list().is_empty());
    }
}

#[test]
fn owner_device_takes_over_the_coordinator() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a1, a2, c, d) = (p[0], p[1], p[2], p[3]);
    let out = net
        .node(&a1)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
        .unwrap();
    net.deliver(a1, out);
    assert_eq!(net.node(&c).role_of(&g, &a2).unwrap(), MemberRole::Owner);
    // The coordinator is away; the other owner device's remove waits...
    net.offline = vec![a1];
    let out = net.node(&a2).remove(&g, &d).unwrap();
    net.deliver(a2, out);
    assert_eq!(net.node(&a2).pending_changes(&g), 1);
    // ...until it takes over.
    let out = net.node(&a2).take_over(&g).unwrap();
    net.deliver(a2, out);
    for x in [a2, c] {
        let r = net.node(&x).roster(&g).unwrap();
        assert_eq!((r.lead, r.term), (a2, 1));
        assert!(!members(&mut net, &g, &x).contains(&d));
    }
    assert_eq!(net.node(&a2).pending_changes(&g), 0);
    // The old coordinator, back, commits the remove it finds waiting before
    // it sees the takeover: it forks off, and is brought back.
    net.offline.clear();
    net.lenient = true;
    net.release();
    assert!(in_step(&mut net, &g, &[a1, a2, c]), "{:?}", net.errors);
    assert!(!net.node(&a1).is_lead(&g));
    assert!(!net.node(&a1).out_of_step(&g));
}

#[test]
fn a_pending_handover_does_not_block_a_takeover() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a1, a2, c, d) = (p[0], p[1], p[2], p[3]);
    let out = net
        .node(&a1)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
        .unwrap();
    net.deliver(a1, out);
    let out = net.node(&a1).promote(&g, c).unwrap();
    net.deliver(a1, out);
    // With the coordinator away, the other owner device asks to hand the
    // group to Carol, and to remove Dave.
    net.offline = vec![a1];
    let out = net
        .node(&a2)
        .change_roster(&g, &[RosterChange::Transfer(vec![c])])
        .unwrap();
    net.deliver(a2, out);
    let out = net.node(&a2).remove(&g, &d).unwrap();
    net.deliver(a2, out);
    assert_eq!(net.node(&a2).pending_changes(&g), 2);
    // Taking over still works; the handover follows in the next commit.
    let out = net.node(&a2).take_over_removing(&g, &[a1]).unwrap();
    net.deliver(a2, out);
    let r = net.node(&c).roster(&g).unwrap();
    assert_eq!((r.lead, r.owners.clone()), (c, vec![c]));
    assert!(!members(&mut net, &g, &c).contains(&d));
    assert!(!members(&mut net, &g, &c).contains(&a1));
    assert_eq!(net.node(&a2).pending_changes(&g), 0);
}

#[test]
fn successor_claims_when_the_owner_is_gone() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids);
    let (a, b, c) = (p[0], p[1], p[2]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::Successor(vec![b])])
        .unwrap();
    net.deliver(a, out);
    // Only the successor may claim.
    assert!(matches!(
        net.node(&c).claim(&g),
        Err(GroupError::NotAllowed(_))
    ));
    net.offline = vec![a];
    let out = net.node(&b).claim(&g).unwrap();
    net.deliver(b, out);
    for x in [b, c] {
        let r = net.node(&x).roster(&g).unwrap();
        assert_eq!((r.owners.clone(), r.lead, r.term), (vec![b], b, 1));
        assert_eq!(r.admins, vec![a]);
        assert!(r.successors.is_empty());
    }
    // The old owner, back, is an admin.
    net.offline.clear();
    net.release();
    assert_eq!(net.node(&a).role_of(&g, &a).unwrap(), MemberRole::Admin);
    assert!(in_step(&mut net, &g, &p));
}

#[test]
fn fork_after_takeover_is_repaired() {
    let (ids, _) = people(5);
    let (mut net, p, g) = team(&ids);
    let (a, a2, c, d, e) = (p[0], p[1], p[2], p[3], p[4]);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
        .unwrap();
    net.deliver(a, out);
    let out = net.node(&a).send_text(&g, "before").unwrap();
    net.deliver(a, out);
    // The coordinator commits while the other owner device takes over:
    // Carol follows the one, Dave the other.
    let oa = net.node(&a).remove(&g, &e).unwrap();
    let oa2 = net.node(&a2).take_over(&g).unwrap();
    net.deliver_only(a, oa, &[c]);
    net.deliver_only(a2, oa2, &[d]);
    assert!(!in_step(&mut net, &g, &[c, d]));
    for x in &p {
        net.take(x);
    }
    // Dave writes: Carol sees she's behind and is brought back.
    net.lenient = true;
    let out = net.node(&d).send_text(&g, "hello?").unwrap();
    net.deliver_only(d, out, &[c]);
    assert!(net.take(&c).contains(&GroupEvent::OutOfStep { group: g }));
    assert!(in_step(&mut net, &g, &[a2, c, d]));
    assert!(!net.node(&c).out_of_step(&g));
    assert_eq!(net.node(&c).roster(&g).unwrap().lead, a2);
    // And reads what follows.
    let out = net.node(&d).send_text(&g, "welcome back").unwrap();
    net.deliver(d, out);
    assert!(
        net.take(&c)
            .iter()
            .any(|e| matches!(e, GroupEvent::Text { text, .. } if text == "welcome back"))
    );
}

#[test]
fn simultaneous_takeovers_converge() {
    let (ids, _) = people(5);
    let (mut net, p, g) = team(&ids);
    // p is sorted: p[1] < p[2], so p[1]'s branch wins a tie.
    let (a1, a2, a3, c, d) = (p[0], p[1], p[2], p[3], p[4]);
    let out = net
        .node(&a1)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2, a3])])
        .unwrap();
    net.deliver(a1, out);
    let o2 = net.node(&a2).take_over(&g).unwrap();
    let o3 = net.node(&a3).take_over(&g).unwrap();
    net.deliver_only(a2, o2, &[c, a1]);
    net.deliver_only(a3, o3, &[d]);
    net.lenient = true;
    // One message from each side.
    let out = net.node(&c).send_text(&g, "from c").unwrap();
    net.deliver(c, out);
    let out = net.node(&d).send_text(&g, "from d").unwrap();
    net.deliver(d, out);
    let everyone = [a1, a2, a3, c, d];
    assert!(in_step(&mut net, &g, &everyone), "{:?}", net.errors);
    for x in everyone {
        assert_eq!(net.node(&x).roster(&g).unwrap().lead, a2);
    }
}

#[test]
fn forged_resync_causes_no_churn() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a, a2, c, d) = (p[0], p[1], p[2], p[3]);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
        .unwrap();
    net.deliver(a, out);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::Lead(a2)])
        .unwrap();
    net.deliver(a, out);
    let (epoch, auth) = net.node(&a2).epoch_state(&g).unwrap();
    let roster = net.node(&a2).roster(&g).unwrap();
    let hint = |term: u32| threnody_groups::SyncHint {
        epoch,
        term,
        auth: auth[..8].try_into().unwrap(),
        lead: roster.lead.as_bytes()[..8].try_into().unwrap(),
    };
    // Carol claims Dave is behind, at the group's own rank: nothing.
    let forged = GroupWire::Resync {
        group: g,
        member: Some(*d.as_bytes()),
        sync: hint(roster.term),
    };
    assert!(net.node(&a2).handle(c, forged).unwrap().send.is_empty());
    // A Dave really behind is brought back once per term, not twice.
    let stale = GroupWire::Resync {
        group: g,
        member: Some(*d.as_bytes()),
        sync: hint(roster.term - 1),
    };
    let first = net.node(&a2).handle(c, stale.clone()).unwrap();
    assert!(matches!(
        &first.send[..],
        [o] if o.to == d && matches!(o.wire, GroupWire::KeyPackageRequest { .. })
    ));
    assert!(net.node(&a2).handle(c, stale).unwrap().send.is_empty());
}

#[test]
fn leaves_reach_the_lead_in_any_order() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a, a2, b, c) = (p[0], p[1], p[2], p[3]);
    let out = net
        .node(&a)
        .change_roster(&g, &[RosterChange::AddOwners(vec![a2])])
        .unwrap();
    net.deliver(a, out);
    // Alice hands over to her other device; Bob merges that before it does.
    let oa = net
        .node(&a)
        .change_roster(&g, &[RosterChange::Lead(a2)])
        .unwrap();
    let for_a2: Vec<_> = oa.send.iter().filter(|o| o.to == a2).cloned().collect();
    net.deliver_only(a, oa, &[b, c]);
    for x in &p {
        net.take(x);
    }
    let out = net.node(&b).leave(&g).unwrap();
    net.deliver(b, out);
    assert_eq!(
        net.node(&a2).buffered(&g),
        1,
        "the leave waits for its epoch"
    );
    net.deliver(
        a,
        Output {
            send: for_a2,
            events: vec![],
        },
    );
    assert!(net.take(&c).contains(&GroupEvent::MemberRemoved {
        group: g,
        member: b,
        by: Some(b)
    }));
    assert!(in_step(&mut net, &g, &[a, a2, c]));
}

#[test]
fn leaves_committed_by_the_coordinator_still_read_as_leaves() {
    let (ids, _) = people(4);
    let (mut net, p, g) = team(&ids);
    let (a, b, c, d) = (p[0], p[1], p[2], p[3]);
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    assert!(net.node(&a).has_roster(&g).unwrap());
    // Dave misses a commit, so his own Remove is for an epoch gone by:
    // Alice removes him herself.
    net.offline.push(d);
    let out = net.node(&a).promote(&g, c).unwrap();
    net.deliver(a, out);
    for x in &p {
        net.take(x);
    }
    let out = net.node(&d).leave(&g).unwrap();
    net.deliver(d, out);
    for x in [a, b, c] {
        let ev = net.take(&x);
        assert!(
            ev.contains(&GroupEvent::MemberRemoved {
                group: g,
                member: d,
                by: Some(d)
            }),
            "{x:?}: {ev:?}"
        );
    }
    assert_eq!(members(&mut net, &g, &b), vec![a, b, c]);

    // A removal is still one.
    let out = net.node(&a).remove(&g, &c).unwrap();
    net.deliver(a, out);
    for x in [a, b] {
        assert!(net.take(&x).contains(&GroupEvent::MemberRemoved {
            group: g,
            member: c,
            by: Some(a)
        }));
    }
}

#[test]
fn a_leave_without_its_members_signature_reads_as_a_removal() {
    let (ids, _) = people(3);
    let (mut net, p, g) = team(&ids);
    let (a, b, c) = (p[0], p[1], p[2]);
    for x in &p {
        net.take(x);
    }
    // Carol runs a version that doesn't sign her leave: nothing tells
    // Alice's commit from a removal.
    let mut out = net.node(&c).leave(&g).unwrap();
    for o in &mut out.send {
        if let GroupWire::Leave { signature, .. } = &mut o.wire {
            assert!(signature.take().is_some(), "signed");
        }
    }
    net.deliver(c, out);
    for x in [a, b] {
        assert!(net.take(&x).contains(&GroupEvent::MemberRemoved {
            group: g,
            member: c,
            by: Some(a)
        }));
    }
    assert_eq!(members(&mut net, &g, &b), vec![a, b]);
}

#[test]
fn members_without_roles_take_a_leave_marked_as_one() {
    let (ids, _) = people(3);
    let refs: Vec<&Identity> = ids.iter().collect();
    let mut net = Net::new(&refs);
    let p: Vec<PublicIdentity> = ids.iter().map(Identity::public).collect();
    let (a, b, c) = (p[0], p[1], p[2]);
    // Bob runs a version without roles: the marked commit is a plain
    // owner commit to him.
    net.nodes.insert(b, Groups::legacy_for_tests(&ids[1]));
    let g = net.node(&a).create("team").unwrap();
    for x in [b, c] {
        let out = net.node(&a).invite(&g, x).unwrap();
        net.deliver(a, out);
    }
    let out = net.node(&c).leave(&g).unwrap();
    net.deliver(c, out);
    assert!(net.take(&b).iter().any(|e| matches!(
        e,
        GroupEvent::MemberRemoved { member, .. } if *member == c
    )));
    assert!(in_step(&mut net, &g, &[a, b]));
    assert_eq!(members(&mut net, &g, &b), vec![a, b]);
    // And the next message goes through as ever.
    let out = net.node(&a).send_text(&g, "still here").unwrap();
    net.deliver(a, out);
    assert!(
        net.take(&b)
            .iter()
            .any(|e| matches!(e, GroupEvent::Text { .. }))
    );
}

#[test]
fn legacy_leave_is_bare() {
    let (ids, _) = people(2);
    let mut net = Net::new(&[&ids[0], &ids[1]]);
    let (a, b) = (ids[0].public(), ids[1].public());
    net.nodes.insert(a, Groups::legacy_for_tests(&ids[0]));
    let g = net.node(&a).create("old").unwrap();
    let out = net.node(&a).invite(&g, b).unwrap();
    net.deliver(a, out);
    let out = net.node(&b).leave(&g).unwrap();
    assert!(matches!(
        &out.send[..],
        [o] if matches!(o.wire, GroupWire::Leave { group, proposal: None, .. } if group == g)
    ));
    net.deliver(b, out);
    assert_eq!(members(&mut net, &g, &a), vec![a]);
}

#[test]
fn roster_and_intents_survive_export_and_restore() {
    let (ids, _) = people(5);
    let (mut net, p, g) = team(&ids[..4]);
    let (a, b, c, d) = (p[0], p[1], p[2], p[3]);
    let e = ids[4].public();
    net.nodes.insert(e, Groups::new(&ids[4]));
    let out = net.node(&a).promote(&g, b).unwrap();
    net.deliver(a, out);
    // Bob's remove waits for Alice; Alice's invitation waits for Eve.
    net.offline = vec![a, e];
    let out = net.node(&b).remove(&g, &d).unwrap();
    net.deliver(b, out);
    net.offline = vec![b, c, e];
    let out = net.node(&a).invite(&g, e).unwrap();
    net.deliver(a, out);
    for (i, x) in ids.iter().enumerate().take(4) {
        let _ = i;
        net.restart(x);
    }
    assert_eq!(net.node(&b).pending_changes(&g), 1);
    assert_eq!(net.node(&b).roster(&g).unwrap().admins, vec![b]);
    net.offline.clear();
    net.release();
    assert!(!members(&mut net, &g, &a).contains(&d));
    assert!(
        members(&mut net, &g, &a).contains(&e),
        "no longer unsolicited"
    );
    assert_eq!(net.node(&b).pending_changes(&g), 0);
    assert!(in_step(&mut net, &g, &[a, b, c, e]));
}

#[test]
fn groups_survive_export_and_restore_as_older_versions_read_them() {
    let (ids, _) = people(2);
    let (mut net, p, g) = team(&ids);
    let out = net.node(&p[0]).promote(&g, p[1]).unwrap();
    net.deliver(p[0], out);
    let state = net.node(&p[0]).export().unwrap();
    // { 0: 1, ... }: still version 1, so older versions read it.
    assert_eq!(&state[..3], &[0xa7, 0x00, 0x01]);
    net.restart(&ids[0]);
    assert_eq!(net.node(&p[0]).roster(&g).unwrap().admins, vec![p[1]]);
}
