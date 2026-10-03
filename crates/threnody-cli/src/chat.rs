//! Interactive session: one node, many peers, line-oriented UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::store::Home;
use threnody_core::{AppMessage, Identity, PublicIdentity, now_ms, safety_number};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::io::{AsyncBufReadExt, BufReader};

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
}

pub struct TunnelOptions {
    pub port: u16,
    pub iface: String,
    pub apply: bool,
}

const HELP: &str = "\
Type a line to send it to the current peer. Commands:
  /connect <invite|contact|host:port>   dial a peer
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
  /quit";

struct Ui {
    node: Node,
    downloads: PathBuf,
    listen_addr: Option<std::net::SocketAddr>,
    current: Option<PublicIdentity>,
    tunnels: Option<Tunnels>,
}

pub async fn run(opts: Options) -> Result<()> {
    let downloads = opts.home.dir().join("downloads");
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
    };
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

impl Ui {
    fn name(&self, p: &PublicIdentity) -> String {
        self.node
            .contacts()
            .get(p)
            .map_or_else(|| p.fingerprint().to_string(), |c| c.label())
    }

    fn resolve_peer(&self, arg: Option<&str>) -> Result<PublicIdentity> {
        match arg {
            Some(q) => Ok(find_contact(&self.node.contacts(), q)?.key),
            None => self
                .current
                .ok_or_else(|| anyhow!("no current peer; use /to <peer>")),
        }
    }

    fn connect(&self, t: &str) {
        let resolved = target::resolve(t, &self.node.contacts());
        let node = self.node.clone();
        let t = t.to_owned();
        tokio::spawn(async move {
            let r = match resolved {
                Ok((addr, pin)) => node.connect(&addr, pin).await.map_err(anyhow::Error::from),
                Err(e) => Err(e),
            };
            if let Err(e) = r {
                println!("! connect {t}: {e:#}");
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
            } => {
                let who = self.name(&peer);
                println!("* connected to {who} at {addr} [tcp, {}]", suite.name());
                if new_contact {
                    println!("  new contact (trust on first use). Compare safety numbers: /safety");
                }
                if self.current.is_none() {
                    self.current = Some(peer);
                    println!("  messages now go to {who}");
                }
            }
            Event::Message { peer, msg } => {
                let who = self.name(&peer);
                match msg {
                    AppMessage::Text { body, .. } => println!("<{who}> {body}"),
                    AppMessage::File { name, data, .. } => {
                        match save_download(&self.downloads, &name, &data) {
                            Ok(p) => println!(
                                "* {who} sent {} ({} bytes) -> {}",
                                name,
                                data.len(),
                                p.display()
                            ),
                            Err(e) => println!("! could not save file from {who}: {e:#}"),
                        }
                    }
                    _ => {}
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
                println!("* {} disconnected ({reason})", self.name(&peer));
                if self.current == Some(peer) {
                    self.current = None;
                }
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
            self.node
                .send(
                    &peer,
                    AppMessage::Text {
                        sent_ms: now_ms(),
                        body: line.to_owned(),
                    },
                )
                .map_err(|_| anyhow!("{} is not connected", self.name(&peer)))?;
            return Ok(false);
        };
        let mut parts = cmd.splitn(2, ' ');
        let verb = parts.next().unwrap_or_default();
        let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());
        match verb {
            "quit" | "q" | "exit" => return Ok(true),
            "help" | "h" | "?" => println!("{HELP}"),
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
                    println!(
                        "  {} via {} {} ({dir}, {})",
                        self.name(&i.peer),
                        i.transport,
                        i.addr,
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
        match &self.tunnels {
            Some(t) => println!("  tunnel       {}", t.summary()),
            None => println!("  tunnel       off (run with --tunnel)"),
        }
        let rate = match self.node.constant_rate() {
            Some(d) => format!("constant-rate {} ms with cover traffic", d.as_millis()),
            None => "off".into(),
        };
        println!(
            "  metadata     length padding: on; timing protection: {rate}; onion routing: not available"
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
