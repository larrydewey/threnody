//! Interactive session: one node, many peers, line-oriented UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::history::FileNote;
use threnody_core::store::Home;
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, safety_number};
use threnody_net::mailbox::DepositStatus;
use threnody_net::{AcceptPolicy, DiscoveryConfig, Event, Node, NodeConfig};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::groups::GroupUi;
use crate::tunnel::Tunnels;
use crate::{describe_contact, find_contact, target};

pub struct Options {
    pub home: Home,
    pub identity: Identity,
    pub listen: Option<String>,
    pub connect: Vec<String>,
    pub policy: AcceptPolicy,
    pub constant_rate: Option<Duration>,
    pub tunnel: Option<TunnelOptions>,
    /// UDP port for LAN discovery; `None` disables it.
    pub discover: Option<u16>,
    /// Advertise and accept sessions over Bluetooth LE.
    pub ble: bool,
    /// Join Wi-Fi Direct groups that approved contacts offer.
    pub wifi_direct: bool,
}

pub struct TunnelOptions {
    pub port: u16,
    pub iface: String,
    pub apply: bool,
}

const HELP: &str = "\
Type a line to send it to the current peer. Commands:
  /connect <invite|contact|host:port>   dial a peer (contacts fall back to relays)
  /relay <peer>                         reach a contact through approved relays
  /onion <peer> [min-relays]            reach a peer so no single relay sees both ends
  /to <peer>                            choose who plain lines go to
  /peers                                live sessions
  /contacts                             contact book
  /name <peer> <name>                   set a local name
  /approve [peer]   /revoke [peer]      mesh / tunnel approval
  /safety [peer]                        show the safety number
  /verify [peer]                        mark safety number as confirmed
  /file <path>                          send a file to the current peer
  /drop [peer]                          close a session
  /policy anyone|contacts|approved      who may connect to us
  /status                               transports and protection level
  /devices   /device add   /device remove <name>   your account's devices
  /ble scan [secs]   /ble connect <n|address>   Bluetooth LE (Linux)
  /wifi-direct [request|leave]          ask the current peer for a Wi-Fi Direct link
  /history [peer] [n]                   recent messages (stored encrypted)
  /disappear <30s|10m|1h|1d|off>        disappearing messages with the current peer
  /quit
Groups (MLS, post-quantum X-Wing ciphersuite):";

struct Ui {
    node: Node,
    downloads: PathBuf,
    listen_addr: Option<std::net::SocketAddr>,
    current: Option<PublicIdentity>,
    tunnels: Option<Tunnels>,
    discovery: Option<std::net::SocketAddr>,
    groups: GroupUi,
    #[cfg(all(feature = "ble", target_os = "linux"))]
    ble_seen: std::sync::Arc<std::sync::Mutex<Vec<crate::ble::Found>>>,
    wifi_direct: bool,
    /// NetworkManager profiles of Wi-Fi Direct groups we joined.
    joined: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    scratch: PathBuf,
}

