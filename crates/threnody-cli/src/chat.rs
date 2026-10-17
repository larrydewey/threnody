//! Interactive session: one node, many peers, line-oriented UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::history::{ConversationId, FileNote, Query};
use threnody_core::message::Clip;
use threnody_core::store::Home;
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, safety_number};
use threnody_net::history::OutgoingFile;
use threnody_net::mailbox::DepositStatus;
use threnody_net::reach::ReachConfig;
use threnody_net::{AcceptPolicy, DiscoveryConfig, Event, Node, NodeConfig};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::groups::GroupUi;
use crate::tunnel::Tunnels;
use crate::{describe_contact, find_contact, target};

pub struct Options {
    pub home: Home,
    pub identity: Identity,
    /// Running as an anonymous identity: the main identity, kept only to
    /// sign a reveal.
    pub main_identity: Option<Identity>,
    pub listen: Option<String>,
    pub connect: Vec<String>,
    pub policy: AcceptPolicy,
    pub constant_rate: Option<Duration>,
    /// Reach contacts through onion circuits first when possible.
    pub onion_first: bool,
    /// Strip metadata from images we send.
    pub strip_metadata: bool,
    /// Disappearing timer for conversations that haven't set one.
    pub default_timer: Option<u32>,
    pub tunnel: Option<TunnelOptions>,
    /// UDP port for LAN discovery; `None` disables it.
    pub discover: Option<u16>,
    /// Reach contacts across the internet (DHT rendezvous, hole punching).
    pub rendezvous: bool,
    /// Always-on approved contact: keep a session to it so address
    /// updates reach it immediately, in the background too.
    pub anchor: Option<String>,
    /// Advertise and accept sessions over Bluetooth LE.
    pub ble: bool,
    /// Join Wi-Fi Direct groups that approved contacts offer.
    pub wifi_direct: bool,
    /// Public addresses to volunteer as a relay at (none: don't).
    pub volunteer_relay: Vec<String>,
    /// Run a directory at this public address, reviewing relays if set.
    pub serve_directory: Option<(String, bool)>,
    /// Route through volunteer relays from subscribed directories.
    pub use_volunteers: bool,
    pub directory_threshold: usize,
    /// Run without a terminal, as a service: commands come over this
    /// control socket ([`control_path`]), not stdin.
    pub control: Option<PathBuf>,
}

/// Where a node running as a service takes commands (`threnody ctl`):
/// `$THRENODY_CONTROL`, else `control.sock` in the data directory.
pub fn control_path(home: &Home) -> PathBuf {
    std::env::var_os("THRENODY_CONTROL")
        .map_or_else(|| home.dir().join("control.sock"), PathBuf::from)
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
  /requests   /accept <peer>   /block <peer>   /delete <peer>   message requests
  /del <n> [all]                        delete message n of the last /history (all: for everyone)
  /edit <n> <text>                      edit your message n of the last /history
  /reply <n> <text>                     reply to message n of the last /history, quoting it
  /pin <n> [off]                        pin (or unpin) message n of the last /history
  /pins                                 the current chat's pinned messages
  /react <n> <emoji> [off]              react to message n of the last /history (again to take it back)
  /safety [peer]                        show the safety number
  /verify [peer]                        mark safety number as confirmed
  /file [-s] <path>... [| caption]      send files (photos) to the current peer;
                                        -s marks them sensitive
  /drop [peer]                          close a session
  /clear [peer]                         delete the messages, keep the contact (all your devices)
  /forget <peer>                        delete a contact and the conversation, on all your devices
  /policy anyone|contacts|approved      who may connect to us
  /status                               transports and protection level
  /keepalive <secs>                     gap between keepalives on contacts' standby paths
  /devices   /device add [host:port]   /device rename <name> <new>   /device remove <name>
  /ble scan [secs]   /ble connect <n|address>   Bluetooth LE (Linux)
  /wifi-direct [request|leave]          ask the current peer for a Wi-Fi Direct link
  /history [peer] [n]                   recent messages (stored encrypted)
  /search [@peer|@group] <words>        find messages everywhere, or in one chat;
                                        \"quoted words\" match as a phrase
  /disappear <30s|10m|1h|1d|off>        disappearing messages with the current peer
  /profile [set <key> <value> | unset <key>]   your profile (shared with no one by default)
  /share [peer] [key,key,…|none]        which profile details a contact sees
  /reveal [peer] [host:port]            (anonymous identity) prove to them who you are
Volunteer relays and directories (Appendix P):
  /dir [list]   /dir add <link>   /dir remove <id>   /dir refresh   /dir relays
  /vonion <peer|invite>                 reach a peer through two volunteer relays
  /volunteer <host:port>...|off         volunteer as a relay for strangers' circuits
  /directory [relays|link]   /directory list|unlist <relay>   (when serving a directory)
Credentials (Appendix O, zero-knowledge selective disclosure):
  /cred [list]                          credentials you hold
  /cred offer <peer> <schema> <key=value>... [days=N]   issue one to a peer
  /cred offers   /cred accept <id>   /cred decline <id>   offers made to you
  /cred ask <peer> <schema> <key,key,…> ask a peer to prove attributes
  /cred asks   /cred present <id> <n> [key,key,…]   prove credential n (of /cred list)
  /cred delete <n>
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
    /// What the last /history (or one-chat /search) showed, numbered from
    /// 1: /del refers to it.
    shown: std::cell::RefCell<(Option<PublicIdentity>, Vec<threnody_core::history::Entry>)>,
    #[cfg(all(feature = "ble", target_os = "linux"))]
    ble_seen: std::sync::Arc<std::sync::Mutex<Vec<crate::ble::Found>>>,
    wifi_direct: bool,
    /// NetworkManager profiles of Wi-Fi Direct groups we joined.
    joined: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    scratch: PathBuf,
    main_identity: Option<Identity>,
}

