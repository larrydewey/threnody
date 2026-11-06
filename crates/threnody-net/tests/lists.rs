//! Lists on a running node: what others send can't take them over.

use threnody_core::history::ConversationId;
use threnody_core::lists::{Item, List, MAX_ITEM_TEXT, MAX_ITEMS, MAX_LISTS, Stamp};
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

    let id = node.create_list("Trip").unwrap();
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

/// A list from someone else, `items` long, each item as long as allowed.
fn big(id: u8, items: usize) -> List {
    let stamp = Stamp {
        ms: now_ms(),
        by: [0xee; 32],
    };
    List {
        id: [id; 16],
        title: format!("junk {id}"),
        title_stamp: stamp,
        items: (0..items as u64)
            .map(|n| Item {
                id: n + 1,
                text: "\u{1F4A9}".repeat(MAX_ITEM_TEXT),
                pos: n as f64,
                stamp,
                ..Item::default()
            })
            .collect(),
    }
}

#[tokio::test]
async fn lists_from_others_cant_stop_ours_being_saved() {
    let dir = tempfile::tempdir().unwrap();
    let (node, _rx) = spawn(&dir, "alice");
    let group = ConversationId::Group([9; 16]);
    let mallory = Identity::generate().public();
    // A member sends lists as large as lists go: about 2 MB each, more
    // than the 16 MiB the stored lists can take.
    for n in 0..9 {
        node.on_shared_list(group, mallory, &big(n + 1, MAX_ITEMS).encode().unwrap());
    }
    assert!(node.lists().len() < 9, "only some are kept");
    // Ours are still kept.
    let id = node.create_list("Groceries").unwrap();
    node.edit_list(&id, |l, s| l.add(None, "eggs", false, s).is_some());
    let home = Home::new(dir.path().join("alice"));
    let identity = home.load_identity(None).unwrap();
    drop(node);
    let (node, _rx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    let l = node.list(&id).expect("kept after a restart").list;
    assert_eq!(l.items()[0].text, "eggs");
}

#[tokio::test]
async fn one_contact_cant_take_every_list_slot() {
    let dir = tempfile::tempdir().unwrap();
    let (node, _rx) = spawn(&dir, "alice");
    let x = Identity::generate().public();
    let y = Identity::generate().public();
    // X sends as many small lists as we keep.
    for n in 0..MAX_LISTS {
        let mut l = big(0, 1);
        l.id[..8].copy_from_slice(&(n as u64).to_le_bytes());
        node.on_shared_list(node.conversation_for(&x), x, &l.encode().unwrap());
    }
    assert!(node.lists().len() < MAX_LISTS / 2);
    // Y's still arrives, and ours still go in and are kept.
    assert!(
        node.on_shared_list(node.conversation_for(&y), y, &big(7, 1).encode().unwrap())
            .is_some()
    );
    while node.lists().len() < MAX_LISTS {
        node.create_list("mine").expect("room for our own");
    }
    assert_eq!(node.create_list("one more"), None, "but not past the limit");
}