pub async fn run(opts: Options) -> Result<()> {
    let downloads = opts.home.dir().join("downloads");
    let opts_dir = opts.home.dir().to_path_buf();
    let groups = GroupUi::load(&opts.home, &opts.identity).context("loading groups")?;
    let tunnels = opts
        .tunnel
        .as_ref()
        .map(|t| {
            Tunnels::new(
                &opts.identity,
                opts.home.dir(),
                t.iface.clone(),
                t.port,
                t.apply,
            )
        })
        .transpose()?;
    let (node, mut events) = Node::new(NodeConfig {
        home: opts.home,
        identity: opts.identity,
        policy: opts.policy,
        constant_rate: opts.constant_rate,
        tunnel_port: opts.tunnel.as_ref().map(|t| t.port),
    })?;
    println!("Threnody — you are {}", node.identity().fingerprint());
    if groups.count() > 0 {
        println!("{} group(s) restored. /groups to list", groups.count());
    }
    if let Some(t) = &tunnels {
        println!("Tunnels on: overlay address {}", t.overlay());
        println!("  WireGuard config: {}", t.conf_path().display());
        if opts.tunnel.as_ref().is_some_and(|o| !o.apply) {
            println!(
                "  Bring it up with: sudo wg-quick up {}",
                t.conf_path().display()
            );
        }
    }

    let listen_addr = match &opts.listen {
        Some(a) => {
            let bound = node
                .listen(a)
                .await
                .with_context(|| format!("listening on {a}"))?;
            println!(
                "Listening on {bound} (tcp). Share: threnody invite <your-ip>:{}",
                bound.port()
            );
            Some(bound)
        }
        None => None,
    };
    let mut ui = Ui {
        node,
        downloads,
        listen_addr,
        current: None,
        tunnels,
        discovery: None,
        groups,
        #[cfg(all(feature = "ble", target_os = "linux"))]
        ble_seen: std::sync::Arc::default(),
        wifi_direct: opts.wifi_direct,
        joined: std::sync::Arc::default(),
        scratch: opts_dir,
    };
    if let (Some(port), Some(tcp)) = (opts.discover, listen_addr) {
        let cfg = DiscoveryConfig {
            bind: std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port)),
            targets: vec![std::net::SocketAddr::from((
                threnody_net::discovery::DEFAULT_GROUP,
                port,
            ))],
            ..DiscoveryConfig::default()
        };
        match ui.node.start_discovery(cfg, tcp.port()) {
            Ok(a) => {
                ui.discovery = Some(a);
                println!(
                    "LAN discovery on udp/{port}: approved peers nearby connect automatically."
                );
            }
            Err(e) => println!("! LAN discovery unavailable: {e}"),
        }
    }
    for t in &opts.connect {
        ui.connect(t);
    }
    #[cfg(all(feature = "ble", target_os = "linux"))]
    let _ble = if opts.ble {
        match crate::ble::listen(&ui.node).await {
            Ok(l) => {
                println!(
                    "Bluetooth LE: advertising, L2CAP psm {}; approved contacts nearby connect automatically",
                    l.psm
                );
                Some(l)
            }
            Err(e) => {
                println!("! Bluetooth LE unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(all(feature = "ble", target_os = "linux")))]
    if opts.ble {
        println!("! this build has no Bluetooth LE support");
    }
    println!("Type /help for commands.");

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                match ui.handle_line(line.trim()).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(e) => println!("! {e:#}"),
                }
            }
            Some(ev) = events.recv() => ui.handle_event(ev),
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    ui.leave_wifi_direct().await;
    Ok(())
}

fn name_of(node: &Node, p: &PublicIdentity) -> String {
    node.contacts()
        .get(p)
        .map_or_else(|| p.fingerprint().to_string(), |c| c.label())
}

impl Ui {
    fn name(&self, p: &PublicIdentity) -> String {
        name_of(&self.node, p)
    }

    fn resolve_peer(&self, arg: Option<&str>) -> Result<PublicIdentity> {
        match arg {
            Some(q) => Ok(find_contact(&self.node.contacts(), q)?.key),
            None => self
                .current
                .ok_or_else(|| anyhow!("no current peer; use /to <peer>")),
        }
    }

    /// Dials directly; for known contacts, falls back to a relay circuit
    /// when the direct path fails or no address is known (spec §7.3).
    fn connect(&self, t: &str) {
        let contacts = self.node.contacts();
        let known = match contacts.find(t) {
            threnody_core::store::Lookup::Found(c) => Some(c.key),
            _ => None,
        };
        let resolved = target::resolve(t, &contacts);
        let node = self.node.clone();
        let t = t.to_owned();
        tokio::spawn(async move {
            let (direct, pin) = match resolved {
                Ok((addr, pin)) => (
                    node.connect(&addr, pin).await.map_err(anyhow::Error::from),
                    pin,
                ),
                Err(e) => (Err(e), None),
            };
            let Err(e) = direct else { return };
            // An invite link names the peer, so it can be reached through
            // relays even before it is a contact.
            let relay_to = known.map(|k| k.fingerprint()).or(pin);
            match relay_to {
                Some(fp) => {
                    println!("* {t}: direct path failed ({e:#}); trying relays");
                    if let Err(e) = node.connect_relayed(fp).await {
                        println!("! connect {t}: {e:#}");
                    }
                }
                None => println!("! connect {t}: {e:#}"),
            }
        });
    }

    /// Shows a message from `peer`; `via` marks mailbox (offline) delivery.
    fn show_message(&mut self, peer: PublicIdentity, msg: AppMessage, via: Option<PublicIdentity>) {
        let who = match via {
            Some(v) => format!("{} (offline, via {})", self.name(&peer), self.name(&v)),
            None => self.name(&peer),
        };
        match msg {
            AppMessage::Text { body, .. } => println!("<{who}> {body}"),
            AppMessage::File { name, data, .. } => {
                let saved = save_download(&self.downloads, &name, &data);
                match &saved {
                    Ok(p) => println!(
                        "* {who} sent {name} ({} bytes) -> {}",
                        data.len(),
                        p.display()
                    ),
                    Err(e) => println!("! could not save file from {who}: {e:#}"),
                }
                self.node.record_file(
                    &peer,
                    false,
                    FileNote {
                        name,
                        size: data.len() as u64,
                        location: saved.ok().map(|p| p.display().to_string()),
                    },
                );
            }
            AppMessage::Group(payload) => {
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                self.groups.incoming(&self.node, &name, peer, &payload);
            }
            _ => {}
        }
    }

    /// Sends text to every device of `peer`'s account (Appendix J): live
    /// where connected, sealed for mailboxes otherwise; recorded in history.
    fn send_to_account(&self, peer: PublicIdentity, body: &str) -> Result<()> {
        let r = self.node.send_text(&peer, body)?;
        if r.live + r.sealed + r.unreachable > 1 || r.sealed > 0 {
            println!(
                "* to {} device(s): {} live, {} sealed for mailboxes{}",
                r.live + r.sealed + r.unreachable,
                r.live,
                r.sealed,
                if r.unreachable == 0 {
                    String::new()
                } else {
                    format!(", {} unreachable", r.unreachable)
                }
            );
        }
        Ok(())
    }

    fn show_history(&self, peer: PublicIdentity, n: usize) -> Result<()> {
        let conv = self.node.conversation_for(&peer);
        let h = self.node.history(conv)?;
        if let Some(t) = h.timer_s {
            println!("  (messages disappear after {})", human_secs(t));
        }
        if h.entries().is_empty() {
            println!("  no history with {}", self.name(&peer));
        }
        for e in h.recent(n) {
            let who = if e.outgoing {
                "me".to_owned()
            } else {
                PublicIdentity::from_bytes(&e.device).map_or_else(|_| "?".into(), |d| self.name(&d))
            };
            let mut mark = match (e.offline, e.expires_at_ms) {
                (_, Some(_)) => " (disappearing)",
                (true, None) => " (offline)",
                _ => "",
            }
            .to_owned();
            if e.delivered {
                mark.push_str(" ✓✓");
            }
            match &e.file {
                Some(f) => println!(
                    "  [{}] <{who}> file {} ({} bytes){}{mark}",
                    clock(e.at_ms),
                    f.name,
                    f.size,
                    f.location
                        .as_ref()
                        .map_or_else(String::new, |l| format!(" at {l}"))
                ),
                None => println!("  [{}] <{who}> {}{mark}", clock(e.at_ms), e.text),
            }
        }
        Ok(())
    }

    /// A contact, a bare fingerprint, or an invite link's fingerprint.
    fn parse_destination(&self, q: &str) -> Result<Fingerprint> {
        match self.resolve_peer(Some(q)) {
            Ok(p) => Ok(p.fingerprint()),
            Err(_) => q
                .trim_start_matches("threnody://")
                .split('@')
                .next()
                .unwrap_or_default()
                .parse::<Fingerprint>()
                .map_err(|_| anyhow!("{q:?} is not a contact, fingerprint or invite link")),
        }
    }

    fn relay(&self, dest: Fingerprint) {
        let node = self.node.clone();
        let who = self
            .node
            .contacts()
            .iter()
            .find(|c| c.fingerprint() == dest)
            .map_or_else(|| dest.to_string(), |c| c.label());
        tokio::spawn(async move {
            if let Err(e) = node.connect_relayed(dest).await {
                println!("! relay to {who}: {e:#}");
            }
        });
    }

    /// Joins an offered Wi-Fi Direct group and opens a session over it,
    /// pinning the peer; the new session replaces the slower one.
    fn wifi_direct_offer(&self, peer: PublicIdentity, offer: threnody_net::DirectOffer) {
        let who = self.name(&peer);
        if !self.wifi_direct {
            println!(
                "* {who} offers a Wi-Fi Direct link ({}); run with --wifi-direct to join",
                offer.ssid
            );
            return;
        }
        println!(
            "* {who} offers a Wi-Fi Direct link ({}); joining…",
            offer.ssid
        );
        let (node, joined, scratch) = (
            self.node.clone(),
            std::sync::Arc::clone(&self.joined),
            self.scratch.clone(),
        );
        tokio::spawn(async move {
            match crate::wifidirect::join(&offer, &scratch).await {
                Ok(name) => {
                    joined
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(name);
                }
                Err(e) => {
                    println!("! Wi-Fi Direct join: {e:#}");
                    return;
                }
            }
            if let Err(e) = node.connect(&offer.addr, Some(peer.fingerprint())).await {
                println!("! Wi-Fi Direct connect to {}: {e:#}", offer.addr);
            }
        });
    }

    async fn leave_wifi_direct(&self) {
        let names = std::mem::take(
            &mut *self
                .joined
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for n in names {
            match crate::wifidirect::leave(&n).await {
                Ok(()) => println!("* left Wi-Fi Direct group ({n})"),
                Err(e) => println!("! leaving {n}: {e:#}"),
            }
        }
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Connected {
                peer,
                addr,
                suite,
                new_contact,
                via,
            } => {
                let who = self.name(&peer);
                match via {
                    Some(v) => {
                        let info = self.node.sessions().into_iter().find(|s| s.peer == peer);
                        let how = match info {
                            Some(i) if i.transport == "onion" && i.outbound => {
                                "onion circuit, first hop"
                            }
                            Some(i) if i.transport == "onion" => "onion circuit, last relay",
                            _ => "relay",
                        };
                        println!(
                            "* connected to {who} through {how} {} [{}, end-to-end]",
                            self.name(&v),
                            suite.name()
                        );
                    }
                    None => {
                        let (t, r) = self
                            .node
                            .sessions()
                            .into_iter()
                            .find(|s| s.peer == peer)
                            .map_or(("tcp", addr.to_string()), |s| (s.transport, s.remote));
                        println!("* connected to {who} at {r} [{t}, {}]", suite.name());
                    }
                }
                if new_contact {
                    println!("  new contact (trust on first use). Compare safety numbers: /safety");
                }
                if self.current.is_none() {
                    self.current = Some(peer);
                    println!("  messages now go to {who}");
                }
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                self.groups.connected(&self.node, &name, &peer);
            }
            Event::Message { peer, msg } => self.show_message(peer, msg, None),
            Event::OfflineMessage { from, via, msg } => self.show_message(from, msg, Some(via)),
            // Shown as ✓✓ in /history; too chatty to print live.
            Event::Delivered { .. } => {}
            Event::DepositReceipt {
                mailbox,
                to,
                status,
            } => {
                let (m, t) = (self.name(&mailbox), self.name(&to));
                match status {
                    DepositStatus::Held => println!("* {m} is holding your message for {t}"),
                    DepositStatus::Delivered => println!("* {m} delivered your message to {t}"),
                    DepositStatus::Declined => println!(
                        "! {m} declined to hold your message for {t} (it needs mutual approval with both of you)"
                    ),
                }
            }
            Event::ApprovalChanged {
                peer,
                remote_approved,
                mutual,
            } => {
                let who = self.name(&peer);
                let state = if remote_approved {
                    "approved you"
                } else {
                    "has not approved you"
                };
                println!(
                    "* {who} {state}{}",
                    if mutual { " — mutually approved" } else { "" }
                );
            }
            Event::WifiDirectOffer { peer, offer } => self.wifi_direct_offer(peer, offer),
            Event::WifiDirectRequested { peer } => println!(
                "* {} asked for a Wi-Fi Direct link; this device can only join groups, not host them",
                self.name(&peer)
            ),
            Event::Disconnected { peer, reason } => {
                // Keep `current`: plain lines then go out as sealed messages.
                println!("* {} disconnected ({reason})", self.name(&peer));
            }
            Event::TunnelUp {
                peer,
                wg_public,
                endpoint,
                overlay,
                psk,
            } => {
                let who = self.name(&peer);
                if let Some(t) = self.tunnels.as_mut() {
                    match t.up(&peer, wg_public, endpoint, psk) {
                        Ok(()) => println!(
                            "* tunnel to {who}: {overlay} via {endpoint} (PSK from this session)"
                        ),
                        Err(e) => println!("! tunnel to {who}: {e:#}"),
                    }
                }
            }
            Event::TunnelDown { peer, wg_public } => {
                let who = self.name(&peer);
                if let Some(t) = self.tunnels.as_mut() {
                    match t.down(&peer, wg_public) {
                        Ok(true) => println!("* tunnel to {who} removed"),
                        Ok(false) => {}
                        Err(e) => println!("! removing tunnel to {who}: {e:#}"),
                    }
                }
            }
            Event::Discovered {
                peer,
                addr,
                connected,
            } => {
                if !connected {
                    println!("* found {} nearby at {addr}", self.name(&peer));
                }
            }
            Event::DialFailed { peer, addr, reason } => {
                println!("! could not reach {} at {addr}: {reason}", self.name(&peer));
            }
            Event::AccountChanged {
                account,
                added,
                removed,
            } => {
                let a = account.fingerprint().to_string();
                for d in added {
                    println!("* account {} added device {}", &a[..9], self.name(&d));
                }
                for d in removed {
                    println!(
                        "! account {} REMOVED device {} — it is no longer trusted",
                        &a[..9],
                        self.name(&d)
                    );
                }
            }
            Event::AccountFork { device, account } => println!(
                "! WARNING: {} presented a conflicting history for account {} — possible compromise; ignored",
                self.name(&device),
                account.fingerprint()
            ),
            Event::DeviceLinked { device } => println!(
                "* linked new device {} into your account",
                self.name(&device)
            ),
            Event::LinkRejected { device } => {
                println!("! rejected a link attempt from {}", self.name(&device))
            }
            Event::ContactsSynced { from } => {
                println!("* contacts updated from {}", self.name(&from))
            }
            Event::TimerChanged { peer, secs } => match secs {
                Some(t) => println!(
                    "* {} set messages to disappear after {}",
                    self.name(&peer),
                    human_secs(t)
                ),
                None => println!("* {} turned disappearing messages off", self.name(&peer)),
            },
            Event::ThisDeviceRemoved => {
                println!("! THIS DEVICE WAS REMOVED from its account; peers will refuse it")
            }
            Event::Rejected { addr, reason } => {
                println!("* rejected connection from {addr}: {reason}")
            }
        }
    }

    /// Returns `Ok(true)` to quit.
    async fn handle_line(&mut self, line: &str) -> Result<bool> {
        if line.is_empty() {
            return Ok(false);
        }
        let Some(cmd) = line.strip_prefix('/') else {
            let peer = self
                .current
                .ok_or_else(|| anyhow!("no current peer; /connect or /to first"))?;
            self.send_to_account(peer, line)?;
            return Ok(false);
        };
        let mut parts = cmd.splitn(2, ' ');
        let verb = parts.next().unwrap_or_default();
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        match verb {
            "quit" | "q" | "exit" => return Ok(true),
            "help" | "h" | "?" => println!("{HELP}\n{}", crate::groups::HELP),
            "connect" | "c" => {
                self.connect(arg.ok_or_else(|| anyhow!("usage: /connect <target>"))?)
            }
            "to" => {
                let p =
                    self.resolve_peer(Some(arg.ok_or_else(|| anyhow!("usage: /to <peer>"))?))?;
                self.current = Some(p);
                println!("* messages now go to {}", self.name(&p));
            }
            "peers" => {
                let s = self.node.sessions();
                if s.is_empty() {
                    println!("No live sessions.");
                }
                for i in s {
                    let dir = if i.outbound { "out" } else { "in" };
                    let path = match i.via {
                        Some(v) if i.transport == "onion" => {
                            let end = if i.outbound {
                                "first hop"
                            } else {
                                "last relay"
                            };
                            format!("onion circuit, {end} {}", self.name(&v))
                        }
                        Some(v) => format!("relay through {}", self.name(&v)),
                        None => format!("{} {}", i.transport, i.remote),
                    };
                    println!(
                        "  {} via {path} ({dir}, {})",
                        self.name(&i.peer),
                        i.suite.name()
                    );
                }
            }
            "contacts" => {
                for c in self.node.contacts().iter() {
                    println!("  {}", describe_contact(c));
                }
            }
            "name" => {
                let (who, name) = arg
                    .and_then(|a| a.rsplit_once(' '))
                    .ok_or_else(|| anyhow!("usage: /name <peer> <name>"))?;
                let key = self.resolve_peer(Some(who))?;
                self.node.update_contacts(|c| {
                    if let Some(c) = c.get_mut(&key) {
                        c.petname = Some(name.to_owned());
                    }
                });
                println!("* named {}", self.name(&key));
            }
            "approve" | "revoke" => {
                let p = self.resolve_peer(arg)?;
                self.node.set_approval(&p, verb == "approve")?;
                println!(
                    "* {} {}",
                    if verb == "approve" {
                        "approved"
                    } else {
                        "revoked"
                    },
                    self.name(&p)
                );
            }
            "safety" => {
                let p = self.resolve_peer(arg)?;
                println!(
                    "  {}\n  Compare with {} in person or by voice, then /verify.",
                    safety_number(&self.node.identity(), &p),
                    self.name(&p)
                );
            }
            "verify" => {
                let p = self.resolve_peer(arg)?;
                self.node.update_contacts(|c| {
                    if let Some(c) = c.get_mut(&p) {
                        c.verified = true;
                    }
                });
                println!("* marked {} verified", self.name(&p));
            }
            "file" => {
                let path = arg.ok_or_else(|| anyhow!("usage: /file <path>"))?;
                let peer = self.current.ok_or_else(|| anyhow!("no current peer"))?;
                let data = tokio::fs::read(path)
                    .await
                    .with_context(|| format!("reading {path}"))?;
                if data.len() > threnody_core::message::MAX_FILE {
                    bail!(
                        "file larger than {} bytes",
                        threnody_core::message::MAX_FILE
                    );
                }
                let name = Path::new(path)
                    .file_name()
                    .map_or("file".into(), |n| n.to_string_lossy().into_owned());
                let len = data.len();
                let location = std::fs::canonicalize(path)
                    .ok()
                    .map(|p| p.display().to_string());
                self.node.send_file(&peer, &name, data, location)?;
                println!("* sent {len} bytes to {}", self.name(&peer));
            }
            "drop" => {
                let p = self.resolve_peer(arg)?;
                self.node.disconnect(&p);
            }
            "policy" => {
                let p = match arg {
                    Some("anyone") => AcceptPolicy::Anyone,
                    Some("contacts") => AcceptPolicy::ContactsOnly,
                    Some("approved") => AcceptPolicy::ApprovedOnly,
                    _ => bail!("usage: /policy anyone|contacts|approved"),
                };
                self.node.set_policy(p);
                println!("* policy: {p:?}");
            }
            "wifi-direct" | "wd" => match arg.unwrap_or("request") {
                "request" => {
                    let peer = self.current.ok_or_else(|| anyhow!("no current peer"))?;
                    if !self.wifi_direct {
                        println!("  note: run with --wifi-direct to join the group offered back");
                    }
                    self.node.request_wifi_direct(&peer)?;
                    println!("* asked {} for a Wi-Fi Direct link", self.name(&peer));
                }
                "leave" => self.leave_wifi_direct().await,
                _ => bail!("usage: /wifi-direct [request|leave]"),
            },
            "status" => self.status(),
            "devices" => self.devices(),
            #[cfg(all(feature = "ble", target_os = "linux"))]
            "ble" => self.ble(arg.unwrap_or("scan")),
            "history" | "hist" => {
                let mut it = arg.unwrap_or("").split_whitespace();
                let (who, n) = match (it.next(), it.next()) {
                    (Some(w), Some(n)) => (Some(w), n.parse().unwrap_or(20)),
                    (Some(w), None) if w.parse::<usize>().is_ok() => {
                        (None, w.parse().unwrap_or(20))
                    }
                    (w, _) => (w, 20),
                };
                let p = self.resolve_peer(who)?;
                self.show_history(p, n)?;
            }
            "disappear" => {
                let p = self.resolve_peer(None)?;
                let secs = match arg {
                    None | Some("off") => None,
                    Some(a) => Some(
                        parse_duration(a)
                            .ok_or_else(|| anyhow!("usage: /disappear <30s|10m|1h|1d|off>"))?,
                    ),
                };
                self.node.set_timer(&p, secs)?;
                match secs {
                    Some(t) => println!(
                        "* new messages with {} disappear after {}",
                        self.name(&p),
                        human_secs(t)
                    ),
                    None => println!("* disappearing messages off for {}", self.name(&p)),
                }
            }
            "device" => {
                let a = arg.unwrap_or("");
                match a.split_once(' ').map_or((a, ""), |(x, y)| (x, y.trim())) {
                    ("add", _) => {
                        let addr = self.listen_addr.ok_or_else(|| {
                            anyhow!("run with --listen so the new device can reach this one")
                        })?;
                        let code = self.node.create_link_code(addr.to_string());
                        println!("  On the new device run:\n\n    threnody link '{code}'\n");
                        if let Ok(qr) = qrcode::QrCode::new(code.to_string().as_bytes()) {
                            let art = qr
                                .render::<qrcode::render::unicode::Dense1x2>()
                                .dark_color(qrcode::render::unicode::Dense1x2::Light)
                                .light_color(qrcode::render::unicode::Dense1x2::Dark)
                                .build();
                            println!("{art}");
                        }
                        println!(
                            "  The code works once, for 10 minutes. Never share it with anyone else."
                        );
                    }
                    ("remove", who) if !who.is_empty() => {
                        let own = self.node.account();
                        let target = own
                            .state()
                            .devices
                            .iter()
                            .find(|(d, n)| {
                                n == who
                                    || threnody_core::identity::fingerprint_matches_prefix(
                                        &d.fingerprint(),
                                        who,
                                    )
                            })
                            .map(|(d, _)| *d)
                            .ok_or_else(|| {
                                anyhow!("no device {who:?} in your account; see /devices")
                            })?;
                        self.node.remove_device(&target)?;
                        println!("* removed {} from your account", target.fingerprint());
                    }
                    _ => bail!("usage: /device add | /device remove <name|fingerprint>"),
                }
            }
            "onion" => {
                let a = arg.ok_or_else(|| {
                    anyhow!("usage: /onion <contact|fingerprint|invite> [min-relays]")
                })?;
                let (q, min) = match a.rsplit_once(' ') {
                    Some((q, n)) if n.parse::<usize>().is_ok() => (q, n.parse::<usize>()?),
                    _ => (a, 2),
                };
                let dest = self.parse_destination(q)?;
                println!("* building an onion circuit to {dest} ({min}+ relays)");
                if min < 2 {
                    println!("  note: with one relay, that relay sees both ends");
                }
                let node = self.node.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.connect_onion(dest, min).await {
                        println!("! onion to {dest}: {e:#}");
                    }
                });
            }
            "relay" => {
                let q = arg.ok_or_else(|| anyhow!("usage: /relay <contact|fingerprint|invite>"))?;
                let dest = self.parse_destination(q)?;
                println!("* looking for a relay path to {dest}");
                self.relay(dest);
            }
            "group" => {
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                let resolve = |q: &str| Ok(find_contact(&node.contacts(), q)?.key);
                self.groups
                    .command(&self.node, &name, &resolve, arg.unwrap_or("list"))?;
            }
            "groups" => {
                let node = self.node.clone();
                self.groups.list(&|p: &PublicIdentity| name_of(&node, p));
            }
            "g" => {
                self.groups.say(
                    &self.node,
                    arg.ok_or_else(|| anyhow!("usage: /g <group> <text>"))?,
                )?;
            }
            other => bail!("unknown command /{other}; try /help"),
        }
        Ok(false)
    }

    #[cfg(all(feature = "ble", target_os = "linux"))]
    fn ble(&self, args: &str) {
        let mut it = args.split_whitespace();
        let seen = std::sync::Arc::clone(&self.ble_seen);
        match (it.next(), it.next()) {
            (Some("scan") | None, secs) => {
                let secs = secs.and_then(|s| s.parse().ok()).unwrap_or(8);
                println!("* scanning Bluetooth LE for {secs}s…");
                tokio::spawn(async move {
                    match crate::ble::scan(secs).await {
                        Ok(found) => {
                            if found.is_empty() {
                                println!("* no Threnody devices nearby");
                            }
                            for (i, f) in found.iter().enumerate() {
                                println!(
                                    "  {}: {} {} psm {} rssi {}",
                                    i + 1,
                                    f.addr,
                                    f.name.as_deref().unwrap_or("-"),
                                    f.psm,
                                    f.rssi.map_or("?".into(), |r| r.to_string())
                                );
                            }
                            if !found.is_empty() {
                                println!("  /ble connect <n> to open a session");
                            }
                            *seen
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = found;
                        }
                        Err(e) => println!("! Bluetooth scan: {e:#}"),
                    }
                });
            }
            (Some("connect"), Some(which)) => {
                let list = seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let f = which
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| list.get(n.wrapping_sub(1)).cloned())
                    .or_else(|| {
                        list.iter()
                            .find(|f| f.addr.to_string().eq_ignore_ascii_case(which))
                            .cloned()
                    });
                let Some(f) = f else {
                    println!("! no such device; /ble scan first");
                    return;
                };
                let node = self.node.clone();
                println!(
                    "* connecting to {} over Bluetooth LE (psm {})…",
                    f.addr, f.psm
                );
                tokio::spawn(async move {
                    if let Err(e) = crate::ble::connect(&node, &f, None).await {
                        println!("! Bluetooth connect: {e:#}");
                    }
                });
            }
            _ => println!("! usage: /ble scan [secs] | /ble connect <n|address>"),
        }
    }

    fn devices(&self) {
        let account = self.node.account();
        let me = self.node.identity();
        println!("  account {}", account.id().fingerprint());
        for (d, name) in &account.state().devices {
            let tag = if *d == me { " (this device)" } else { "" };
            let live = if self.node.sessions().iter().any(|s| s.peer == *d) {
                ", connected"
            } else {
                ""
            };
            println!("    {name:<16} {}{tag}{live}", d.fingerprint());
        }
        for d in &account.state().removed {
            println!("    removed          {}", d.fingerprint());
        }
    }

    /// Spec §10: persistent indicators of transport, tunnel and protection.
    fn status(&self) {
        let sessions = self.node.sessions();
        let account = self.node.account();
        println!(
            "  account      {} ({} device(s))",
            account.id().fingerprint(),
            account.state().devices.len()
        );
        println!("  this device  {}", self.node.identity().fingerprint());
        match self.listen_addr {
            Some(a) => println!("  listening    tcp {a}"),
            None => println!("  listening    no"),
        }
        println!("  policy       {:?}", self.node.policy());
        println!(
            "  transports   {}",
            if sessions.is_empty() {
                "none active".into()
            } else {
                format!("tcp ({} session(s))", sessions.len())
            }
        );
        let held = self.node.held_messages();
        println!("  mailbox      holding {held} sealed message(s) for others");
        match self.discovery {
            Some(a) => println!(
                "  discovery    lan beacons on udp/{} (approved peers only)",
                a.port()
            ),
            None => println!("  discovery    off"),
        }
        match &self.tunnels {
            Some(t) => println!("  tunnel       {}", t.summary()),
            None => println!("  tunnel       off (run with --tunnel)"),
        }
        let rate = match self.node.constant_rate() {
            Some(d) => format!("constant-rate {} ms with cover traffic", d.as_millis()),
            None => "off".into(),
        };
        println!(
            "  metadata     length padding: on; timing protection: {rate}; onion: /onion (carrying {} circuit(s) for others)",
            self.node.onion_hops()
        );
    }
}

