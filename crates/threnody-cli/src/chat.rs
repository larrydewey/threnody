//! Interactive session: one node, many peers, line-oriented UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::store::Home;
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, now_ms, safety_number};
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
}

pub async fn run(opts: Options) -> Result<()> {
    let downloads = opts.home.dir().join("downloads");
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
            let direct = match resolved {
                Ok((addr, pin)) => node.connect(&addr, pin).await.map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            let Err(e) = direct else { return };
            match known {
                Some(key) => {
                    println!("* {t}: direct path failed ({e:#}); trying relays");
                    if let Err(e) = node.connect_relayed(key.fingerprint()).await {
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
                match save_download(&self.downloads, &name, &data) {
                    Ok(p) => println!(
                        "* {who} sent {name} ({} bytes) -> {}",
                        data.len(),
                        p.display()
                    ),
                    Err(e) => println!("! could not save file from {who}: {e:#}"),
                }
            }
            AppMessage::Group(payload) => {
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                self.groups.incoming(&self.node, &name, peer, &payload);
            }
            _ => {}
        }
    }

    /// Seals `msg` for an absent contact and leaves it with mailboxes.
    fn send_offline(&self, peer: PublicIdentity, msg: &AppMessage) -> Result<()> {
        let who = self.name(&peer);
        if !self.node.can_send_offline(&peer) {
            bail!("{who} is not connected and has not given us prekeys yet; try /relay");
        }
        let n = self.node.send_offline(&peer, msg)?;
        println!("* {who} is not connected; sealed message offered to {n} mailbox(es)");
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
                    None => println!("* connected to {who} at {addr} [tcp, {}]", suite.name()),
                }
                if new_contact {
                    println!("  new contact (trust on first use). Compare safety numbers: /safety");
                }
                if self.current.is_none() {
                    self.current = Some(peer);
                    println!("  messages now go to {who}");
                }
            }
            Event::Message { peer, msg } => self.show_message(peer, msg, None),
            Event::OfflineMessage { from, via, msg } => self.show_message(from, msg, Some(via)),
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
            let msg = AppMessage::Text {
                sent_ms: now_ms(),
                body: line.to_owned(),
            };
            if self.node.send(&peer, msg.clone()).is_err() {
                self.send_offline(peer, &msg)?;
            }
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
                        None => format!("{} {}", i.transport, i.addr),
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
                self.node.send(
                    &peer,
                    AppMessage::File {
                        sent_ms: now_ms(),
                        name,
                        data,
                    },
                )?;
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
            "status" => self.status(),
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
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                self.groups.say(
                    &self.node,
                    &name,
                    arg.ok_or_else(|| anyhow!("usage: /g <group> <text>"))?,
                )?;
            }
            other => bail!("unknown command /{other}; try /help"),
        }
        Ok(false)
    }

    /// Spec §10: persistent indicators of transport, tunnel and protection.
    fn status(&self) {
        let sessions = self.node.sessions();
        println!("  identity     {}", self.node.identity().fingerprint());
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
