//! Wake-ups for sleeping devices (Appendix R).

use std::time::Duration;

use threnody_core::AppMessage;
use threnody_core::push::PushTarget;
use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

struct N {
    node: Node,
    rx: UnboundedReceiver<Event>,
    addr: String,
}

async fn spawn(dir: &tempfile::TempDir, name: &str) -> N {
    let home = Home::new(dir.path().join(name));
    let identity = home.create_identity(None).unwrap();
    let (node, rx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    let addr = node.listen("127.0.0.1:0").await.unwrap().to_string();
    N { node, rx, addr }
}

async fn next(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(20), async {
        loop {
            let e = rx.recv().await.expect("event stream open");
            if pred(&e) {
                return e;
            }
        }
    })
    .await
    .expect("expected event did not arrive")
}

async fn link(x: &mut N, y: &mut N) {
    let yid = x.node.connect(&y.addr, None).await.unwrap();
    next(&mut y.rx, |e| matches!(e, Event::Connected { .. })).await;
    x.node.set_approval(&yid, true).unwrap();
    y.node.set_approval(&x.node.identity(), true).unwrap();
    next(&mut x.rx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
    next(&mut y.rx, |e| {
        matches!(e, Event::ApprovalChanged { mutual: true, .. })
    })
    .await;
}

async fn wait_for(secs: u64, cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(secs), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition not reached");
}

fn text(s: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
        id: 0,
        quote: None,
        forwarded: false,
    }
}

/// A one-request push server answering `status`; yields its URL and the
/// request it got (head, body).
async fn push_server(status: &str) -> (String, tokio::sync::oneshot::Receiver<(String, Vec<u8>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/up/device", listener.local_addr().unwrap());
    let (tx, rx) = tokio::sync::oneshot::channel();
    let status = status.to_owned();
    tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let (head, body) = loop {
            let n = s.read(&mut chunk).await.unwrap();
            buf.extend_from_slice(&chunk[..n]);
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map_or(0, |v| v.trim().parse().unwrap());
                while buf.len() < i + 4 + len {
                    let n = s.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                break (head, buf[i + 4..i + 4 + len].to_vec());
            }
            assert!(n > 0, "connection closed early");
        };
        s.write_all(format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let _ = tx.send((head, body));
    });
    (url, rx)
}

fn target(url: &str) -> PushTarget {
    PushTarget {
        url: url.into(),
        keys: None,
        dummies_per_day: 0,
        expires_ms: 0,
    }
}

#[tokio::test]
async fn a_mailbox_wakes_a_sleeping_recipient_with_an_empty_ping() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b, mut r) = (
        spawn(&dir, "a").await,
        spawn(&dir, "b").await,
        spawn(&dir, "r").await,
    );
    link(&mut a, &mut b).await;
    link(&mut a, &mut r).await;
    link(&mut b, &mut r).await;
    let bid = b.node.identity();
    wait_for(10, || a.node.can_send_offline(&bid)).await;

    let (url, got) = push_server("201 Created").await;
    b.node.set_push_target(Some(target(&url))).unwrap();
    // Every mutually approved contact connected now learns it.
    wait_for(10, || {
        r.node.push_targets() == 1 && a.node.push_targets() == 1
    })
    .await;
    assert_eq!(b.node.push_target().unwrap().url, url);

    // B goes to sleep.
    a.node.disconnect(&bid);
    r.node.disconnect(&bid);
    wait_for(10, || {
        a.node.sessions().iter().all(|s| s.peer != bid)
            && r.node.sessions().iter().all(|s| s.peer != bid)
    })
    .await;

    a.node.send_offline(&bid, &text("wake up")).unwrap();
    wait_for(10, || r.node.held_messages() == 1).await;
    // R holds it, so R wakes B: after a random wait, an empty POST.
    let (head, body) = timeout(Duration::from_secs(40), got)
        .await
        .expect("no wake-up")
        .unwrap();
    assert!(head.starts_with("post /up/device "), "{head}");
    assert!(head.contains("\nttl: 86400"), "{head}");
    assert!(head.contains("\nurgency: high"), "{head}");
    assert!(!head.contains("user-agent"), "{head}");
    assert_eq!(body.len(), threnody_net::push::WAKE_CONTENT);
    assert!(!body.windows(4).any(|w| w == b"wake"));

    // Withdrawn: contacts forget it.
    b.node.set_push_target(None).unwrap();
    b.node.connect(&r.addr, None).await.unwrap();
    wait_for(10, || r.node.push_targets() == 0).await;
}

#[tokio::test]
async fn a_sender_wakes_the_recipient_itself_and_forgets_a_gone_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, mut b) = (spawn(&dir, "a").await, spawn(&dir, "b").await);
    link(&mut a, &mut b).await;
    let bid = b.node.identity();
    let (url, got) = push_server("410 Gone").await;
    b.node.set_push_target(Some(target(&url))).unwrap();
    wait_for(10, || a.node.push_targets() == 1).await;
    a.node.disconnect(&bid);
    wait_for(10, || a.node.sessions().iter().all(|s| s.peer != bid)).await;

    // No mailbox anywhere: the message waits at A for a session, and A
    // wakes B.
    let r = a.node.send_text(&bid, "are you there").unwrap();
    assert_eq!(r.queued, 1);
    timeout(Duration::from_secs(40), got)
        .await
        .expect("no wake-up")
        .unwrap();
    // The endpoint is gone (B unregistered it): A forgets it.
    wait_for(10, || a.node.push_targets() == 0).await;
}

#[tokio::test]
async fn strangers_get_no_endpoint_and_bad_ones_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (mut a, b) = (spawn(&dir, "a").await, spawn(&dir, "b").await);
    // Connected but never approved: no endpoint goes to B.
    a.node.connect(&b.addr, None).await.unwrap();
    a.node
        .set_push_target(Some(target("http://127.0.0.1:9/x")))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(b.node.push_targets(), 0);
    // Malformed endpoints are refused.
    assert!(
        a.node
            .set_push_target(Some(target("http://push.example/x")))
            .is_err()
    );
    let _ = &mut a.rx;
}
