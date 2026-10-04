use std::time::Duration;

use threnody_core::store::Home;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

async fn spawn(dir: &tempfile::TempDir, name: &str) -> (Node, UnboundedReceiver<Event>, String) {
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
    (node, rx, addr)
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

#[tokio::test]
async fn history_records_both_sides_and_disappearing_messages_vanish() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _arx, _) = spawn(&dir, "a").await;
    let (b, mut brx, baddr) = spawn(&dir, "b").await;
    let bid = a.connect(&baddr, None).await.unwrap();
    let aid = a.identity();
    next(&mut brx, |e| matches!(e, Event::Connected { .. })).await;

    a.send_text(&bid, "stays").unwrap();
    next(&mut brx, |e| matches!(e, Event::Message { .. })).await;

    a.set_timer(&bid, Some(1)).unwrap();
    a.send_text(&bid, "vanishes").unwrap();
    let Event::TimerChanged { secs, .. } =
        next(&mut brx, |e| matches!(e, Event::TimerChanged { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(secs, Some(1), "receiver adopts the sender's timer");

    let texts = |n: &Node, p| {
        n.history(n.conversation_for(p))
            .unwrap()
            .entries()
            .iter()
            .map(|e| (e.outgoing, e.text.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(&a, &bid),
        [(true, "stays".into()), (true, "vanishes".into())]
    );
    timeout(Duration::from_secs(5), async {
        while texts(&b, &aid).len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        texts(&b, &aid),
        [(false, "stays".into()), (false, "vanishes".into())]
    );

    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert_eq!(texts(&a, &bid), [(true, "stays".into())]);
    assert_eq!(texts(&b, &aid), [(false, "stays".into())]);
    assert_eq!(
        b.history(b.conversation_for(&aid)).unwrap().timer_s,
        Some(1)
    );
}
