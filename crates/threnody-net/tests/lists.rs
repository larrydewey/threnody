//! Lists on a running node: what others send can't take them over.

use threnody_core::history::ConversationId;
use threnody_core::lists::{List, Stamp};
use threnody_core::store::Home;
use threnody_core::{Identity, now_ms};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;

fn spawn(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>) {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap()
}

#[tokio::test]
async fn a_forged_stamp_from_a_member_does_not_lock_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let (node, _rx) = spawn(&dir, "alice");
    let group = ConversationId::Group([9; 16]);
    let mallory = Identity::generate().public();

    let id = node.create_list("Trip");
    node.edit_list(&id, |l, s| l.add(None, "tickets", false, s).is_some());
    node.share_list(&id, group);

    // A member of the group renames it and changes the item, stamped as
    // far ahead as a stamp goes.
    let mut forged: List = node.list(&id).unwrap().list;
    forged.title = "PWNED".into();
    forged.title_stamp = Stamp {
        ms: u64::MAX,
        by: [0xff; 32],
    };
    forged.items[0].text = "nothing".into();
    forged.items[0].stamp = forged.title_stamp;
    node.on_shared_list(group, mallory, &forged.encode().unwrap());
    let l = node.list(&id).unwrap().list;
    assert_eq!(l.title, "Trip");
    assert_eq!(l.items()[0].text, "tickets");

    // One a little ahead (a fast clock) is taken, but the next change
    // here still wins over it.
    let mut ahead = l.clone();
    let soon = Stamp {
        ms: now_ms() + 60_000,
        by: [0xff; 32],
    };
    ahead.rename("Holiday", soon);
    node.on_shared_list(group, mallory, &ahead.encode().unwrap());
    assert_eq!(node.list(&id).unwrap().list.title, "Holiday");
    node.edit_list(&id, |l, s| {
        l.rename("Trip", s);
        true
    });
    let l = node.list(&id).unwrap().list;
    assert_eq!(l.title, "Trip");
    assert!(l.title_stamp > soon);
}
