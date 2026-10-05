//! Interactive session: one node, many peers, line-oriented UI.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use threnody_core::history::FileNote;
use threnody_core::store::Home;
use threnody_core::{AppMessage, Fingerprint, Identity, PublicIdentity, safety_number};
use threnody_net::history::OutgoingFile;
use threnody_net::mailbox::DepositStatus;
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
  /requests   /accept <peer>   /block <peer>   /delete <peer>   message requests
  /del <n> [all]                        delete message n of the last /history (all: for everyone)
  /edit <n> <text>                      edit your message n of the last /history
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
  /devices   /device add [host:port]   /device rename <name> <new>   /device remove <name>
  /ble scan [secs]   /ble connect <n|address>   Bluetooth LE (Linux)
  /wifi-direct [request|leave]          ask the current peer for a Wi-Fi Direct link
  /history [peer] [n]                   recent messages (stored encrypted)
  /disappear <30s|10m|1h|1d|off>        disappearing messages with the current peer
  /profile [set <key> <value> | unset <key>]   your profile (shared with no one by default)
  /share [peer] [key,key,…|none]        which profile details a contact sees
  /reveal [peer] [host:port]            (anonymous identity) prove to them who you are
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
    /// What the last /history showed, numbered from 1: /del refers to it.
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
    node.set_strip_metadata(opts.strip_metadata);
    node.set_default_timer(opts.default_timer);
    if opts.main_identity.is_some() {
        println!(
            "Threnody — anonymous identity {}. Nothing links it to your main one unless you /reveal it.",
            node.identity().fingerprint()
        );
    } else {
        println!("Threnody — you are {}", node.identity().fingerprint());
    }
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
    let stopped = stop_signal();
    tokio::pin!(stopped);
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
            AppMessage::File {
                name,
                data,
                id,
                sensitive,
                caption,
                album,
                ..
            } => {
                let saved = save_download(&self.downloads, &name, &data);
                let mark = if sensitive { " [sensitive]" } else { "" };
                match &saved {
                    Ok(p) => println!(
                        "* {who} sent {name}{mark} ({} bytes) -> {}",
                        data.len(),
                        p.display()
                    ),
                    Err(e) => println!("! could not save file from {who}: {e:#}"),
                }
                if !caption.is_empty() {
                    println!("<{who}> {caption}");
                }
                self.node.record_received_file(
                    &peer,
                    FileNote {
                        name,
                        size: data.len() as u64,
                        location: saved.ok().map(|p| p.display().to_string()),
                        sensitive,
                        album,
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
        if let Some(t) = self.node.timer(&peer) {
            println!("  (messages disappear after {})", human_secs(t));
        }
        if h.entries().is_empty() {
            println!("  no history with {}", self.name(&peer));
        }
        *self.shown.borrow_mut() = (Some(peer), h.recent(n).to_vec());
        for (i, e) in h.recent(n).iter().enumerate() {
            print!("{:>3}", i + 1);
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
            match &e.file {
                Some(f) => println!(
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
                None => println!("  [{}] <{who}> {}{mark}", clock(e.at_ms), e.text),
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
            println!("* your profile is empty. /profile set name <your name>");
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
            println!("  {k}: {v}  ({shown})");
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
            Event::MessageEdited { peer, .. } => {
                println!(
                    "* {} edited a message (/history to see it)",
                    self.name(&peer)
                );
            }
            Event::MessagesDeleted { peer, count } => {
                println!("* {} deleted {count} message(s)", self.name(&peer));
            }
            Event::MessageRequest { peer, msg } => {
                let what = match &msg {
                    AppMessage::Text { body, .. } => format!("{body:?}"),
                    AppMessage::File { name, .. } => format!("a file ({name})"),
                    _ => "a message".into(),
                };
                println!(
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
            Event::Delivered { .. } => {}
            Event::Reacted { peer, .. } => {
                println!("* {} reacted (see /history)", self.name(&peer));
            }
            Event::ProfileChanged { peer } => {
                let shown = self
                    .node
                    .contacts()
                    .get(&peer)
                    .map(|c| c.profile.clone())
                    .unwrap_or_default();
                if shown.is_empty() {
                    println!(
                        "* {} no longer shares any profile details",
                        self.name(&peer)
                    );
                } else {
                    let list: Vec<String> =
                        shown.iter().map(|(k, v)| format!("{k}: {v}")).collect();
                    println!("* {} shares {}", self.name(&peer), list.join(", "));
                }
            }
            Event::IdentityRevealed {
                peer,
                identity,
                invite,
            } => {
                println!(
                    "* {} revealed who they are: {} (proven by its signature)",
                    self.name(&peer),
                    identity.fingerprint()
                );
                match invite {
                    Some(i) => println!("  to add them: /connect {i}"),
                    None => println!(
                        "  they gave no address; add them when you meet: {}",
                        identity.fingerprint()
                    ),
                }
            }
            Event::HistorySynced { from, added } => {
                println!(
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
                    DepositStatus::Held => println!("* {m} is holding your message for {t}{how}"),
                    DepositStatus::Delivered => {
                        println!("* {m} delivered your message to {t}{how}")
                    }
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
                // Did we verify any device of this account? Then a new one is a
                // key we haven't checked in a conversation we trusted.
                let verified_before = account != self.node.account().id()
                    && self.node.contacts().iter().any(|c| {
                        c.account == Some(account) && c.verified && !added.contains(&c.key)
                    });
                for d in added {
                    if verified_before {
                        println!(
                            "! SAFETY: account {} added device {} that you haven't verified — /safety {} before trusting it",
                            &a[..9],
                            self.name(&d),
                            &d.fingerprint().to_string()[..9]
                        );
                    } else {
                        println!("* account {} added device {}", &a[..9], self.name(&d));
                    }
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
                println!("* {} {emoji}", if add { "reacted" } else { "took back" });
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
                println!("* edited");
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
                println!(
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
                    println!("  no message requests");
                }
                for p in pending {
                    let n = self
                        .node
                        .history(self.node.conversation_for(&p))
                        .map_or(0, |h| h.entries().len());
                    println!("  {}  {n} message(s)", self.name(&p));
                }
            }
            "accept" | "block" | "delete" => {
                let p = self.resolve_peer(arg)?;
                let who = self.name(&p);
                match verb {
                    "accept" => {
                        self.node.accept_contact(&p);
                        println!("* accepted {who}; their messages show normally now");
                        self.show_history(p, 20)?;
                    }
                    "block" => {
                        self.node.block_contact(&p);
                        println!(
                            "* blocked {who}: no sessions or messages from them, conversation deleted"
                        );
                    }
                    _ => {
                        self.node.delete_request(&p);
                        println!("* deleted the request from {who}");
                    }
                }
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
                println!("* sent {n} file(s), {len} bytes, to {}", self.name(&peer));
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
                    println!("* {} sees none of your profile", self.name(&peer));
                } else {
                    println!("* {} sees your {}", self.name(&peer), shared.join(", "));
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
                println!(
                    "* revealed to {} that you are {}. This can't be taken back.",
                    self.name(&peer),
                    main.public().fingerprint()
                );
            }
            "clear" => {
                let p = self.resolve_peer(arg)?;
                self.node.clear_conversation(&p);
                println!(
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
                println!(
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
                        println!("* renamed {who} to {:?}", name.trim());
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
                println!("* sent {n} file(s), {len} bytes, to the group");
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
            Some(d) => format!("cover traffic every {} ms", d.as_millis()),
            None => "off (--no-cover)".into(),
        };
        let onion = if self.node.prefer_onion() {
            "onion circuits first when two approved relays allow"
        } else {
            "direct first (--no-onion)"
        };
        match self.node.default_timer() {
            Some(t) => println!(
                "  disappear    after {} unless a chat says otherwise (/disappear)",
                human_secs(t)
            ),
            None => println!("  disappear    off by default (--disappear-default)"),
        }
        println!(
            "  metadata     padding: on; timing: {rate}; routing: {onion}; carrying {} circuit(s) for others",
            self.node.onion_hops()
        );
        println!(
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
            caption: if i == 0 {
                caption.to_owned()
            } else {
                String::new()
            },
            album,
        });
    }
    Ok(out)
}

/// Saves a received file under `dir` using only its final path component,
/// never overwriting an existing file.
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
fn clock(ms: u64) -> String {
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