pub async fn run(opts: Options) -> Result<()> {
    let downloads = opts.home.dir().join("downloads");
    let opts_dir = opts.home.dir().to_path_buf();
    let control = opts.control.as_deref().map(control::listen).transpose()?;
    crate::out::set_service(control.is_some());
    let groups =
        GroupUi::load(&opts.home, &opts.identity, downloads.clone()).context("loading groups")?;
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
    node.set_prefer_onion(opts.onion_first);
    node.set_use_volunteers(opts.use_volunteers);
    node.set_directory_threshold(opts.directory_threshold);
    if let Some((addr, review)) = &opts.serve_directory {
        let link = node.serve_directory(addr, *review)?;
        say!("Serving a relay directory. Share this link with people who should use it:");
        say!("  {link}");
        if *review {
            say!("  Relays wait for /directory list <relay> before they're listed.");
        }
    }
    if !opts.volunteer_relay.is_empty() {
        if node.directories().is_empty() {
            say!(
                "  note: subscribe to a directory (/dir add <link>) to be listed as a volunteer relay"
            );
        }
        node.set_volunteer(Some(opts.volunteer_relay.clone()))?;
        say!(
            "Volunteering as a relay at {}",
            opts.volunteer_relay.join(", ")
        );
    }
    node.set_strip_metadata(opts.strip_metadata);
    node.set_default_timer(opts.default_timer);
    if opts.main_identity.is_some() {
        say!(
            "Threnody — anonymous identity {}. Nothing links it to your main one unless you /reveal it.",
            node.identity().fingerprint()
        );
    } else {
        say!("Threnody — you are {}", node.identity().fingerprint());
    }
    if groups.count() > 0 {
        say!("{} group(s) restored. /groups to list", groups.count());
    }
    if let Some(t) = &tunnels {
        say!("Tunnels on: overlay address {}", t.overlay());
        say!("  WireGuard config: {}", t.conf_path().display());
        if opts.tunnel.as_ref().is_some_and(|o| !o.apply) {
            say!(
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
            say!(
                "Listening on {bound} (tcp). Share: threnody invite <your-ip>:{}",
                bound.port()
            );
            Some(bound)
        }
        None => None,
    };
    node.set_reach(opts.rendezvous);
    let quic_addr = match listen_addr {
        Some(tcp) => listen_quic(&node, tcp).await,
        None => None,
    };
    if let Some(q) = quic_addr {
        // On loopback there is nobody to reach.
        if node.reach_enabled() && !q.ip().is_loopback() {
            let anchor = opts
                .anchor
                .as_deref()
                .and_then(|name| match node.contacts().find(name) {
                    threnody_core::store::Lookup::Found(c) if c.mutually_approved() => Some(c.key),
                    _ => {
                        eprintln!("! --anchor: {name:?} is not a mutually approved contact");
                        None
                    }
                });
            match node.start_reach(ReachConfig {
                anchor,
                ..ReachConfig::default()
            }) {
                Ok(()) => say!(
                    "QUIC on udp/{}; reaching approved contacts across the internet \
                     (public DHT rendezvous; --no-rendezvous turns it off)",
                    q.port()
                ),
                Err(e) => say!("! internet reachability unavailable: {e}"),
            }
        } else {
            say!("QUIC on udp/{}; internet rendezvous off", q.port());
        }
    }
    let mut ui = Ui {
        node,
        downloads,
        listen_addr,
        current: None,
        tunnels,
        discovery: None,
        groups,
        shown: std::cell::RefCell::new((None, Vec::new())),
        #[cfg(all(feature = "ble", target_os = "linux"))]
        ble_seen: std::sync::Arc::default(),
        wifi_direct: opts.wifi_direct,
        joined: std::sync::Arc::default(),
        scratch: opts_dir,
        main_identity: opts.main_identity,
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
                say!("LAN discovery on udp/{port}: approved peers nearby connect automatically.");
            }
            Err(e) => say!("! LAN discovery unavailable: {e}"),
        }
    }
    for t in &opts.connect {
        ui.connect(t);
    }
    #[cfg(all(feature = "ble", target_os = "linux"))]
    let _ble = if opts.ble {
        match crate::ble::listen(&ui.node).await {
            Ok(l) => {
                say!(
                    "Bluetooth LE: advertising, L2CAP psm {}; approved contacts nearby connect automatically",
                    l.psm
                );
                Some(l)
            }
            Err(e) => {
                say!("! Bluetooth LE unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    #[cfg(not(all(feature = "ble", target_os = "linux")))]
    if opts.ble {
        say!("! this build has no Bluetooth LE support");
    }
    if control.is_some() {
        say!("Running as a service. Commands: threnody ctl <command> (/help lists them).");
    } else {
        say!("Type /help for commands.");
    }

    let mut lines = control
        .is_none()
        .then(|| BufReader::new(tokio::io::stdin()).lines());
    let stopped = stop_signal();
    tokio::pin!(stopped);
    loop {
        tokio::select! {
            line = next_line(&mut lines) => {
                let Some(line) = line? else { break };
                match ui.handle_line(line.trim()).await {
                    Ok(true) => break,
                    Ok(false) => {}
                    Err(e) => say!("! {e:#}"),
                }
            }
            conn = control::accept(control.as_ref()) => {
                if control::serve(&mut ui, conn).await {
                    break;
                }
            }
            Some(ev) = events.recv() => ui.handle_event(ev),
            () = &mut stopped => break,
        }
    }
    ui.leave_wifi_direct().await;
    #[cfg(all(feature = "ble", target_os = "linux"))]
    if let Some(b) = _ble {
        b.shutdown().await;
    }
    Ok(())
}

/// The next line typed, or never when stdin isn't read (as a service).
async fn next_line(
    lines: &mut Option<tokio::io::Lines<BufReader<tokio::io::Stdin>>>,
) -> std::io::Result<Option<String>> {
    match lines {
        Some(l) => l.next_line().await,
        None => std::future::pending().await,
    }
}

/// The control socket of a node running as a service: `threnody ctl`
/// sends one command line and gets back what the node said running it.
/// The socket is owner-only and lives in the data directory, which is
/// owner-only too.
mod control {
    use std::path::Path;
    #[cfg(unix)]
    use std::time::Duration;

    #[cfg(unix)]
    use anyhow::Context;
    use anyhow::{Result, bail};
    #[cfg(unix)]
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    use super::Ui;

    /// Longest command line taken.
    #[cfg(unix)]
    const MAX_LINE: u64 = 64 * 1024;
    /// A client gets this long to send its command.
    #[cfg(unix)]
    const READ_TIMEOUT: Duration = Duration::from_secs(10);

    #[cfg(unix)]
    pub type Listener = tokio::net::UnixListener;
    #[cfg(unix)]
    pub type Conn = tokio::net::UnixStream;
    #[cfg(not(unix))]
    pub type Listener = std::convert::Infallible;
    #[cfg(not(unix))]
    pub type Conn = std::convert::Infallible;

    #[cfg(unix)]
    pub fn listen(path: &Path) -> Result<Listener> {
        use std::os::unix::fs::{FileTypeExt, PermissionsExt};
        // A socket left by a node that didn't shut down cleanly; nothing
        // else is ever removed.
        if let Ok(m) = std::fs::symlink_metadata(path) {
            if !m.file_type().is_socket() {
                bail!("{} exists and is not a socket", path.display());
            }
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                bail!("a node is already running here ({})", path.display());
            }
            std::fs::remove_file(path)?;
        }
        let l = Listener::bind(path).with_context(|| {
            format!(
                "control socket {} (a path over about 100 bytes is too long: \
                     set THRENODY_CONTROL to a shorter one)",
                path.display()
            )
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(l)
    }

    #[cfg(not(unix))]
    pub fn listen(_: &Path) -> Result<Listener> {
        bail!("running as a service needs Unix (its control socket)")
    }

    /// The next client, or never without a socket.
    pub async fn accept(l: Option<&Listener>) -> Option<Conn> {
        #[cfg(unix)]
        if let Some(l) = l {
            return l.accept().await.ok().map(|(c, _)| c);
        }
        #[cfg(not(unix))]
        let _ = l;
        std::future::pending().await
    }

    /// Runs the client's command and answers with what was said; `true`
    /// when it was /quit.
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub async fn serve(ui: &mut Ui, conn: Option<Conn>) -> bool {
        let Some(conn) = conn else { return false };
        #[cfg(unix)]
        {
            let (rd, mut wr) = conn.into_split();
            let mut line = String::new();
            let read = tokio::time::timeout(
                READ_TIMEOUT,
                BufReader::new(rd.take(MAX_LINE)).read_line(&mut line),
            )
            .await;
            if !matches!(read, Ok(Ok(n)) if n > 0) {
                return false;
            }
            let line = line.trim();
            // The log says which command ran, not its arguments or answer
            // (a plain line is a message).
            let cmd = line
                .split_whitespace()
                .next()
                .filter(|w| w.starts_with('/'));
            say!("* control: {}", cmd.unwrap_or("message"));
            let (r, mut said) = crate::out::capture(ui.handle_line(line)).await;
            let quit = match r {
                Ok(q) => q,
                Err(e) => {
                    said.push_str(&format!("! {e:#}\n"));
                    false
                }
            };
            let _ = wr.write_all(said.as_bytes()).await;
            let _ = wr.shutdown().await;
            quit
        }
        #[cfg(not(unix))]
        match conn {}
    }
}

/// Ctrl-C, or (on Unix) being stopped by a service manager, `kill` or a
/// closed terminal: each shuts down as /quit does.
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let (Ok(mut term), Ok(mut hup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
                _ = hup.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
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
                    say!("* {t}: direct path failed ({e:#}); trying relays");
                    if let Err(e) = node.connect_relayed(fp).await {
                        say!("! connect {t}: {e:#}");
                    }
                }
                None => say!("! connect {t}: {e:#}"),
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
            AppMessage::Text { .. } if crate::out::private() => {
                say!("<{who}> sent a message (/history to read it)");
            }
            AppMessage::Text { body, quote, .. } => {
                if let Some(q) = quote {
                    say!("<{who}> {}", quoted(&q, &|d| self.name(d)));
                }
                say!("<{who}> {body}");
            }
            AppMessage::File {
                name,
                data,
                id,
                sensitive,
                caption,
                album,
                clip,
                ..
            } => {
                let saved = save_download(&self.downloads, &name, &data);
                let mark = file_mark(sensitive, clip);
                match &saved {
                    Ok(p) => say!(
                        "* {who} sent {name}{mark} ({} bytes) -> {}",
                        data.len(),
                        p.display()
                    ),
                    Err(e) => say!("! could not save file from {who}: {e:#}"),
                }
                if !caption.is_empty() && !crate::out::private() {
                    say!("<{who}> {caption}");
                }
                self.node.record_received_file(
                    &peer,
                    FileNote {
                        name,
                        size: data.len() as u64,
                        location: saved.ok().map(|p| p.display().to_string()),
                        sensitive,
                        album,
                        clip,
                        forwarded: false,
                    },
                    &caption,
                    id,
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
        if r.live + r.sealed + r.queued > 1 || r.sealed + r.queued > 0 {
            say!(
                "* to {} device(s): {} live, {} sealed for mailboxes{}",
                r.live + r.sealed + r.queued,
                r.live,
                r.sealed,
                if r.queued == 0 {
                    String::new()
                } else {
                    format!(", {} waiting to connect", r.queued)
                }
            );
        }
        Ok(())
    }

    /// `/search [@peer|@group] <words>`: every conversation, or one. The
    /// results of a search in one chat with a contact are numbered for
    /// /del, /edit and /react, like /history's.
    fn search(&self, arg: &str) -> Result<()> {
        const LIMIT: usize = 50;
        let usage = || anyhow!("usage: /search [@peer|@group] <words>");
        let (scope, query) = match arg.trim().strip_prefix('@') {
            Some(rest) => {
                let (who, q) = rest.split_once(char::is_whitespace).ok_or_else(usage)?;
                let conv = match find_contact(&self.node.contacts(), who) {
                    Ok(c) => (Some(c.key), self.node.conversation_for(&c.key)),
                    Err(e) => match self.groups.find(who) {
                        Ok(g) => (None, ConversationId::Group(g)),
                        Err(_) => bail!("{e} (and no group matches either)"),
                    },
                };
                (Some(conv), q)
            }
            None => (None, arg),
        };
        if Query::new(query).is_empty() {
            return Err(usage());
        }
        let hits = self.node.search(scope.map(|(_, c)| c), query, LIMIT)?;
        if hits.is_empty() {
            say!("  no messages match");
            return Ok(());
        }
        // Oldest first, like /history, so the newest is nearest the prompt.
        let hits: Vec<_> = hits.into_iter().rev().collect();
        if let Some((Some(peer), _)) = scope {
            *self.shown.borrow_mut() = (Some(peer), hits.iter().map(|h| h.entry.clone()).collect());
        }
        let contacts = self.node.contacts();
        for (i, h) in hits.iter().enumerate() {
            let e = &h.entry;
            let place = match (scope, h.conversation) {
                (Some(_), _) => String::new(),
                (None, ConversationId::Group(g)) => format!("{} ", self.groups.label(&g)),
                (None, ConversationId::Peer(k)) => {
                    let device = contacts
                        .iter()
                        .find(|c| c.account.is_some_and(|a| a.0 == k) || *c.key.as_bytes() == k)
                        .map(|c| c.key)
                        .or_else(|| PublicIdentity::from_bytes(&k).ok());
                    format!("{} ", device.map_or_else(|| "?".into(), |d| self.name(&d)))
                }
            };
            let who = if e.outgoing {
                "me".to_owned()
            } else {
                PublicIdentity::from_bytes(&e.device).map_or_else(|_| "?".into(), |d| self.name(&d))
            };
            let body = match &e.file {
                Some(f) if e.text.is_empty() => format!("file {}", f.name),
                Some(f) => format!("file {}: {}", f.name, e.text),
                None => e.text.clone(),
            };
            if scope.is_some_and(|(p, _)| p.is_some()) {
                say_part!("{:>3}", i + 1);
            } else {
                say_part!("   ");
            }
            say!("  [{}] {place}<{who}> {body}", clock(e.at_ms));
        }
        if hits.len() == LIMIT {
            say!("  (the {LIMIT} newest matches; add words to narrow it)");
        }
        Ok(())
    }

    fn show_history(&self, peer: PublicIdentity, n: usize) -> Result<()> {
        let conv = self.node.conversation_for(&peer);
        let h = self.node.history(conv)?;
        if let Some(t) = self.node.timer(&peer) {
            say!("  (messages disappear after {})", human_secs(t));
        }
        if h.entries().is_empty() {
            say!("  no history with {}", self.name(&peer));
        }
        *self.shown.borrow_mut() = (Some(peer), h.recent(n).to_vec());
        for (i, e) in h.recent(n).iter().enumerate() {
            say_part!("{:>3}", i + 1);
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
            if e.edited_ms != 0 {
                mark.push_str(" (edited)");
            }
            if e.delivered {
                mark.push_str(" ✓✓");
            }
            // Reactions as "👍2 ❤️": each emoji, with a count when several.
            let mut counts: Vec<(&str, usize)> = Vec::new();
            for (_, r) in &e.reactions {
                match counts.iter_mut().find(|(x, _)| *x == r) {
                    Some(c) => c.1 += 1,
                    None => counts.push((r, 1)),
                }
            }
            if !counts.is_empty() {
                let shown: Vec<String> = counts
                    .iter()
                    .map(|(r, n)| {
                        if *n > 1 {
                            format!("{r}{n}")
                        } else {
                            (*r).to_owned()
                        }
                    })
                    .collect();
                mark.push_str(&format!("  [{}]", shown.join(" ")));
            }
            if let Some(q) = &e.quote {
                say_part!("  {}\n   ", quoted(q, &|d| self.name(d)));
            }
            match &e.file {
                Some(f) => say!(
                    "  [{}] <{who}> file {}{} ({} bytes){}{}{mark}",
                    clock(e.at_ms),
                    f.name,
                    if f.sensitive { " [sensitive]" } else { "" },
                    f.size,
                    f.location
                        .as_ref()
                        .map_or_else(String::new, |l| format!(" at {l}")),
                    if e.text.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", e.text)
                    }
                ),
                None => say!("  [{}] <{who}> {}{mark}", clock(e.at_ms), e.text),
            }
        }
        Ok(())
    }

    /// `/profile`, `/profile set <key> <value>`, `/profile unset <key>`.
    fn profile_command(&self, arg: Option<&str>) -> Result<()> {
        let mut profile = self.node.profile();
        match arg.map(|a| a.splitn(3, ' ').collect::<Vec<_>>()).as_deref() {
            None => {}
            Some(["set", key, value]) => {
                let value = value.trim().to_owned();
                match profile.iter_mut().find(|(k, _)| k == key) {
                    Some(kv) => kv.1 = value,
                    None => profile.push(((*key).to_owned(), value)),
                }
                self.node.set_profile(profile.clone())?;
            }
            Some(["unset", key]) => {
                profile.retain(|(k, _)| k != key);
                self.node.set_profile(profile.clone())?;
            }
            _ => bail!("usage: /profile [set <key> <value> | unset <key>]"),
        }
        if profile.is_empty() {
            say!("* your profile is empty. /profile set name <your name>");
        }
        for (k, v) in &profile {
            let who: Vec<String> = self
                .node
                .contacts()
                .iter()
                .filter(|c| c.shares.contains(k))
                .map(|c| c.label())
                .collect();
            let shown = if who.is_empty() {
                "shared with no one".to_owned()
            } else {
                format!("shared with {}", who.join(", "))
            };
            say!("  {k}: {v}  ({shown})");
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
                say!("! relay to {who}: {e:#}");
            }
        });
    }

    /// Joins an offered Wi-Fi Direct group and opens a session over it,
    /// pinning the peer; the new session replaces the slower one.
    fn wifi_direct_offer(&self, peer: PublicIdentity, offer: threnody_net::DirectOffer) {
        let who = self.name(&peer);
        if !self.wifi_direct {
            say!(
                "* {who} offers a Wi-Fi Direct link ({}); run with --wifi-direct to join",
                offer.ssid
            );
            return;
        }
        say!(
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
                    say!("! Wi-Fi Direct join: {e:#}");
                    return;
                }
            }
            if let Err(e) = node.connect(&offer.addr, Some(peer.fingerprint())).await {
                say!("! Wi-Fi Direct connect to {}: {e:#}", offer.addr);
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
                Ok(()) => say!("* left Wi-Fi Direct group ({n})"),
                Err(e) => say!("! leaving {n}: {e:#}"),
            }
        }
    }

    async fn dir_command(&self, arg: Option<&str>) -> Result<()> {
        let mut words = arg.unwrap_or("").split_whitespace();
        match words.next().unwrap_or("list") {
            "list" => {
                let dirs = self.node.directories();
                if dirs.is_empty() {
                    say!("No directories. /dir add <threnody-dir://…> subscribes to one.");
                }
                for d in dirs {
                    let until = d.valid_until_ms.map_or("no document".to_owned(), |t| {
                        format!("valid until {}", fmt_time(t))
                    });
                    say!(
                        "  {}  {} relays, {until}, {} token credential(s)\n    {}",
                        threnody_core::directory::short_id(&d.issuer),
                        d.relays,
                        d.tokens,
                        d.link
                    );
                }
                let usable = self.node.volunteer_relays().len();
                say!(
                    "  {usable} volunteer relay(s) usable now; routing through them is {}",
                    if self.node.use_volunteers() {
                        "on"
                    } else {
                        "off"
                    }
                );
            }
            "add" => {
                let link = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /dir add <threnody-dir://…>"))?;
                say!("* subscribing (over an anonymous link)…");
                let d = self.node.subscribe_directory(link).await?;
                say!(
                    "* subscribed to directory {}: {} relays, {} token credential(s)",
                    threnody_core::directory::short_id(&d.issuer),
                    d.relays,
                    d.tokens
                );
            }
            "remove" | "rm" => {
                let id = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /dir remove <id>"))?;
                let found = self.node.directories().into_iter().find(|d| {
                    threnody_core::directory::short_id(&d.issuer).starts_with(&id.to_uppercase())
                });
                match found {
                    Some(d) if self.node.unsubscribe_directory(&d.issuer) => {
                        say!("* unsubscribed")
                    }
                    _ => bail!("no directory {id}"),
                }
            }
            "refresh" => {
                self.node.refresh_directories().await;
                say!("* directories refreshed");
            }
            "relays" => {
                for r in self.node.volunteer_relays() {
                    say!("  {}  {}", r.identity.fingerprint(), r.addrs.join(", "));
                }
            }
            _ => bail!("usage: /dir [list|add <link>|remove <id>|refresh|relays]"),
        }
        Ok(())
    }

    fn directory_command(&self, arg: Option<&str>) -> Result<()> {
        if !self.node.serving_directory() {
            bail!("not serving a directory (run with --serve-directory <host:port>)");
        }
        let mut words = arg.unwrap_or("").split_whitespace();
        match words.next().unwrap_or("relays") {
            "relays" => {
                let relays = self.node.directory_relays();
                if relays.is_empty() {
                    say!("No relays have registered yet.");
                }
                for r in relays {
                    say!(
                        "  {} {}  {}  (descriptor expires {})",
                        if r.listed { "listed " } else { "waiting" },
                        r.identity.fingerprint(),
                        r.addrs.join(", "),
                        fmt_time(r.expires_ms)
                    );
                }
            }
            cmd @ ("list" | "unlist") => {
                let q = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /directory {cmd} <relay fingerprint>"))?;
                let relay = self
                    .node
                    .directory_relays()
                    .into_iter()
                    .find(|r| {
                        threnody_core::identity::fingerprint_matches_prefix(
                            &r.identity.fingerprint(),
                            q,
                        )
                    })
                    .ok_or_else(|| anyhow!("no registered relay {q}"))?;
                self.node
                    .set_relay_listed(&relay.identity.fingerprint(), cmd == "list");
                say!(
                    "* {} {}",
                    relay.identity.fingerprint(),
                    if cmd == "list" { "listed" } else { "unlisted" }
                );
            }
            _ => bail!("usage: /directory [relays|list <relay>|unlist <relay>]"),
        }
        Ok(())
    }

    fn cred_command(&self, arg: Option<&str>) -> Result<()> {
        let mut words = arg.unwrap_or("").split_whitespace();
        let parse_id = |w: Option<&str>| -> Result<u64> {
            let w = w.ok_or_else(|| anyhow!("missing id"))?;
            u64::from_str_radix(w, 16).map_err(|_| anyhow!("{w:?} is not an id"))
        };
        match words.next().unwrap_or("list") {
            "list" => {
                let held = self.node.credentials();
                if held.is_empty() {
                    say!("No credentials. Someone you know can /cred offer you one.");
                }
                for (i, c) in held.iter().enumerate() {
                    let attrs: Vec<String> = c
                        .attributes
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    say!(
                        "  {}. {} from {} (expires day {}): {}",
                        i + 1,
                        c.schema,
                        c.issuer,
                        c.expires_day,
                        attrs.join(", ")
                    );
                }
            }
            "offer" => {
                let peer = self.resolve_peer(words.next())?;
                let schema = words.next().ok_or_else(|| {
                    anyhow!("usage: /cred offer <peer> <schema> key=value… [days=N]")
                })?;
                let mut days = 365u32;
                let mut attrs = Vec::new();
                for w in words {
                    let (k, v) = w
                        .split_once('=')
                        .ok_or_else(|| anyhow!("{w:?}: expected key=value"))?;
                    if k == "days" {
                        days = v.parse()?;
                    } else {
                        attrs.push((k.to_owned(), v.to_owned()));
                    }
                }
                let expires = threnody_core::credential::day(threnody_core::now_ms()) + days;
                let id = self.node.offer_credential(&peer, schema, attrs, expires)?;
                say!(
                    "* offered {} a credential ({schema}, {id:x})",
                    self.name(&peer)
                );
            }
            "offers" => {
                for o in self.node.credential_offers() {
                    let attrs: Vec<String> = o
                        .attributes
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect();
                    say!(
                        "  {:x}  {} from {}: {}",
                        o.id,
                        o.schema,
                        self.name(&o.peer),
                        attrs.join(", ")
                    );
                }
            }
            "accept" => {
                self.node.accept_credential_offer(parse_id(words.next())?)?;
                say!("* accepted; waiting for the issuer to sign");
            }
            "decline" => {
                self.node.decline_credential(parse_id(words.next())?)?;
                say!("* declined");
            }
            "ask" => {
                let peer = self.resolve_peer(words.next())?;
                let schema = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /cred ask <peer> <schema> <key,key,…>"))?;
                let keys: Vec<String> = words
                    .next()
                    .map(|k| {
                        k.split(',')
                            .filter(|k| !k.is_empty())
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let id = self.node.ask_credential(&peer, schema, keys)?;
                say!(
                    "* asked {} for a credential ({schema}, {id:x})",
                    self.name(&peer)
                );
            }
            "asks" => {
                for a in self.node.credential_asks() {
                    say!(
                        "  {:x}  {} asks for {} from {}",
                        a.id,
                        self.name(&a.peer),
                        a.keys.join(", "),
                        a.schema
                    );
                }
            }
            "present" => {
                let id = parse_id(words.next())?;
                let n: usize = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /cred present <id> <n> [key,key,…]"))?
                    .parse()?;
                let held = self.node.credentials();
                let cred = held
                    .get(n.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("no credential {n}"))?;
                let ask = self
                    .node
                    .credential_asks()
                    .into_iter()
                    .find(|a| a.id == id)
                    .ok_or_else(|| anyhow!("no request {id:x}"))?;
                let keys: Vec<String> = match words.next() {
                    Some(k) => k.split(',').map(str::to_owned).collect(),
                    None => ask.keys.clone(),
                };
                self.node.present_credential(id, cred.id, &keys)?;
                let shown: Vec<&String> = keys.iter().filter(|k| ask.keys.contains(k)).collect();
                say!(
                    "* proved to {}: {}",
                    self.name(&ask.peer),
                    if shown.is_empty() {
                        "that you hold it".to_owned()
                    } else {
                        shown
                            .iter()
                            .map(|k| k.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
            }
            "delete" => {
                let n: usize = words
                    .next()
                    .ok_or_else(|| anyhow!("usage: /cred delete <n>"))?
                    .parse()?;
                let held = self.node.credentials();
                let cred = held
                    .get(n.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("no credential {n}"))?;
                self.node.delete_credential(cred.id);
                say!("* deleted");
            }
            _ => bail!("see /help for /cred commands"),
        }
        Ok(())
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
                        say!(
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
                        say!("* connected to {who} at {r} [{t}, {}]", suite.name());
                    }
                }
                if new_contact {
                    say!("  new contact (trust on first use). Compare safety numbers: /safety");
                }
                if self.current.is_none() {
                    self.current = Some(peer);
                    say!("  messages now go to {who}");
                }
                let node = self.node.clone();
                let name = |p: &PublicIdentity| name_of(&node, p);
                self.groups.connected(&self.node, &name, &peer);
            }
            Event::Message { peer, msg } => self.show_message(peer, msg, None),
            Event::MessageEdited { peer, .. } => {
                say!(
                    "* {} edited a message (/history to see it)",
                    self.name(&peer)
                );
            }
            Event::MessagesDeleted { peer, count } => {
                say!("* {} deleted {count} message(s)", self.name(&peer));
            }
            Event::MessageRequest { peer, msg } => {
                let what = match &msg {
                    AppMessage::Text { .. } if crate::out::private() => "a message".into(),
                    AppMessage::Text { body, .. } => format!("{body:?}"),
                    AppMessage::File { name, .. } => format!("a file ({name})"),
                    _ => "a message".into(),
                };
                say!(
                    "? message request from {}: {what} — /accept {p}, /block {p} or /delete {p}",
                    self.name(&peer),
                    p = &peer.fingerprint().to_string()[..9]
                );
            }
            Event::OfflineMessage { from, via, msg } => self.show_message(from, msg, Some(via)),
            // A member acknowledged a group message we forwarded: send the
            // sender a receipt.
            Event::Delivered {
                peer,
                local_id,
                group: Some(group),
                relay_for: Some(origin),
            } => self
                .groups
                .relayed(&self.node, &peer, &group, local_id, &origin),
            // Shown as ✓✓ in /history; too chatty to print live.
            Event::Delivered { .. } | Event::Read { .. } => {}
            // Calls are for the apps; the terminal only says one rang.
            Event::CallIncoming { peer, .. } => {
                say!("* {} is calling (answer in the app)", self.name(&peer));
            }
            Event::CallRinging { .. }
            | Event::CallStarted { .. }
            | Event::CallSignal { .. }
            | Event::CallVideo { .. }
            | Event::CallReaction { .. }
            | Event::CallEnded { .. } => {}
            Event::Typing { peer, active } => {
                if active {
                    say!("* {} is typing", self.name(&peer));
                }
            }
            Event::Reacted { peer, .. } => {
                say!("* {} reacted (see /history)", self.name(&peer));
            }
            Event::Pinned { peer, pin, .. } => say!(
                "* {} {} a message (see /pins)",
                self.name(&peer),
                if pin { "pinned" } else { "unpinned" }
            ),
            Event::ProfileChanged { peer } => {
                let shown = self
                    .node
                    .contacts()
                    .get(&peer)
                    .map(|c| c.profile.clone())
                    .unwrap_or_default();
                if shown.is_empty() {
                    say!(
                        "* {} no longer shares any profile details",
                        self.name(&peer)
                    );
                } else {
                    let list: Vec<String> =
                        shown.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    say!("* {} shares {}", self.name(&peer), list.join(", "));
                }
            }
            Event::IdentityRevealed {
                peer,
                identity,
                invite,
            } => {
                say!(
                    "* {} revealed who they are: {} (proven by its signature)",
                    self.name(&peer),
                    identity.fingerprint()
                );
                match invite {
                    Some(i) => say!("  to add them: /connect {i}"),
                    None => say!(
                        "  they gave no address; add them when you meet: {}",
                        identity.fingerprint()
                    ),
                }
            }
            Event::HistorySynced { from, added } => {
                say!(
                    "* {added} message(s) synced from your device {}",
                    self.name(&from)
                );
            }
            Event::DepositReceipt {
                mailbox,
                to,
                status,
                anonymous,
            } => {
                let (m, t) = (self.name(&mailbox), self.name(&to));
                let how = if anonymous { " (sender hidden)" } else { "" };
                match status {
                    DepositStatus::Held => say!("* {m} is holding your message for {t}{how}"),
                    DepositStatus::Delivered => {
                        say!("* {m} delivered your message to {t}{how}")
                    }
                    DepositStatus::Declined => say!(
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
                say!(
                    "* {who} {state}{}",
                    if mutual { " — mutually approved" } else { "" }
                );
            }
            Event::WifiDirectOffer { peer, offer } => self.wifi_direct_offer(peer, offer),
            Event::WifiDirectRequested { peer } => say!(
                "* {} asked for a Wi-Fi Direct link; this device can only join groups, not host them",
                self.name(&peer)
            ),
            Event::Disconnected { peer, reason } => {
                // Keep `current`: plain lines then go out as sealed messages.
                say!("* {} disconnected ({reason})", self.name(&peer));
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
                        Ok(()) => say!(
                            "* tunnel to {who}: {overlay} via {endpoint} (PSK from this session)"
                        ),
                        Err(e) => say!("! tunnel to {who}: {e:#}"),
                    }
                }
            }
            Event::TunnelDown { peer, wg_public } => {
                let who = self.name(&peer);
                if let Some(t) = self.tunnels.as_mut() {
                    match t.down(&peer, wg_public) {
                        Ok(true) => say!("* tunnel to {who} removed"),
                        Ok(false) => {}
                        Err(e) => say!("! removing tunnel to {who}: {e:#}"),
                    }
                }
            }
            Event::Discovered {
                peer,
                addr,
                connected,
            } => {
                if !connected {
                    say!("* found {} nearby at {addr}", self.name(&peer));
                }
            }
            Event::DialFailed { peer, addr, reason } => {
                say!("! could not reach {} at {addr}: {reason}", self.name(&peer));
            }
            Event::Addresses {
                candidates,
                symmetric,
            } => {
                let list: Vec<String> = candidates.iter().map(ToString::to_string).collect();
                say!(
                    "* internet addresses: {}{}",
                    list.join(", "),
                    if symmetric { " (symmetric NAT)" } else { "" }
                );
            }
            Event::Punching {
                peer,
                targets,
                dialing,
            } => {
                let list: Vec<String> = targets.iter().map(ToString::to_string).collect();
                say!(
                    "* opening a direct path to {} ({}): {}",
                    self.name(&peer),
                    if dialing { "dialing" } else { "probing" },
                    list.join(", ")
                );
            }
            Event::ReachNote { note } => say!("  · {note}"),
            Event::VolunteerNote { note } => say!("  · {note}"),
            Event::CredentialOffered { offer } => {
                let attrs: Vec<String> = offer
                    .attributes
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                say!(
                    "* {} offers you a credential ({}): {}. /cred accept {:x} or /cred decline {:x}",
                    self.name(&offer.peer),
                    offer.schema,
                    attrs.join(", "),
                    offer.id,
                    offer.id
                );
            }
            Event::CredentialReceived { peer, schema } => {
                say!(
                    "* got a credential ({schema}) from {}. /cred list",
                    self.name(&peer)
                );
            }
            Event::CredentialAsked { ask } => {
                say!(
                    "* {} asks you to prove {} from your {} credential. /cred present {:x} <n> or /cred decline {:x}",
                    self.name(&ask.peer),
                    if ask.keys.is_empty() {
                        "that you hold one".to_owned()
                    } else {
                        ask.keys.join(", ")
                    },
                    ask.schema,
                    ask.id,
                    ask.id
                );
            }
            Event::CredentialPresented { peer, verified, .. } => {
                let attrs: Vec<String> = verified
                    .attributes
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                say!(
                    "* {} proved a credential ({}) issued by {}: {}",
                    self.name(&peer),
                    verified.schema,
                    self.name(&verified.issuer),
                    if attrs.is_empty() {
                        "(nothing else shown)".to_owned()
                    } else {
                        attrs.join(", ")
                    }
                );
                say!("  pseudonym for you: {}", hex8(&verified.pseudonym));
            }
            Event::CredentialFailed { peer, reason, .. } => {
                say!("! credential exchange with {}: {reason}", self.name(&peer));
            }
            Event::PunchFailed { peer } => {
                say!("* no direct path to {} yet", self.name(&peer));
            }
            Event::AccountChanged {
                account,
                added,
                removed,
            } => {
                let a = account.fingerprint().to_string();
                // Did we verify any device of this account? Then a new one is a
                // key we haven't checked in a conversation we trusted.
                let verified_before = account != self.node.account().id()
                    && self.node.contacts().iter().any(|c| {
                        c.account == Some(account) && c.verified && !added.contains(&c.key)
                    });
                for d in added {
                    if verified_before {
                        say!(
                            "! SAFETY: account {} added device {} that you haven't verified — /safety {} before trusting it",
                            &a[..9],
                            self.name(&d),
                            &d.fingerprint().to_string()[..9]
                        );
                    } else {
                        say!("* account {} added device {}", &a[..9], self.name(&d));
                    }
                }
                for d in removed {
                    say!(
                        "! account {} REMOVED device {} — it is no longer trusted",
                        &a[..9],
                        self.name(&d)
                    );
                }
            }
            Event::AccountFork { device, account } => say!(
                "! WARNING: {} presented a conflicting history for account {} — possible compromise; ignored",
                self.name(&device),
                account.fingerprint()
            ),
            Event::DeviceLinked { device } => {
                say!(
                    "* linked new device {} into your account",
                    self.name(&device)
                );
                self.groups.linked(&self.node, &device);
            }
            Event::LinkRejected { device } => {
                say!("! rejected a link attempt from {}", self.name(&device))
            }
            Event::ListChanged { .. } => say!("* a list changed"),
            Event::FoldersChanged { from } => {
                say!("* folders updated from {}", self.name(&from))
            }
            Event::ContactsSynced { from } => {
                say!("* contacts updated from {}", self.name(&from))
            }
            Event::TimerChanged { peer, secs } => match secs {
                Some(t) => say!(
                    "* {} set messages to disappear after {}",
                    self.name(&peer),
                    human_secs(t)
                ),
                None => say!("* {} turned disappearing messages off", self.name(&peer)),
            },
            Event::ThisDeviceRemoved => {
                say!("! THIS DEVICE WAS REMOVED from its account; peers will refuse it")
            }
            Event::Rejected { addr, reason } => {
                say!("* rejected connection from {addr}: {reason}")
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
            "help" | "h" | "?" => say!("{HELP}\n{}", crate::groups::HELP),
            "connect" | "c" => {
                self.connect(arg.ok_or_else(|| anyhow!("usage: /connect <target>"))?)
            }
            "keepalive" => {
                let secs: u64 = arg
                    .and_then(|a| a.trim().parse().ok())
                    .ok_or_else(|| anyhow!("usage: /keepalive <seconds>"))?;
                self.node
                    .set_standby_keepalive(std::time::Duration::from_secs(secs));
                say!("* keepalives on standby holes every {secs} s");
            }
            "to" => {
                let p =
                    self.resolve_peer(Some(arg.ok_or_else(|| anyhow!("usage: /to <peer>"))?))?;
                self.current = Some(p);
                // Not connected: look for it across the internet now.
                self.node.seek(&p);
                say!("* messages now go to {}", self.name(&p));
            }
            "peers" => {
                let s = self.node.sessions();
                if s.is_empty() {
                    say!("No live sessions.");
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
                    say!(
                        "  {} via {path} ({dir}, {})",
                        self.name(&i.peer),
                        i.suite.name()
                    );
                }
            }
            "contacts" => {
                for c in self.node.contacts().iter() {
                    say!("  {}", describe_contact(c));
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
                say!("* named {}", self.name(&key));
            }
            "react" => {
                let mut it = arg.unwrap_or("").split_whitespace();
                let usage = || anyhow!("usage: /react <n> <emoji> [off] (n from /history)");
                let n: usize = it.next().and_then(|n| n.parse().ok()).ok_or_else(usage)?;
                let emoji = it.next().ok_or_else(usage)?;
                let (peer, shown) = self.shown.borrow().clone();
                let peer =
                    peer.ok_or_else(|| anyhow!("run /history first; /react uses its numbers"))?;
                let e = shown
                    .get(n.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| anyhow!("no message {n} in the last /history"))?;
                // Toggles, unless `off` says to remove.
                let me = self.node.reactor_of(&self.node.identity());
                let add = it.next() != Some("off")
                    && !e.reactions.iter().any(|(w, r)| *w == me && r == emoji);
                if e.message_id() == 0 || !self.node.react(&peer, e.message_id(), emoji, add) {
                    bail!(
                        "that message can't take reactions (it's gone, or too old to have an id)"
                    );
                }
                say!("* {} {emoji}", if add { "reacted" } else { "took back" });
            }
            "pin" => {
                let mut it = arg.unwrap_or("").split_whitespace();
                let n: usize = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| anyhow!("usage: /pin <n> [off] (n from /history)"))?;
                let pin = it.next() != Some("off");
                let (peer, shown) = self.shown.borrow().clone();
                let peer =
                    peer.ok_or_else(|| anyhow!("run /history first; /pin uses its numbers"))?;
                let e = shown
                    .get(n.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("no message {n} in the last /history"))?;
                if !self.node.pin(&peer, e.message_id(), pin) {
                    bail!("that message can't be pinned (gone, or too old to have an id)");
                }
                say!("* {}", if pin { "pinned" } else { "unpinned" });
            }
            "pins" => {
                let peer = self
                    .current
                    .ok_or_else(|| anyhow!("no current peer; /to <peer>"))?;
                let pins = self.node.pinned(self.node.conversation_for(&peer));
                if pins.is_empty() {
                    say!("No pinned messages.");
                }
                for e in pins {
                    say!(
                        "  📌 [{}] {}",
                        clock(e.at_ms),
                        threnody_core::history::snippet(&e)
                    );
                }
            }
            "reply" => {
                let (n, body) = arg
                    .and_then(|a| a.split_once(' '))
                    .ok_or_else(|| anyhow!("usage: /reply <n> <text> (n from /history)"))?;
                let n: usize = n.parse().map_err(|_| anyhow!("usage: /reply <n> <text>"))?;
                let (peer, shown) = self.shown.borrow().clone();
                let peer =
                    peer.ok_or_else(|| anyhow!("run /history first; /reply uses its numbers"))?;
                let e = shown
                    .get(n.wrapping_sub(1))
                    .ok_or_else(|| anyhow!("no message {n} in the last /history"))?;
                let quote = threnody_core::message::Quote::of(e);
                self.node.send_reply(&peer, body.trim(), Some(quote))?;
            }
            "edit" => {
                let (n, body) = arg
                    .and_then(|a| a.split_once(' '))
                    .ok_or_else(|| anyhow!("usage: /edit <n> <new text> (n from /history)"))?;
                let n: usize = n
                    .parse()
                    .map_err(|_| anyhow!("usage: /edit <n> <new text>"))?;
                let (peer, shown) = self.shown.borrow().clone();
                let peer =
                    peer.ok_or_else(|| anyhow!("run /history first; /edit uses its numbers"))?;
                let e = shown
                    .get(n.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| anyhow!("no message {n} in the last /history"))?;
                if !e.outgoing || e.file.is_some() || e.local_id == 0 {
                    bail!("only your own text messages can be edited");
                }
                if !self.node.edit_message(&peer, e.local_id, body.trim()) {
                    bail!("that message is gone");
                }
                say!("* edited");
            }
            "del" => {
                let mut it = arg.unwrap_or("").split_whitespace();
                let n: usize = it
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| anyhow!("usage: /del <n> [all] (n from /history)"))?;
                let everyone = it.next() == Some("all");
                // Exactly the message the last /history numbered `n`.
                let (peer, shown) = self.shown.borrow().clone();
                let peer =
                    peer.ok_or_else(|| anyhow!("run /history first; /del uses its numbers"))?;
                let conv = self.node.conversation_for(&peer);
                let e = shown
                    .get(n.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| anyhow!("no message {n} in the last /history"))?;
                if everyone && !e.outgoing {
                    bail!("only your own messages can be deleted for everyone");
                }
                let e = &e;
                let gone = if e.message_id() != 0 {
                    self.node
                        .delete_messages(&peer, &[e.message_id()], everyone)
                } else {
                    self.node.delete_entry(conv, e.at_ms, e.device)
                };
                say!(
                    "* deleted {gone} message(s){}",
                    if everyone {
                        " here and for everyone who supports it"
                    } else {
                        ""
                    }
                );
            }
            "requests" => {
                let pending: Vec<_> = self
                    .node
                    .contacts()
                    .iter()
                    .filter(|c| !c.accepted && !c.blocked && !self.node.is_own_device(&c.key))
                    .map(|c| c.key)
                    .collect();
                if pending.is_empty() {
                    say!("  no message requests");
                }
                for p in pending {
                    let n = self
                        .node
                        .history(self.node.conversation_for(&p))
                        .map_or(0, |h| h.entries().len());
                    say!("  {}  {n} message(s)", self.name(&p));
                }
            }
            "accept" | "block" | "delete" => {
                let p = self.resolve_peer(arg)?;
                let who = self.name(&p);
                match verb {
                    "accept" => {
                        self.node.accept_contact(&p);
                        say!("* accepted {who}; their messages show normally now");
                        self.show_history(p, 20)?;
                    }
                    "block" => {
                        self.node.block_contact(&p);
                        say!(
                            "* blocked {who}: no sessions or messages from them, conversation deleted"
                        );
                    }
                    _ => {
                        self.node.delete_request(&p);
                        say!("* deleted the request from {who}");
                    }
                }
            }
            "approve" | "revoke" => {
                let p = self.resolve_peer(arg)?;
                self.node.set_approval(&p, verb == "approve")?;
                say!(
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
                say!(
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
                say!("* marked {} verified", self.name(&p));
            }
            "file" => {
                let arg = arg.ok_or_else(|| anyhow!("usage: /file [-s] <path>... [| caption]"))?;
                let peer = self.current.ok_or_else(|| anyhow!("no current peer"))?;
                let files = read_files(arg).await?;
                let (n, len) = (
                    files.len(),
                    files.iter().map(|f| f.data.len()).sum::<usize>(),
                );
                for f in files {
                    self.node.send_file(&peer, f)?;
                }
                say!("* sent {n} file(s), {len} bytes, to {}", self.name(&peer));
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
                say!("* policy: {p:?}");
            }
            "wifi-direct" | "wd" => match arg.unwrap_or("request") {
                "request" => {
                    let peer = self.current.ok_or_else(|| anyhow!("no current peer"))?;
                    if !self.wifi_direct {
                        say!("  note: run with --wifi-direct to join the group offered back");
                    }
                    self.node.request_wifi_direct(&peer)?;
                    say!("* asked {} for a Wi-Fi Direct link", self.name(&peer));
                }
                "leave" => self.leave_wifi_direct().await,
                _ => bail!("usage: /wifi-direct [request|leave]"),
            },
            "status" => self.status(),
            "dir" => self.dir_command(arg).await?,
            "directory" => self.directory_command(arg)?,
            "volunteer" => match arg {
                None | Some("") => say!(
                    "* {}",
                    if self.node.volunteering() {
                        "volunteering as a relay"
                    } else {
                        "not volunteering"
                    }
                ),
                Some("off") => {
                    self.node.set_volunteer(None)?;
                    say!("* no longer volunteering");
                }
                Some(a) => {
                    let addrs: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
                    self.node.set_volunteer(Some(addrs.clone()))?;
                    say!("* volunteering as a relay at {}", addrs.join(", "));
                }
            },
            "vonion" => {
                let q =
                    arg.ok_or_else(|| anyhow!("usage: /vonion <contact|fingerprint|invite>"))?;
                let dest = self.parse_destination(q)?;
                let addr = crate::target::invite_addr(q).or_else(|| {
                    self.node
                        .contacts()
                        .iter()
                        .find(|c| c.key.fingerprint() == dest)
                        .and_then(|c| c.last_addr.clone())
                });
                let addr = addr.ok_or_else(|| anyhow!("no address known for {dest}"))?;
                say!("* building a circuit to {dest} through volunteer relays");
                let node = self.node.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.connect_volunteer(dest, &addr).await {
                        say!("! volunteer circuit to {dest}: {e:#}");
                    }
                });
            }
            "cred" => self.cred_command(arg)?,
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
            "search" | "find" => self.search(arg.unwrap_or(""))?,
            "profile" => self.profile_command(arg)?,
            "share" => {
                // `/share`, `/share <peer>`, `/share <peer> <keys|none>`, or
                // `/share <keys|none>` for the current peer.
                let words: Vec<&str> = arg
                    .map(|a| a.split_whitespace().collect())
                    .unwrap_or_default();
                let mine = self.node.profile();
                let is_keys =
                    |w: &str| w == "none" || w.split(',').all(|k| mine.iter().any(|(m, _)| m == k));
                let (peer, keys) = match words.as_slice() {
                    [] => (self.resolve_peer(None)?, None),
                    [w] if is_keys(w) => (self.resolve_peer(None)?, Some(*w)),
                    [p] => (self.resolve_peer(Some(p))?, None),
                    [p, k] => (self.resolve_peer(Some(p))?, Some(*k)),
                    _ => bail!("usage: /share [peer] [key,key,…|none]"),
                };
                if let Some(k) = keys {
                    let keys: Vec<String> = if k == "none" {
                        Vec::new()
                    } else {
                        k.split(',').map(str::to_owned).collect()
                    };
                    if let Some(bad) = keys.iter().find(|k| !mine.iter().any(|(m, _)| m == *k)) {
                        bail!("your profile has no {bad:?}; /profile set {bad} <value> first");
                    }
                    self.node.set_shared_with(&peer, &keys)?;
                }
                let shared = self.node.shared_with(&peer);
                if shared.is_empty() {
                    say!("* {} sees none of your profile", self.name(&peer));
                } else {
                    say!("* {} sees your {}", self.name(&peer), shared.join(", "));
                }
            }
            "reveal" => {
                let main = self.main_identity.as_ref().ok_or_else(|| {
                    anyhow!("/reveal is for anonymous identities (threnody --persona <id> run)")
                })?;
                let mut words = arg.unwrap_or("").split_whitespace();
                let first = words.next();
                // `/reveal host:port` (current peer) or `/reveal <peer> [host:port]`.
                let (peer, addr) = match (first, words.next()) {
                    (Some(a), None) if a.contains(':') => (self.resolve_peer(None)?, Some(a)),
                    (p, a) => (self.resolve_peer(p)?, a),
                };
                let proof = threnody_core::persona::LinkProof::sign(main, &self.node.identity());
                let invite =
                    addr.map(|a| crate::target::invite_link(&main.public().fingerprint(), a));
                self.node.reveal(&peer, proof, invite)?;
                say!(
                    "* revealed to {} that you are {}. This can't be taken back.",
                    self.name(&peer),
                    main.public().fingerprint()
                );
            }
            "clear" => {
                let p = self.resolve_peer(arg)?;
                self.node.clear_conversation(&p);
                say!(
                    "* cleared your messages with {}; they're still a contact",
                    self.name(&p)
                );
            }
            "forget" => {
                let p =
                    self.resolve_peer(Some(arg.ok_or_else(|| anyhow!("usage: /forget <peer>"))?))?;
                let name = self.name(&p);
                self.node.delete_conversation(&p);
                if self.current == Some(p) {
                    self.current = None;
                }
                say!(
                    "* deleted {name} and your conversation; they can write again as a new request"
                );
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
                    Some(t) => say!(
                        "* new messages with {} disappear after {}",
                        self.name(&p),
                        human_secs(t)
                    ),
                    None => say!("* disappearing messages off for {}", self.name(&p)),
                }
            }
            "device" => {
                let a = arg.unwrap_or("");
                match a.split_once(' ').map_or((a, ""), |(x, y)| (x, y.trim())) {
                    ("add", given) => {
                        let listen = self.listen_addr.ok_or_else(|| {
                            anyhow!("run with --listen so the new device can reach this one")
                        })?;
                        // The code carries an address the new device dials:
                        // the one given, else ours on the LAN.
                        let addr = if given.is_empty() {
                            reachable(listen).to_string()
                        } else {
                            given.to_owned()
                        };
                        let code = self.node.create_link_code(addr);
                        say!("  On the new device run:\n\n    threnody link '{code}'\n");
                        if let Ok(qr) = qrcode::QrCode::new(code.to_string().as_bytes()) {
                            let art = qr
                                .render::<qrcode::render::unicode::Dense1x2>()
                                .dark_color(qrcode::render::unicode::Dense1x2::Light)
                                .light_color(qrcode::render::unicode::Dense1x2::Dark)
                                .build();
                            say!("{art}");
                        }
                        say!(
                            "  The code works once, for 10 minutes. Never share it with anyone else."
                        );
                    }
                    ("rename", rest) if rest.contains(' ') => {
                        let (who, name) = rest.split_once(' ').unwrap_or_default();
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
                        self.node.rename_device(&target, name)?;
                        say!("* renamed {who} to {:?}", name.trim());
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
                        say!("* removed {} from your account", target.fingerprint());
                    }
                    _ => {
                        bail!("usage: /device add [host:port] | /device remove <name|fingerprint>")
                    }
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
                say!("* building an onion circuit to {dest} ({min}+ relays)");
                if min < 2 {
                    say!("  note: with one relay, that relay sees both ends");
                }
                let node = self.node.clone();
                tokio::spawn(async move {
                    if let Err(e) = node.connect_onion(dest, min).await {
                        say!("! onion to {dest}: {e:#}");
                    }
                });
            }
            "relay" => {
                let q = arg.ok_or_else(|| anyhow!("usage: /relay <contact|fingerprint|invite>"))?;
                let dest = self.parse_destination(q)?;
                say!("* looking for a relay path to {dest}");
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
            "ghistory" => {
                let node = self.node.clone();
                self.groups.history(
                    &self.node,
                    &|p: &PublicIdentity| name_of(&node, p),
                    arg.ok_or_else(|| anyhow!("usage: /ghistory <group> [count]"))?,
                )?;
            }
            "gpoll" => self.groups.poll(
                &self.node,
                arg.ok_or_else(|| anyhow!("usage: /gpoll <group> <question> | <option>..."))?,
            )?,
            "gvote" => self.groups.vote(
                &self.node,
                arg.ok_or_else(|| anyhow!("usage: /gvote <group> <n> [choice...]"))?,
            )?,
            "gr" => {
                self.groups.reply(
                    &self.node,
                    arg.ok_or_else(|| anyhow!("usage: /gr <group> <n> <text>"))?,
                )?;
            }
            "g" => {
                self.groups.say(
                    &self.node,
                    arg.ok_or_else(|| anyhow!("usage: /g <group> <text>"))?,
                )?;
            }
            "gfile" => {
                let (n, len) = self
                    .groups
                    .send_file(
                        &self.node,
                        arg.ok_or_else(|| {
                            anyhow!("usage: /gfile <group> [-s] <path>... [| caption]")
                        })?,
                    )
                    .await?;
                say!("* sent {n} file(s), {len} bytes, to the group");
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
                say!("* scanning Bluetooth LE for {secs}s…");
                tokio::spawn(async move {
                    match crate::ble::scan(secs).await {
                        Ok(found) => {
                            if found.is_empty() {
                                say!("* no Threnody devices nearby");
                            }
                            for (i, f) in found.iter().enumerate() {
                                say!(
                                    "  {}: {} {} psm {} rssi {}",
                                    i + 1,
                                    f.addr,
                                    f.name.as_deref().unwrap_or("-"),
                                    f.psm,
                                    f.rssi.map_or("?".into(), |r| r.to_string())
                                );
                            }
                            if !found.is_empty() {
                                say!("  /ble connect <n> to open a session");
                            }
                            *seen
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = found;
                        }
                        Err(e) => say!("! Bluetooth scan: {e:#}"),
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
                    say!("! no such device; /ble scan first");
                    return;
                };
                let node = self.node.clone();
                say!(
                    "* connecting to {} over Bluetooth LE (psm {})…",
                    f.addr,
                    f.psm
                );
                tokio::spawn(async move {
                    if let Err(e) = crate::ble::connect(&node, &f, None).await {
                        say!("! Bluetooth connect: {e:#}");
                    }
                });
            }
            _ => say!("! usage: /ble scan [secs] | /ble connect <n|address>"),
        }
    }

    fn devices(&self) {
        let account = self.node.account();
        let me = self.node.identity();
        say!("  account {}", account.id().fingerprint());
        for (d, name) in &account.state().devices {
            let tag = if *d == me { " (this device)" } else { "" };
            let live = if self.node.sessions().iter().any(|s| s.peer == *d) {
                ", connected"
            } else {
                ""
            };
            say!("    {name:<16} {}{tag}{live}", d.fingerprint());
        }
        for d in &account.state().removed {
            say!("    removed          {}", d.fingerprint());
        }
    }

    /// Spec §10: persistent indicators of transport, tunnel and protection.
    fn status(&self) {
        let sessions = self.node.sessions();
        let account = self.node.account();
        say!(
            "  account      {} ({} device(s))",
            account.id().fingerprint(),
            account.state().devices.len()
        );
        say!("  this device  {}", self.node.identity().fingerprint());
        match (self.listen_addr, self.node.quic_addr()) {
            (Some(a), Some(q)) => say!("  listening    tcp {a}, quic {q}"),
            (Some(a), None) => say!("  listening    tcp {a}"),
            _ => say!("  listening    no"),
        }
        let r = self.node.reachability();
        if r.enabled {
            let cands: Vec<String> = r.candidates.iter().map(|c| c.addr.to_string()).collect();
            say!(
                "  internet     {}; addresses {}{}{}",
                if r.online {
                    "in the DHT"
                } else {
                    "joining the DHT"
                },
                if cands.is_empty() {
                    "unknown yet".into()
                } else {
                    cands.join(", ")
                },
                if r.symmetric {
                    "; symmetric NAT (direct paths unlikely)"
                } else {
                    ""
                },
                if r.punching.is_empty() {
                    String::new()
                } else {
                    format!("; punching to {} contact(s)", r.punching.len())
                }
            );
        } else {
            say!("  internet     off (--no-rendezvous, or an anonymous identity)");
        }
        say!("  policy       {:?}", self.node.policy());
        say!(
            "  transports   {}",
            if sessions.is_empty() {
                "none active".into()
            } else {
                let mut kinds: Vec<&str> = sessions.iter().map(|s| s.transport).collect();
                kinds.sort_unstable();
                kinds.dedup();
                format!("{} ({} session(s))", kinds.join(", "), sessions.len())
            }
        );
        let held = self.node.held_messages();
        say!("  mailbox      holding {held} sealed message(s) for others");
        match self.discovery {
            Some(a) => say!(
                "  discovery    lan beacons on udp/{} (approved peers only)",
                a.port()
            ),
            None => say!("  discovery    off"),
        }
        match &self.tunnels {
            Some(t) => say!("  tunnel       {}", t.summary()),
            None => say!("  tunnel       off (run with --tunnel)"),
        }
        let rate = match self.node.constant_rate() {
            Some(d) => format!("cover traffic every {} ms", d.as_millis()),
            None => "off (--no-cover)".into(),
        };
        let onion = if self.node.prefer_onion() {
            "onion circuits first when two approved relays allow"
        } else {
            "direct first (--no-onion)"
        };
        match self.node.default_timer() {
            Some(t) => say!(
                "  disappear    after {} unless a chat says otherwise (/disappear)",
                human_secs(t)
            ),
            None => say!("  disappear    off by default (--disappear-default)"),
        }
        say!(
            "  metadata     padding: on; timing: {rate}; routing: {onion}; carrying {} circuit(s) for others",
            self.node.onion_hops()
        );
        say!(
            "  images       {}",
            if self.node.strip_metadata() {
                "metadata stripped before sending"
            } else {
                "sent with their metadata (--keep-metadata)"
            }
        );
    }
}

/// Reads the files named by `/file` or `/gfile` arguments:
/// `[-s] <path>... [| caption]`. A whole argument that names a file is one
/// path (spaces and all); otherwise paths are split on spaces. Several
/// files go as an album, the caption with the first.
pub(crate) async fn read_files(arg: &str) -> Result<Vec<OutgoingFile>> {
    let (paths, caption) = match arg.split_once('|') {
        Some((p, c)) => (p.trim(), c.trim()),
        None => (arg.trim(), ""),
    };
    let (sensitive, paths) = match paths.strip_prefix("-s ") {
        Some(rest) => (true, rest.trim()),
        None => (false, paths),
    };
    let paths: Vec<&str> = if Path::new(paths).is_file() {
        vec![paths]
    } else {
        paths.split_whitespace().collect()
    };
    if paths.is_empty() {
        bail!("usage: [-s] <path>... [| caption]");
    }
    let album = if paths.len() > 1 {
        threnody_net::history::album_id()
    } else {
        0
    };
    let mut out = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let data = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {path}"))?;
        out.push(OutgoingFile {
            name: Path::new(path)
                .file_name()
                .map_or("file".into(), |n| n.to_string_lossy().into_owned()),
            data,
            location: std::fs::canonicalize(path)
                .ok()
                .map(|p| p.display().to_string()),
            sensitive,
            clip: None,
            caption: if i == 0 {
                caption.to_owned()
            } else {
                String::new()
            },
            album,
            forwarded: false,
        });
    }
    Ok(out)
}

/// Saves a received file under `dir` using only its final path component,
/// never overwriting an existing file.
/// What to say after a received file's name: whether it is sensitive,
/// and whether it is a voice or video message (and how long).
pub(crate) fn file_mark(sensitive: bool, clip: Option<Clip>) -> String {
    let mut mark = String::new();
    if let Some(c) = clip {
        let kind = if c.video { "video" } else { "voice" };
        let s = c.duration_ms / 1000;
        mark += &format!(" [{kind} message, {}:{:02}]", s / 60, s % 60);
    }
    if sensitive {
        mark += " [sensitive]";
    }
    mark
}

pub(crate) fn save_download(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf> {
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
pub fn parse_duration(a: &str) -> Option<u32> {
    let a = a.trim();
    let (num, mult) = match a.chars().last()? {
        's' => (&a[..a.len() - 1], 1),
        'm' => (&a[..a.len() - 1], 60),
        'h' => (&a[..a.len() - 1], 3600),
        'd' => (&a[..a.len() - 1], 86_400),
        'w' => (&a[..a.len() - 1], 7 * 86_400),
        c if c.is_ascii_digit() => (a, 1),
        _ => return None,
    };
    num.parse::<u32>()
        .ok()?
        .checked_mul(mult)
        .filter(|s| *s > 0)
}

pub fn human_secs(s: u32) -> String {
    match s {
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// `HH:MM` (UTC) for a Unix-ms time.
/// An address other devices can dial for `listen`: if it is a wildcard
/// (0.0.0.0 / ::), this machine's address on the network its default route
/// uses (found without sending anything).
fn reachable(listen: std::net::SocketAddr) -> std::net::SocketAddr {
    if !listen.ip().is_unspecified() {
        return listen;
    }
    let probe = if listen.is_ipv4() {
        "192.0.2.1:9"
    } else {
        "[2001:db8::1]:9"
    };
    std::net::UdpSocket::bind(std::net::SocketAddr::new(listen.ip(), 0))
        .and_then(|s| s.connect(probe).and_then(|()| s.local_addr()))
        .map_or(listen, |a| std::net::SocketAddr::new(a.ip(), listen.port()))
}

/// `HH:MM` in local time, with the date if it isn't today.
pub(crate) fn clock(ms: u64) -> String {
    let tz = jiff::tz::TimeZone::system();
    let Ok(t) = jiff::Timestamp::from_millisecond(i64::try_from(ms).unwrap_or(i64::MAX)) else {
        return "?".into();
    };
    let t = t.to_zoned(tz.clone());
    let today = jiff::Timestamp::now().to_zoned(tz).date();
    if t.date() == today {
        t.strftime("%H:%M").to_string()
    } else {
        t.strftime("%b %-d %H:%M").to_string()
    }
}

#[cfg(test)]
mod duration_tests {
    use super::*;

    #[test]
    fn link_codes_get_a_dialable_address() {
        let any: std::net::SocketAddr = "0.0.0.0:7450".parse().unwrap();
        let r = reachable(any);
        assert_eq!(r.port(), 7450);
        // Without any network the wildcard comes back unchanged.
        assert!(!r.ip().is_unspecified() || r == any);
        let lo: std::net::SocketAddr = "127.0.0.1:7450".parse().unwrap();
        assert_eq!(reachable(lo), lo);
    }

    #[test]
    fn durations_parse_and_print() {
        assert_eq!(parse_duration("30s"), Some(30));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("1w"), Some(604_800));
        assert_eq!(parse_duration("45"), Some(45));
        assert_eq!(parse_duration("0s"), None);
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(human_secs(7200), "2h");
        assert_eq!(human_secs(90), "90s");
    }
}

/// Opens the QUIC endpoint on the TCP listener's port: on `[::]` (IPv4 and
/// IPv6) when TCP listens on every address, else on the same address.
async fn listen_quic(node: &Node, tcp: std::net::SocketAddr) -> Option<std::net::SocketAddr> {
    let first = if tcp.ip().is_unspecified() {
        format!("[::]:{}", tcp.port())
    } else {
        tcp.to_string()
    };
    let fallback = format!("0.0.0.0:{}", tcp.port());
    for a in [first, fallback] {
        if let Ok(q) = node.listen_quic(&a).await {
            return Some(q);
        }
    }
    say!("! QUIC unavailable on udp/{}", tcp.port());
    None
}

/// The first bytes of a value in hex, for display.
fn hex8(b: &[u8]) -> String {
    b.iter().take(8).map(|x| format!("{x:02x}")).collect()
}

/// A local time for display.
fn fmt_time(ms: u64) -> String {
    jiff::Timestamp::from_millisecond(i64::try_from(ms).unwrap_or(0))
        .map(|t| {
            t.to_zoned(jiff::tz::TimeZone::system())
                .strftime("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// A quote as a line above the reply: `> who: text`.
pub(crate) fn quoted(
    q: &threnody_core::message::Quote,
    name: &dyn Fn(&PublicIdentity) -> String,
) -> String {
    let who = PublicIdentity::from_bytes(&q.author).map_or_else(|_| "?".into(), |d| name(&d));
    if q.text.is_empty() {
        format!("> {who}: (message gone)")
    } else {
        format!("> {who}: {}", q.text)
    }
}
