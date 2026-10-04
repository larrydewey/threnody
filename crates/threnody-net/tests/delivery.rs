//! End-to-end acknowledgements: messages sent on a link that died
//! unnoticed are sent again in the next session, delivered once, and old
//! peers without the feature still get plain messages.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity};
use threnody_net::frame::{read_frame, write_frame};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig, handshake};
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn node(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>) {
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

async fn next(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(10), async {
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

/// Fails unless no event matching `pred` arrives for a while.
async fn none(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) {
    let r = timeout(Duration::from_millis(500), async {
        loop {
            let e = rx.recv().await.expect("event stream open");
            if pred(&e) {
                return e;
            }
        }
    })
    .await;
    assert!(r.is_err(), "unexpected event: {r:?}");
}

async fn wait_for(cond: impl Fn() -> bool) {
    timeout(Duration::from_secs(10), async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("condition never held");
}

fn text(s: &str) -> AppMessage {
    AppMessage::Text {
        sent_ms: 0,
        body: s.into(),
        expires_in_s: None,
        id: 0,
    }
}

fn is_text(e: &Event, body: &str) -> bool {
    matches!(e, Event::Message { msg: AppMessage::Text { body: b, .. }, .. } if b == body)
}

/// One end of a link whose writes vanish (reported as written) once `cut`
/// is set: a connection that died without either end noticing.
struct Lossy {
    inner: DuplexStream,
    cut: Arc<AtomicBool>,
}

impl AsyncRead for Lossy {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Lossy {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.cut.load(Ordering::SeqCst) {
            return Poll::Ready(Ok(buf.len()));
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Connects `a` to `b` over a lossy link; returns the cut switches for
/// what `a` writes and what `b` writes.
async fn lossy_link(a: &Node, b: &Node) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let (x, y) = tokio::io::duplex(1 << 20);
    let (cut_a, cut_b) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (xa, yb) = (
        Lossy {
            inner: x,
            cut: cut_a.clone(),
        },
        Lossy {
            inner: y,
            cut: cut_b.clone(),
        },
    );
    let b2 = b.clone();
    let accept = tokio::spawn(async move { b2.accept_stream(yb, "link", "b".into()).await });
    a.connect_stream(xa, "link", "a".into(), Some(b.identity().fingerprint()))
        .await
        .unwrap();
    accept.await.unwrap().unwrap();
    // `b` wants `a`'s messages (else they'd be requests).
    b.accept_contact(&a.identity());
    (cut_a, cut_b)
}

#[tokio::test]
async fn messages_lost_on_a_dead_link_are_sent_again_on_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    let b = bob.identity();
    let (cut_a, _) = lossy_link(&alice, &bob).await;

    alice.send(&b, text("first")).unwrap();
    next(&mut brx, |e| is_text(e, "first")).await;
    wait_for(|| alice.unacked(&b) == 0).await;

    // The link dies silently: the send "succeeds" and nothing arrives.
    cut_a.store(true, Ordering::SeqCst);
    alice.send(&b, text("into the void")).unwrap();
    none(&mut brx, |e| is_text(e, "into the void")).await;
    assert_eq!(alice.unacked(&b), 1);

    // A new session (here over TCP) carries it after all.
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    alice
        .connect(&addr.to_string(), Some(b.fingerprint()))
        .await
        .unwrap();
    next(&mut brx, |e| is_text(e, "into the void")).await;
    wait_for(|| alice.unacked(&b) == 0).await;
    none(&mut brx, |e| matches!(e, Event::Message { .. })).await;
}

#[tokio::test]
async fn a_message_whose_ack_was_lost_is_delivered_once() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    let b = bob.identity();
    let (_, cut_b) = lossy_link(&alice, &bob).await;
    // Let the Hellos cross, then lose everything Bob says.
    alice.send(&b, text("warm up")).unwrap();
    next(&mut brx, |e| is_text(e, "warm up")).await;
    wait_for(|| alice.unacked(&b) == 0).await;
    cut_b.store(true, Ordering::SeqCst);

    alice.send(&b, text("once")).unwrap();
    next(&mut brx, |e| is_text(e, "once")).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(alice.unacked(&b), 1, "the ack was lost");

    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    alice
        .connect(&addr.to_string(), Some(b.fingerprint()))
        .await
        .unwrap();
    wait_for(|| alice.unacked(&b) == 0).await;
    none(&mut brx, |e| is_text(e, "once")).await;
}

#[tokio::test]
async fn peers_without_acks_get_plain_messages() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let old = Identity::generate();
    let (x, mut y) = tokio::io::duplex(1 << 20);
    let old_id = old.public();
    // An old responder: no Hello of its own, never sends Tracked or Ack.
    let peer = tokio::spawn(async move {
        let mut chan = handshake::accept(&mut y, &old).await.unwrap();
        let mut got = Vec::new();
        loop {
            let f = read_frame(&mut y).await.unwrap().unwrap();
            let m = chan.open(&f).unwrap();
            if got.is_empty() {
                // First message from the initiator opens our sending chain.
                let reply = chan
                    .seal(&AppMessage::Approval { approved: false })
                    .unwrap();
                write_frame(&mut y, &reply).await.unwrap();
            }
            let done = matches!(&m, AppMessage::Text { body, .. } if body == "hello old friend");
            got.push(m);
            if done {
                return got;
            }
        }
    });
    alice
        .connect_stream(x, "link", "old".into(), Some(old_id.fingerprint()))
        .await
        .unwrap();
    alice.send(&old_id, text("hello old friend")).unwrap();
    let got = timeout(Duration::from_secs(10), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        got.iter()
            .all(|m| !matches!(m, AppMessage::Tracked { .. } | AppMessage::Ack(_))),
        "an old peer would drop the session on these: {got:?}"
    );
    wait_for(|| alice.unacked(&old_id) == 0).await;
}

#[tokio::test]
async fn unacknowledged_messages_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, _arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    let b = bob.identity();
    let (cut_a, _) = lossy_link(&alice, &bob).await;
    alice.send(&b, text("warm up")).unwrap();
    next(&mut brx, |e| is_text(e, "warm up")).await;
    cut_a.store(true, Ordering::SeqCst);
    alice.send(&b, text("before the crash")).unwrap();
    assert_eq!(alice.unacked(&b), 1);

    // Alice's process dies; a new one starts from the same home.
    alice.shutdown();
    drop(alice);
    let home = Home::new(dir.path().join("alice"));
    let identity = home.load_identity(None).unwrap();
    let (alice, _arx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    assert_eq!(alice.unacked(&b), 1);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    alice
        .connect(&addr.to_string(), Some(b.fingerprint()))
        .await
        .unwrap();
    next(&mut brx, |e| is_text(e, "before the crash")).await;
    wait_for(|| alice.unacked(&b) == 0).await;
}

#[tokio::test]
async fn history_marks_messages_delivered_when_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let (alice, mut arx) = node(&dir, "alice");
    let (bob, mut brx) = node(&dir, "bob");
    let b = bob.identity();
    let (cut_a, _) = lossy_link(&alice, &bob).await;
    let conv = alice.conversation_for(&b);
    let last = |a: &Node| a.history(conv).unwrap().entries().last().cloned().unwrap();

    alice.send_text(&b, "tick").unwrap();
    let e = next(&mut arx, |e| matches!(e, Event::Delivered { .. })).await;
    let entry = last(&alice);
    assert!(entry.delivered && entry.text == "tick");
    assert!(
        matches!(e, Event::Delivered { peer, local_id, group: None, relay_for: None } if peer == b && local_id == entry.local_id)
    );

    alice
        .send_file(&b, "a.txt", vec![1, 2, 3], Some("/tmp/a.txt".into()))
        .unwrap();
    next(&mut brx, |e| {
        matches!(
            e,
            Event::Message {
                msg: AppMessage::File { .. },
                ..
            }
        )
    })
    .await;
    next(&mut arx, |e| matches!(e, Event::Delivered { .. })).await;
    let entry = last(&alice);
    assert!(entry.delivered && entry.file.is_some_and(|f| f.size == 3));

    // Lost on a dead link: not delivered until the resend is acknowledged.
    cut_a.store(true, Ordering::SeqCst);
    alice.send_text(&b, "later").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!last(&alice).delivered);
    let addr = bob.listen("127.0.0.1:0").await.unwrap();
    alice
        .connect(&addr.to_string(), Some(b.fingerprint()))
        .await
        .unwrap();
    next(&mut arx, |e| matches!(e, Event::Delivered { .. })).await;
    assert!(last(&alice).delivered);
}
