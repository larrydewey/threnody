//! A real group call among three nodes: a WebRTC leg between every two,
//! one of them (Bob to Carol) relayed by the hub (Alice) because they
//! have no session, and every participant's video reaching the others.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libwebrtc::video_frame::I420Buffer;
use threnody_core::PublicIdentity;
use threnody_core::store::Home;
use threnody_media::GroupMedia;
use threnody_net::{AcceptPolicy, Node, NodeConfig};
use tokio::time::timeout;

const GROUP: [u8; 16] = [3; 16];
const CALL: u64 = 77;
const SECRET: [u8; 32] = [5; 32];

fn node(dir: &tempfile::TempDir, name: &str) -> Node {
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
    .0
}

async fn connect(a: &Node, b: &Node) {
    let addr = b.listen_quic("127.0.0.1:0").await.unwrap();
    a.listen_quic("127.0.0.1:0").await.unwrap();
    a.connect_quic(addr, Some(b.identity().fingerprint()))
        .await
        .unwrap();
    let (ai, bi) = (a.identity(), b.identity());
    timeout(Duration::from_secs(10), async {
        while !(a.sessions().iter().any(|s| s.peer == bi)
            && b.sessions().iter().any(|s| s.peer == ai))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("connected");
}

type Registry = Arc<Mutex<HashMap<PublicIdentity, Arc<GroupMedia>>>>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_people_see_each_other_through_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (node(&dir, "a"), node(&dir, "b"), node(&dir, "c"));
    connect(&b, &a).await;
    connect(&c, &a).await;
    let ids = [a.identity(), b.identity(), c.identity()];

    // The roster, as the group layer would build it.
    a.start_group_call(GROUP, CALL, SECRET, true).unwrap();
    for n in [&b, &c] {
        assert!(n.group_call_rang(GROUP, CALL, ids[0], ids[0], SECRET, true));
    }
    b.join_group_call(CALL, true).unwrap();
    a.group_call_joined(CALL, ids[1], true);
    c.group_call_joined(CALL, ids[1], true);
    c.join_group_call(CALL, true).unwrap();
    a.group_call_joined(CALL, ids[2], true);
    b.group_call_joined(CALL, ids[2], true);

    // Leg signalling goes straight to the other side's media (the group
    // layer's job in the apps).
    let registry: Registry = Arc::default();
    let connected = Arc::new(AtomicUsize::new(0));
    let mut medias = Vec::new();
    for n in [&a, &b, &c] {
        let me = n.identity();
        let reg = registry.clone();
        let rt = tokio::runtime::Handle::current();
        let out: threnody_media::group::SignalOut = Arc::new(move |peer, data| {
            let reg = reg.clone();
            rt.spawn(async move {
                // The peer's media may not exist yet: wait for it.
                loop {
                    let m = reg.lock().unwrap().get(&peer).cloned();
                    if let Some(m) = m {
                        m.signal(me, data).await;
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
        });
        let k = connected.clone();
        let m = Arc::new(GroupMedia::new(
            n.clone(),
            CALL,
            false,
            true,
            out,
            Arc::new(move || {
                k.fetch_add(1, Ordering::SeqCst);
            }),
        ));
        registry.lock().unwrap().insert(me, m.clone());
        medias.push(m);
    }
    for (i, m) in medias.iter().enumerate() {
        for (j, p) in ids.iter().enumerate() {
            if i != j {
                m.add(*p).await.unwrap();
            }
        }
    }
    // Six leg ends connect.
    timeout(Duration::from_secs(20), async {
        while connected.load(Ordering::SeqCst) < 6 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("every leg connects");

    // Everyone sends frames; everyone sees both others.
    let senders: Vec<_> = medias
        .iter()
        .map(|m| {
            let m = m.clone();
            tokio::spawn(async move {
                let f = I420Buffer::new(320, 240);
                for _ in 0..200 {
                    m.send_video(&f, 0);
                    tokio::time::sleep(Duration::from_millis(33)).await;
                }
            })
        })
        .collect();
    for (i, m) in medias.iter().enumerate() {
        let mut seen = std::collections::HashSet::new();
        timeout(Duration::from_secs(20), async {
            while seen.len() < 2 {
                if let Some((p, _)) = m.next_frame(Duration::from_millis(200)).await {
                    seen.insert(p);
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("participant {i} saw only {seen:?}"));
        assert!(!seen.contains(&ids[i]));
    }
    for s in senders {
        s.abort();
    }
}