/// Saves a received file under `dir` using only its final path component,
/// never overwriting an existing file.
fn save_download(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let base: String = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '/' | '\\' | ':'))
        .collect();
    let base = match base.trim_start_matches('.') {
        "" => "file".to_owned(),
        b => b.to_owned(),
    };
    for i in 0..1000 {
        let candidate = if i == 0 {
            dir.join(&base)
        } else {
            dir.join(format!("{i}-{base}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                use std::io::Write;
                f.write_all(data)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many files named {base}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_cannot_escape_or_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let p = save_download(dir.path(), "../../etc/passwd", b"x").unwrap();
        assert_eq!(p, dir.path().join("passwd"));
        let p2 = save_download(dir.path(), "passwd", b"y").unwrap();
        assert_eq!(p2, dir.path().join("1-passwd"));
        assert_eq!(
            save_download(dir.path(), "..", b"z").unwrap(),
            dir.path().join("file")
        );
        assert_eq!(
            save_download(dir.path(), ".bashrc", b"z").unwrap(),
            dir.path().join("bashrc")
        );
    }
}

/// `30s`, `10m`, `2h`, `1d` or plain seconds.
fn parse_duration(a: &str) -> Option<u32> {
    let a = a.trim();
    let (num, mult) = match a.chars().last()? {
        's' => (&a[..a.len() - 1], 1),
        'm' => (&a[..a.len() - 1], 60),
        'h' => (&a[..a.len() - 1], 3600),
        'd' => (&a[..a.len() - 1], 86_400),
        c if c.is_ascii_digit() => (a, 1),
        _ => return None,
    };
    num.parse::<u32>()
        .ok()?
        .checked_mul(mult)
        .filter(|s| *s > 0)
}

fn human_secs(s: u32) -> String {
    match s {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// `HH:MM` (UTC) for a Unix-ms time.
fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{:02}:{:02}", (secs / 3600) % 24, (secs / 60) % 60)
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn durations_parse_and_print() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("45"), Some(45));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(human_secs(7200), "2h");
        assert_eq!(human_secs(90), "90s");
    }
}
