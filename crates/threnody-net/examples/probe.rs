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
    // Serve canned mainline testnet: probe --serve-testnet COUNT.
    // Prints "TESTNET <addr>" lines for each seed node (source addresses
    // rewritten to 198.51.100.1, the alias on the test's ethernetns).
    if has(&args, "--serve-testnet") {
        let count: usize = arg(&args, "--serve-testnet").unwrap().parse()?;
        let t = mainline::Testnet::builder(count)
            .bind_address("198.51.100.1".parse()?)
            .build()?;
        for b in &t.bootstrap {
            println!("TESTNET {b}");
        }
        std::mem::forget(t);
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }

    // Serve a tiny DHT for the netns test: `probe --serve-dht PORT [BOOT]`.
    if has(&args, "--serve-dht") {
        let port: u16 = arg(&args, "--serve-dht").unwrap().parse()?;
        let mut b = mainline::Dht::builder();
        b.server_mode().port(port).no_bootstrap();
        if let Some(boot) = arg(&args, "--extra") {
            b.extra_bootstrap(&[boot]);
        }
        if let Some(pip) = arg(&args, "--public-ip") {
            b.public_ip(pip.parse()?);
        }
        let _dht = b.build()?;
        println!("dht on :{port}");
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    }

    // Answer BEP42-style KRPC pings: report the querier's source address
    // back (like libtorrent DHT nodes), so `gather` learns our NATed
    // address even against a mainline testnet that omits ping replies.
    if has(&args, "--serve-reflector") {
        let spec = arg(&args, "--serve-reflector").unwrap();
        // Bind the exact address: replies then leave from that address.
        let bind: std::net::SocketAddr = if spec.contains(':') {
            spec.parse()?
        } else {
            format!("0.0.0.0:{spec}").parse()?
        };
        let sock = tokio::net::UdpSocket::bind(bind).await?;
        println!("reflector on {bind}");
        loop {
            let mut buf = [0u8; 2048];
            let (n, src) = sock.recv_from(&mut buf).await?;
            let data = &buf[..n];
            eprintln!("reflector: {n} bytes from {src}");
            if data
                .windows(b"q4:ping".len())
                .any(|w| w == b"q4:ping")
            {
                let Some(i) = data.windows(b"1:t2:".len()).position(|w| w == b"1:t2:") else {
                    continue;
                };
                let tid = &data[i + 5..i + 7];
                let std::net::IpAddr::V4(a) = src.ip() else {
                    continue;
                };
                let mut r = b"d2:ip6:".to_vec();
                r.extend_from_slice(&a.octets());
                r.extend_from_slice(&src.port().to_be_bytes());
                r.extend_from_slice(b"1:rd2:id20:");
                r.extend_from_slice(&threnody_core::crypto::random_bytes::<20>());
                r.extend_from_slice(b"1:t2:");
                r.extend_from_slice(tid);
                r.extend_from_slice(b"1:y1:re");
                sock.send_to(&r, src).await?;
            }
        }
    }

    let home_dir = arg(&args, "--home").unwrap_or_else(|| "probe-home".into());
    let home = Home::new(home_dir);
    let identity = match home.load_identity(None) {
        Ok(i) => i,
        Err(_) => home.create_identity(None).unwrap(),
    };
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
    let mut refls = Vec::new();
    if let Some(r) = arg(&args, "--reflect") {
        refls.extend(r.split(',').map(|s| s.to_owned()));
    }
    let cfg = ReachConfig {
        bootstrap: (!bs.is_empty()).then_some(bs),
        reflectors: refls,
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
    let watcher = node.clone();
    tokio::spawn(async move {
        loop {
            let r = watcher.reachability();
            println!("reach: online {} candidates {:?} symmetric {}", r.online, r.candidates, r.symmetric);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
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
