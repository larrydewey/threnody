//! Minimal two-node probe for NAT reachability tests (Appendix N).
//!
//! Bootstrap mode (directly reachable peers):
//!   probe --home A --listen-port P               # prints addr + fingerprint
//!   probe --home B --dial A_ADDR                 # connects, approves; prints
//!      "mutual" once both sides approved; exits.
//!
//! Rendezvous mode (under NAT):
//!   probe --home A --rendezvous [--bootstrap h:p] [--loopback] \
//!         --listen-port P --seek FP_OR_NAME &
//!   prints "connected to FP via direct|relay <addr>" on success.

use std::time::Duration;

use threnody_core::store::{Contacts, Home, Lookup};
use threnody_core::PublicIdentity;
use threnody_net::reach::ReachConfig;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn find_contact<'a>(contacts: &'a Contacts, q: &str) -> Option<&'a threnody_core::store::Contact> {
    match contacts.find(q) {
        Lookup::Found(c) => Some(c),
        _ => None,
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let home_dir = arg(&args, "--home").unwrap_or_else(|| "probe-home".into());
    let home = Home::new(home_dir);
    let identity = match home.load_identity(None) {
        Ok(i) => i,
        Err(_) => home.create_identity(None).unwrap(),
    };
    // Serve a tiny DHT for the netns test: `probe --serve-dht PORT [BOOT]`.
    if has(&args, "--serve-dht") {
        let port: u16 = arg(&args, "--serve-dht").unwrap().parse()?;
        let mut b = mainline::Dht::builder();
        b.server_mode().port(port).no_bootstrap();
        if let Some(boot) = arg(&args, "--extra") {
            b.extra_bootstrap(&[boot]);
        }
        let _dht = b.build()?;
        println!("dht on :{port}");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }

    let fp = identity.public().fingerprint();
    let (node, mut rx) = Node::new(NodeConfig {
        home,
        identity,
        policy: AcceptPolicy::Anyone,
        constant_rate: None,
        tunnel_port: None,
    })
    .unwrap();
    println!("fingerprint {fp}");

    // Bootstrap: accept one approved contact, then exit.
    if let Some(addr) = arg(&args, "--accept") {
        let bound = node.listen(&format!("0.0.0.0:{addr}")).await?;
        println!("listening tcp {bound}");
        loop {
            match rx.recv().await {
                Some(Event::ApprovalChanged { mutual: true, peer, .. }) => {
                    println!("mutual {}", peer.fingerprint());
                    return Ok(());
                }
                Some(Event::ApprovalChanged { peer, .. }) => {
                    node.set_approval(&peer, true)?;
                }
                _ => {}
            }
        }
    }

    // Dial mode: connect and approve, then wait for mutual approval.
    if let Some(addr) = arg(&args, "--dial") {
        let peer = node.connect(&addr, None).await?;
        node.set_approval(&peer, true)?;
        while let Some(ev) = rx.recv().await {
            if let Event::ApprovalChanged { mutual: true, .. } = ev {
                println!("mutual");
                return Ok(());
            }
        }
        return Ok(());
    }

    node.set_reach(true);
    let q = match arg(&args, "--listen-port") {
        Some(p) => node.listen_quic(&format!("0.0.0.0:{p}")).await?,
        None => node.listen_quic("0.0.0.0:0").await?,
    };
    println!("quic {q}");
    let mut bs = Vec::new();
    if let Some(b) = arg(&args, "--bootstrap") {
        bs.extend(b.split(',').map(|s| s.to_owned()));
    }
    let cfg = ReachConfig {
        bootstrap: (!bs.is_empty()).then_some(bs),
        reflectors: vec![],
        loopback: has(&args, "--loopback"),
        local_candidates: !has(&args, "--no-local"),
        poll_foreground: Duration::from_secs(1),
        poll_seeking: Duration::from_secs(1),
        republish: Duration::from_secs(60),
        regather: Duration::from_secs(5),
        ..ReachConfig::default()
    };
    node.start_reach(cfg)?;
    if let Some(q) = arg(&args, "--seek") {
        let contacts = node.contacts();
        match find_contact(&contacts, &q) {
            Some(c) => {
                let peer: PublicIdentity = c.key;
                node.seek(&peer);
                println!("seeking {}", peer.fingerprint());
            }
            None => return Err(format!("unknown contact {q:?}").into()),
        }
    }
    loop {
        match rx.recv().await {
            Some(Event::Connected { peer, addr, via, .. }) => {
                let how = match via {
                    None => "direct",
                    Some(_) => "relay",
                };
                println!("connected to {} via {} {addr}", peer.fingerprint(), how);
                if has(&args, "--one") {
                    return Ok(());
                }
            }
            Some(Event::ReachNote { note }) => println!("note: {note}"),
            Some(Event::Addresses { candidates, symmetric }) => {
                println!("addresses: {candidates:?} symmetric {symmetric}");
            }
            _ => {}
        }
    }
}
