//! The nodes behind the window: the main identity's and one per anonymous
//! identity, each with a thread draining its events. Received files are
//! saved here, approved contacts redialed, and everything the window
//! needs to hear about is passed to it over a channel.
//!
//! Every `ThrenodyNode` call blocks, so the window calls into this from
//! worker threads (see `ui::bg`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use threnody_ffi::{
    ContactInfo, FileOptions, GroupInfo, GroupInvite, HistoryEntry, NodeEvent, PersonaRecord,
    ThrenodyNode,
};

use crate::keyring;
use crate::settings::{self, Settings};

pub type Node = Arc<ThrenodyNode>;

/// The port the main identity listens on, as on Android and in the CLI.
pub const PORT: u16 = 7450;

/// What the window hears from the nodes.
pub enum UiEvent {
    /// `persona` is the anonymous identity it concerns (`None`: the main one).
    Node {
        persona: Option<String>,
        event: NodeEvent,
    },
    Log(String),
    /// An anonymous identity was created, burned or renamed.
    PersonasChanged,
}

/// How a new identity is protected.
pub enum Protect {
    /// A random key kept in the system keyring (the default).
    Keyring,
    Passphrase(String),
    /// Stored unprotected: only when the user explicitly chose it.
    Nothing,
}

/// Why the main identity didn't open.
pub enum OpenError {
    /// It is sealed and the keyring can't open it: ask for the passphrase.
    NeedPassphrase,
    /// A new identity, and there is no keyring to seal it with.
    NoKeyring(String),
    Failed(String),
}

/// Opens (creating on first use) the identity in `home`. An existing plain
/// identity is sealed with a keyring key first, when there is a keyring.
pub fn open_main(
    home: &Path,
    protect: Option<Protect>,
    passphrase: Option<String>,
) -> Result<Node, OpenError> {
    let dir = home.display().to_string();
    let store = threnody_core::store::Home::new(home);
    let failed = |e: threnody_ffi::ThrenodyError| OpenError::Failed(e.to_string());
    if !store.has_identity() {
        let pw = match protect.unwrap_or(Protect::Keyring) {
            Protect::Keyring => match keyring::create(home) {
                Ok(pw) => Some(pw.0.clone()),
                Err(e) => return Err(OpenError::NoKeyring(e)),
            },
            Protect::Passphrase(p) => Some(p),
            Protect::Nothing => None,
        };
        return ThrenodyNode::open(dir, pw, None).map_err(failed);
    }
    if !threnody_ffi::identity_is_sealed(dir.clone()).map_err(failed)? {
        // Seal a plain identity, as `threnody keyring on` would.
        if let Ok(pw) = keyring::create(home) {
            if threnody_ffi::change_passphrase(dir.clone(), None, Some(pw.0.clone())).is_err() {
                keyring::delete(home);
                return ThrenodyNode::open(dir, None, None).map_err(failed);
            }
            return ThrenodyNode::open(dir, Some(pw.0.clone()), None).map_err(failed);
        }
        return ThrenodyNode::open(dir, None, None).map_err(failed);
    }
    if let Some(pw) = passphrase {
        return ThrenodyNode::open(dir, Some(pw), None).map_err(|e| {
            OpenError::Failed(if e.to_string().to_lowercase().contains("decrypt") {
                "Wrong passphrase".into()
            } else {
                e.to_string()
            })
        });
    }
    match keyring::get(home) {
        Some(pw) => {
            ThrenodyNode::open(dir, Some(pw.0.clone()), None).map_err(|_| OpenError::NeedPassphrase)
        }
        None => Err(OpenError::NeedPassphrase),
    }
}

/// A conversation: a contact's account (all its devices) or a group.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChatRef {
    pub persona: Option<String>,
    pub target: Target,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// The conversation key (account fingerprint, else the device's).
    Contact(String),
    Group(String),
}

/// One row of the conversation list, with what the chat needs to know.
#[derive(Clone, Debug)]
pub struct Conversation {
    pub chat: ChatRef,
    pub title: String,
    /// The device to address; sends reach every device of the account.
    pub device: String,
    pub devices: Vec<String>,
    pub connected: bool,
    pub approved: bool,
    pub verified: bool,
    pub any_verified: bool,
    pub unverified: Vec<String>,
    pub accepted: bool,
    pub revealed: Option<String>,
    pub revealed_invite: Option<String>,
    pub group: Option<GroupInfo>,
    /// A group invitation waiting for an answer.
    pub invite: Option<GroupInvite>,
    pub last: Option<HistoryEntry>,
}

impl Conversation {
    pub fn is_request(&self) -> bool {
        self.group.is_none() && self.invite.is_none() && !self.accepted
    }

    pub fn sort_key(&self) -> (u8, u64) {
        let pinned = if self.invite.is_some() || self.is_request() {
            1
        } else {
            0
        };
        (pinned, self.last.as_ref().map_or(0, |e| e.at_ms))
    }
}

pub struct Persona {
    pub node: Node,
    pub label: String,
    pub home: PathBuf,
    pub port: u16,
    alive: Arc<AtomicBool>,
}

pub struct Core {
    pub home: PathBuf,
    pub main: Node,
    pub port: u16,
    personas: Mutex<HashMap<String, Persona>>,
    settings: Mutex<Settings>,
    log: Mutex<String>,
    ui: async_channel::Sender<UiEvent>,
    metered: AtomicBool,
    redialing: Mutex<HashSet<Option<String>>>,
    /// The conversation on screen while the window is focused; its
    /// messages don't notify.
    pub watching: Mutex<Option<ChatRef>>,
    /// Holds the advisory lock on the data directory.
    _lock: std::fs::File,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Core {
    /// Starts the main node: listening, redialing, its anonymous
    /// identities, and the event threads.
    pub fn start(
        home: PathBuf,
        main: Node,
        lock_file: std::fs::File,
        ui: async_channel::Sender<UiEvent>,
    ) -> Arc<Self> {
        // When the usual port is taken (the CLI running too), keep the one
        // used last time, so invites already given out keep working.
        let fallback = Settings::load().number("port").unwrap_or(0);
        let port = main
            .listen(format!("0.0.0.0:{PORT}"))
            .or_else(|_| main.listen(format!("0.0.0.0:{fallback}")))
            .or_else(|_| main.listen("0.0.0.0:0".into()))
            .ok()
            .and_then(|a| a.rsplit(':').next()?.parse().ok())
            .unwrap_or(0);
        let core = Arc::new(Self {
            home,
            main,
            port,
            personas: Mutex::new(HashMap::new()),
            settings: Mutex::new(Settings::load()),
            log: Mutex::new(String::new()),
            ui,
            metered: AtomicBool::new(false),
            redialing: Mutex::new(HashSet::new()),
            watching: Mutex::new(None),
            _lock: lock_file,
        });
        if port != PORT {
            core.settings().set_number("port", Some(u32::from(port)));
        }
        if port == PORT {
            core.say(format!("listening on port {port}"));
        } else {
            core.say(format!(
                "port {PORT} is taken (is the CLI running?); listening on port {port}"
            ));
        }
        core.apply_privacy_to(&core.main, false);
        core.name_this_device();
        core.pump(core.main.clone(), None, Arc::new(AtomicBool::new(true)));
        core.open_personas();
        core.main.reconnect();
        core.sweeper();
        core
    }

    pub fn say(&self, line: impl Into<String>) {
        let line = line.into();
        {
            let mut log = lock(&self.log);
            log.push_str(&line);
            log.push('\n');
        }
        let _ = self.ui.send_blocking(UiEvent::Log(line));
    }

    pub fn log_text(&self) -> String {
        lock(&self.log).clone()
    }

    pub fn settings(&self) -> MutexGuard<'_, Settings> {
        lock(&self.settings)
    }

    /// The node for a conversation: the main identity's, or a persona's.
    pub fn node(&self, persona: Option<&str>) -> Result<Node, String> {
        match persona {
            None => Ok(self.main.clone()),
            Some(id) => lock(&self.personas)
                .get(id)
                .map(|p| p.node.clone())
                .ok_or_else(|| "that anonymous identity no longer exists".into()),
        }
    }

    pub fn persona_label(&self, id: &str) -> Option<String> {
        lock(&self.personas).get(id).map(|p| p.label.clone())
    }

    pub fn persona_port(&self, id: &str) -> Option<u16> {
        lock(&self.personas).get(id).map(|p| p.port)
    }

    /// Anonymous identities as the main identity lists them, with whether
    /// each is running here.
    pub fn personas(&self) -> Vec<PersonaRecord> {
        self.main.personas().unwrap_or_default()
    }

    pub fn persona_ids(&self) -> Vec<String> {
        lock(&self.personas).keys().cloned().collect()
    }

    // ----- Privacy -----

    pub fn set_metered(&self, metered: bool) {
        if self.metered.swap(metered, Ordering::Relaxed) != metered {
            self.apply_privacy();
            self.say(format!(
                "* {} network: cover traffic every {}",
                if metered { "metered" } else { "unmetered" },
                self.cover_ms().map_or("—".into(), |ms| format!("{ms} ms"))
            ));
        }
    }

    fn cover_ms(&self) -> Option<u32> {
        self.settings().flag(settings::COVER).then(|| {
            if self.metered.load(Ordering::Relaxed) {
                settings::COVER_METERED_MS
            } else {
                settings::COVER_UNMETERED_MS
            }
        })
    }

    /// Applies the privacy settings (all on unless turned off) to every node.
    pub fn apply_privacy(&self) {
        self.apply_privacy_to(&self.main, false);
        for p in lock(&self.personas).values() {
            self.apply_privacy_to(&p.node, true);
        }
    }

    fn apply_privacy_to(&self, node: &Node, persona: bool) {
        let (onion, strip, reach, timer, volunteers) = {
            let s = self.settings();
            (
                s.flag(settings::ONION),
                s.flag(settings::STRIP),
                s.flag(settings::REACH),
                s.number(settings::TIMER),
                s.flag(settings::VOLUNTEERS),
            )
        };
        node.set_use_volunteers(volunteers);
        node.set_cover_traffic(self.cover_ms());
        node.set_onion_first(onion);
        node.set_strip_metadata(strip);
        // Anonymous identities never publish themselves in the DHT.
        node.set_reach_internet(reach && !persona);
        node.set_default_disappearing(timer);
    }

    // ----- Devices and addresses -----

    /// New identities are called "device"; name this one after the machine.
    fn name_this_device(&self) {
        let Some(me) = self.main.devices().into_iter().find(|d| d.this_device) else {
            return;
        };
        if me.name != "device" {
            return;
        }
        let host = gtk::glib::host_name().to_string();
        if host.is_empty() {
            return;
        }
        match self.main.rename_device(me.fingerprint, host.clone()) {
            Ok(()) => self.say(format!("* named this device \"{host}\"")),
            Err(e) => self.say(format!("! naming this device: {e}")),
        }
    }

    /// This machine's address on its default network, for invites.
    pub fn lan_ip() -> Option<std::net::IpAddr> {
        // Connecting a UDP socket picks a route without sending anything.
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
    }

    /// The invite link for the main identity or a persona.
    pub fn invite(&self, persona: Option<&str>) -> Result<String, String> {
        let ip = Self::lan_ip().ok_or("No network: connect to a network to make an invite.")?;
        let (node, port) = match persona {
            None => (self.main.clone(), self.port),
            Some(id) => (
                self.node(Some(id))?,
                self.persona_port(id)
                    .ok_or("that anonymous identity isn't running")?,
            ),
        };
        Ok(node.invite_link(format!("{ip}:{port}")))
    }

    pub fn link_code(&self) -> Result<String, String> {
        let ip =
            Self::lan_ip().ok_or("No network: both devices need to be on the same network.")?;
        Ok(self.main.create_link_code(format!("{ip}:{}", self.port)))
    }

    /// The network changed: tell every node, and redial.
    pub fn network_changed(&self) {
        let mobile = self.metered.load(Ordering::Relaxed);
        self.main.network_changed(false, mobile);
        self.main.reconnect();
        for p in lock(&self.personas).values() {
            p.node.network_changed(false, mobile);
            p.node.reconnect();
        }
        self.say("* network changed; reconnecting");
    }

    // ----- Conversations -----

    /// Every conversation of every identity, newest first, requests and
    /// invitations on top.
    pub fn conversations(&self) -> Vec<Conversation> {
        let mut out = conversations(&self.main, None);
        let personas: Vec<(String, Node)> = lock(&self.personas)
            .iter()
            .map(|(id, p)| (id.clone(), p.node.clone()))
            .collect();
        for (id, node) in personas {
            out.extend(conversations(&node, Some(id)));
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.sort_key()));
        out
    }

    /// A contact's name, "You" for this device, else a short fingerprint.
    pub fn name_of(node: &Node, fp: &str) -> String {
        if fp == node.device_fingerprint() {
            return "You".into();
        }
        node.contacts()
            .into_iter()
            .find(|c| c.fingerprint == fp)
            .and_then(|c| c.name)
            .unwrap_or_else(|| short(fp))
    }

    // ----- Relay directories (Appendix P) -----

    /// Subscribes every identity, main and anonymous, to a directory: each
    /// fetches relays and tokens over its own anonymous links.
    pub fn subscribe_directory(&self, link: &str) -> Result<threnody_ffi::DirectoryRecord, String> {
        let rec = self
            .main
            .subscribe_directory(link.to_owned())
            .map_err(|e| e.to_string())?;
        for id in self.persona_ids() {
            if let Ok(n) = self.node(Some(&id))
                && let Err(e) = n.subscribe_directory(link.to_owned())
            {
                self.say(format!(
                    "! anonymous identity {}: directory: {e}",
                    short(&id)
                ));
            }
        }
        Ok(rec)
    }

    pub fn unsubscribe_directory(&self, id_hex: &str) {
        self.main.unsubscribe_directory(id_hex.to_owned());
        for id in self.persona_ids() {
            if let Ok(n) = self.node(Some(&id)) {
                n.unsubscribe_directory(id_hex.to_owned());
            }
        }
    }

    // ----- Anonymous identities -----

    fn open_personas(self: &Arc<Self>) {
        match self.main.burn_expired_personas() {
            Ok(burned) => {
                for id in burned {
                    self.say(format!(
                        "* burned expired anonymous identity {}",
                        short(&id)
                    ));
                }
            }
            Err(e) => self.say(format!("! anonymous identities: {e}")),
        }
        for rec in self.personas() {
            if let Err(e) = self.open_persona(&rec) {
                self.say(format!("! anonymous identity {}: {e}", rec.label));
            }
        }
    }

    /// Opens a persona's node on a port of its own (kept, so its invites
    /// keep working). No discovery, Bluetooth or DHT: those would tell
    /// nearby devices or strangers who it is.
    fn open_persona(self: &Arc<Self>, rec: &PersonaRecord) -> Result<(), String> {
        if lock(&self.personas).contains_key(&rec.id) {
            return Ok(());
        }
        let home = PathBuf::from(&rec.home);
        let sealed =
            threnody_ffi::identity_is_sealed(rec.home.clone()).map_err(|e| e.to_string())?;
        let pw = if sealed {
            Some(
                keyring::get(&home)
                    .ok_or("its key isn't in the keyring")?
                    .0
                    .clone(),
            )
        } else {
            None
        };
        let node = ThrenodyNode::open(rec.home.clone(), pw, None).map_err(|e| e.to_string())?;
        let key = format!("port_{}", rec.id);
        let want = self.settings().number(&key).unwrap_or(0);
        let addr = node
            .listen(format!("0.0.0.0:{want}"))
            .or_else(|_| node.listen("0.0.0.0:0".into()))
            .map_err(|e| e.to_string())?;
        let port: u16 = addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        self.settings().set_number(&key, Some(u32::from(port)));
        self.apply_privacy_to(&node, true);
        let alive = Arc::new(AtomicBool::new(true));
        self.pump(node.clone(), Some(rec.id.clone()), alive.clone());
        node.reconnect();
        lock(&self.personas).insert(
            rec.id.clone(),
            Persona {
                node,
                label: rec.label.clone(),
                home,
                port,
                alive,
            },
        );
        self.say(format!(
            "anonymous identity {} listening on port {port}",
            short(&rec.id)
        ));
        // Same directories as the main identity, through links of its own.
        let links: Vec<String> = self
            .main
            .directories()
            .into_iter()
            .map(|d| d.link)
            .collect();
        if !links.is_empty() {
            let (core, id) = (Arc::downgrade(self), rec.id.clone());
            std::thread::spawn(move || {
                let Some(core) = core.upgrade() else { return };
                let Ok(node) = core.node(Some(&id)) else {
                    return;
                };
                let have: Vec<String> = node.directories().into_iter().map(|d| d.link).collect();
                for l in links.into_iter().filter(|l| !have.contains(l)) {
                    let _ = node.subscribe_directory(l);
                }
            });
        }
        Ok(())
    }

    /// Makes and starts an anonymous identity, sealed with a keyring key of
    /// its own (as the CLI does).
    pub fn create_persona(
        self: &Arc<Self>,
        label: &str,
        expires_ms: Option<u64>,
    ) -> Result<String, String> {
        let rec = self
            .main
            .create_persona(label.into(), expires_ms, None)
            .map_err(|e| e.to_string())?;
        let home = PathBuf::from(&rec.home);
        match keyring::create(&home) {
            Ok(pw) => {
                threnody_ffi::change_passphrase(rec.home.clone(), None, Some(pw.0.clone()))
                    .map_err(|e| e.to_string())?;
            }
            Err(e) => self.say(format!("! no keyring ({e}); its key is stored unprotected")),
        }
        self.open_persona(&rec)?;
        let _ = self.ui.send_blocking(UiEvent::PersonasChanged);
        Ok(rec.id)
    }

    pub fn rename_persona(&self, id: &str, label: &str) -> Result<(), String> {
        self.main
            .rename_persona(id.into(), label.into())
            .map_err(|e| e.to_string())?;
        if let Some(p) = lock(&self.personas).get_mut(id) {
            p.label = label.into();
        }
        let _ = self.ui.send_blocking(UiEvent::PersonasChanged);
        Ok(())
    }

    /// Burns an anonymous identity: its node, keys, history and files.
    pub fn burn_persona(&self, id: &str) -> Result<(), String> {
        let gone = lock(&self.personas).remove(id);
        let home = gone.as_ref().map(|p| p.home.clone()).or_else(|| {
            self.personas()
                .into_iter()
                .find(|r| r.id == id)
                .map(|r| PathBuf::from(r.home))
        });
        if let Some(p) = gone {
            p.alive.store(false, Ordering::Relaxed);
            p.node.shutdown();
        }
        self.main
            .burn_persona(id.into())
            .map_err(|e| e.to_string())?;
        if let Some(h) = home {
            keyring::delete(&h);
        }
        self.settings().set_number(&format!("port_{id}"), None);
        self.say(format!("* burned anonymous identity {}", short(id)));
        let _ = self.ui.send_blocking(UiEvent::PersonasChanged);
        Ok(())
    }

    // ----- Files -----

    /// Where a node keeps received photos (and a persona everything): in
    /// its own data directory, so burning a persona takes them along.
    fn media_dir(&self, persona: Option<&str>) -> PathBuf {
        let base = match persona {
            None => self.home.clone(),
            Some(id) => lock(&self.personas)
                .get(id)
                .map_or_else(|| self.home.clone(), |p| p.home.clone()),
        };
        base.join("media")
    }

    /// Keeps a received file: photos privately, others in
    /// Downloads/Threnody. A persona keeps everything privately.
    fn keep(&self, persona: Option<&str>, name: &str, data: &[u8]) -> Option<String> {
        let dir = if persona.is_some() || is_image(name) {
            self.media_dir(persona)
        } else {
            gtk::glib::user_special_dir(gtk::glib::UserDirectory::Downloads)
                .unwrap_or_else(|| self.home.join("downloads"))
                .join("Threnody")
        };
        let path = free_path(&dir, name);
        let saved = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, data));
        match saved {
            Ok(()) => Some(path.display().to_string()),
            Err(e) => {
                self.say(format!("! saving {name}: {e}"));
                None
            }
        }
    }

    /// Removes kept photos whose messages were deleted or disappeared, and
    /// burns anonymous identities whose time is up. Every five minutes.
    fn sweeper(self: &Arc<Self>) {
        let core = Arc::downgrade(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(300));
                let Some(core) = core.upgrade() else { return };
                let now = threnody_core::now_ms();
                for p in core.personas() {
                    if p.expires_ms.is_some_and(|t| t <= now) {
                        let _ = core.burn_persona(&p.id);
                    }
                }
                core.sweep(None);
                for id in core.persona_ids() {
                    core.sweep(Some(&id));
                }
            }
        });
    }

    fn sweep(&self, persona: Option<&str>) {
        let Ok(node) = self.node(persona) else { return };
        let dir = self.media_dir(persona);
        let Ok(files) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut kept: HashSet<String> = HashSet::new();
        let mut all = Vec::new();
        for c in node.contacts() {
            match node.history(c.fingerprint, u32::MAX) {
                Ok(h) => all.extend(h),
                Err(_) => return,
            }
        }
        for g in node.groups() {
            match node.group_history(g.id, u32::MAX) {
                Ok(h) => all.extend(h),
                Err(_) => return,
            }
        }
        kept.extend(all.into_iter().filter_map(|e| e.file?.location));
        for f in files.flatten() {
            let path = f.path().display().to_string();
            if !kept.contains(&path) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }

    // ----- Events -----

    fn pump(self: &Arc<Self>, node: Node, persona: Option<String>, alive: Arc<AtomicBool>) {
        let core = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("threnody-events".into())
            .spawn(move || {
                while alive.load(Ordering::Relaxed) {
                    let Some(e) = node.next_event(1000) else {
                        continue;
                    };
                    let Some(core) = core.upgrade() else { return };
                    let e = core.handle(&node, persona.as_deref(), e);
                    let _ = core.ui.send_blocking(UiEvent::Node {
                        persona: persona.clone(),
                        event: e,
                    });
                }
            })
            .expect("spawning the event thread");
    }

    /// What the window doesn't need to do itself: logging (never message
    /// text), keeping files, redialing. Returns the event, file data dropped.
    fn handle(self: &Arc<Self>, node: &Node, persona: Option<&str>, e: NodeEvent) -> NodeEvent {
        match e {
            NodeEvent::File {
                peer,
                name,
                data,
                id,
                sensitive,
                caption,
                album,
            } => {
                self.say(format!(
                    "* {} sent a file ({} bytes)",
                    short(&peer),
                    data.len()
                ));
                let location = self.keep(persona, &name, &data);
                let options = FileOptions {
                    sensitive,
                    caption: caption.clone(),
                    album,
                };
                if let Err(x) = node.record_received_file(
                    peer.clone(),
                    name.clone(),
                    data.len() as u64,
                    location,
                    id,
                    options,
                ) {
                    self.say(format!("! recording a file: {x}"));
                }
                NodeEvent::File {
                    peer,
                    name,
                    data: Vec::new(),
                    id,
                    sensitive,
                    caption,
                    album,
                }
            }
            NodeEvent::GroupFile {
                id,
                group,
                from,
                name,
                data,
                ours,
                sensitive,
                caption,
                album,
            } => {
                self.say(format!(
                    "* group {}: {} sent a file ({} bytes)",
                    short(&group),
                    short(&from),
                    data.len()
                ));
                let location = self.keep(persona, &name, &data);
                let options = FileOptions {
                    sensitive,
                    caption: caption.clone(),
                    album,
                };
                if let Err(x) = node.record_received_group_file(
                    group.clone(),
                    from.clone(),
                    name.clone(),
                    data.len() as u64,
                    location,
                    id,
                    options,
                ) {
                    self.say(format!("! recording a file: {x}"));
                }
                NodeEvent::GroupFile {
                    id,
                    group,
                    from,
                    name,
                    data: Vec::new(),
                    ours,
                    sensitive,
                    caption,
                    album,
                }
            }
            e => {
                match &e {
                    NodeEvent::Connected { peer, via } => self.say(format!(
                        "* connected {}{}",
                        short(peer),
                        via.as_deref()
                            .map(|v| format!(" via {}", short(v)))
                            .unwrap_or_default()
                    )),
                    NodeEvent::Disconnected { peer, reason } => {
                        self.say(format!("* {} disconnected ({reason})", short(peer)));
                        if node
                            .contacts()
                            .iter()
                            .any(|c| c.fingerprint == *peer && c.mutually_approved)
                        {
                            self.redial(node.clone(), persona.map(str::to_owned));
                        }
                    }
                    // The log is for transports, not content.
                    NodeEvent::Message { peer, text, .. } => self.say(format!(
                        "* message from {} ({} chars)",
                        short(peer),
                        text.chars().count()
                    )),
                    NodeEvent::GroupMessage {
                        group, from, text, ..
                    } => self.say(format!(
                        "* group {}: message from {} ({} chars)",
                        short(group),
                        short(from),
                        text.chars().count()
                    )),
                    NodeEvent::MessageRequest { peer, .. } => {
                        self.say(format!("* message request from {}", short(peer)));
                    }
                    NodeEvent::ApprovalChanged { peer, mutual } => {
                        self.say(format!("* {} approval: mutual={mutual}", short(peer)));
                    }
                    other => self.say(format!("· {other:?}")),
                }
                e
            }
        }
    }

    /// After an approved contact's session ends, redials with backoff
    /// until every approved contact is connected again.
    fn redial(self: &Arc<Self>, node: Node, persona: Option<String>) {
        if !lock(&self.redialing).insert(persona.clone()) {
            return;
        }
        let core = Arc::downgrade(self);
        std::thread::spawn(move || {
            const DELAYS: [u64; 6] = [3, 10, 30, 60, 120, 300];
            for n in 0.. {
                std::thread::sleep(Duration::from_secs(DELAYS[n.min(DELAYS.len() - 1)]));
                let Some(core) = core.upgrade() else { return };
                let missing = node
                    .contacts()
                    .iter()
                    .any(|c| c.mutually_approved && !c.connected);
                let gone = persona
                    .as_deref()
                    .is_some_and(|p| core.node(Some(p)).is_err());
                if !missing || gone {
                    lock(&core.redialing).remove(&persona);
                    return;
                }
                core.say("* redialing approved contacts");
                node.reconnect();
            }
        });
    }

    pub fn shutdown(&self) {
        for p in lock(&self.personas).values() {
            p.alive.store(false, Ordering::Relaxed);
            p.node.shutdown();
        }
        self.main.shutdown();
    }
}

/// One entry per contact account and group of `node`.
fn conversations(node: &Node, persona: Option<String>) -> Vec<Conversation> {
    let mine = node.account_fingerprint();
    let me = node.device_fingerprint();
    let mut by_key: Vec<(String, Vec<ContactInfo>)> = Vec::new();
    for c in node.contacts() {
        if c.blocked || c.fingerprint == me || c.account.as_deref() == Some(mine.as_str()) {
            continue;
        }
        let k = c.account.clone().unwrap_or_else(|| c.fingerprint.clone());
        match by_key.iter_mut().find(|(key, _)| *key == k) {
            Some((_, v)) => v.push(c),
            None => by_key.push((k, vec![c])),
        }
    }
    let mut out = Vec::new();
    for (key, devices) in by_key {
        let best = devices
            .iter()
            .find(|d| d.connected)
            .unwrap_or(&devices[0])
            .clone();
        let name = devices.iter().find_map(|d| d.name.clone());
        let last = node
            .history(best.fingerprint.clone(), 1)
            .ok()
            .and_then(|mut h| h.pop());
        out.push(Conversation {
            chat: ChatRef {
                persona: persona.clone(),
                target: Target::Contact(key),
            },
            title: name.unwrap_or_else(|| short(&best.fingerprint)),
            device: best.fingerprint.clone(),
            connected: devices.iter().any(|d| d.connected),
            approved: devices.iter().any(|d| d.local_approved),
            verified: devices.iter().all(|d| d.verified),
            any_verified: devices.iter().any(|d| d.verified),
            unverified: devices
                .iter()
                .filter(|d| !d.verified)
                .map(|d| d.fingerprint.clone())
                .collect(),
            accepted: devices.iter().any(|d| d.accepted),
            revealed: devices.iter().find_map(|d| d.revealed.clone()),
            revealed_invite: devices.iter().find_map(|d| d.revealed_invite.clone()),
            devices: devices.iter().map(|d| d.fingerprint.clone()).collect(),
            group: None,
            invite: None,
            last,
        });
    }
    for g in node.groups() {
        let last = node
            .group_history(g.id.clone(), 1)
            .ok()
            .and_then(|mut h| h.pop());
        out.push(Conversation {
            chat: ChatRef {
                persona: persona.clone(),
                target: Target::Group(g.id.clone()),
            },
            title: g.name.clone(),
            device: String::new(),
            devices: Vec::new(),
            connected: false,
            approved: true,
            verified: true,
            any_verified: false,
            unverified: Vec::new(),
            accepted: true,
            revealed: None,
            revealed_invite: None,
            group: Some(g),
            invite: None,
            last,
        });
    }
    for i in node.group_invites() {
        out.push(Conversation {
            chat: ChatRef {
                persona: persona.clone(),
                target: Target::Group(i.group.clone()),
            },
            title: i.name.clone(),
            device: String::new(),
            devices: Vec::new(),
            connected: false,
            approved: false,
            verified: true,
            any_verified: false,
            unverified: Vec::new(),
            accepted: true,
            revealed: None,
            revealed_invite: None,
            group: None,
            invite: Some(i),
            last: None,
        });
    }
    out
}

pub fn short(fp: &str) -> String {
    fp.chars().take(9).collect()
}

pub fn is_image(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "heic" | "heif" | "tif" | "tiff"
    )
}

/// `dir/name` with any path the sender put in the name stripped, and a
/// number added if a file of that name exists.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("file");
    let candidate = dir.join(base);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (base, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(candidate)
}

/// What the list or a notification says for a file.
pub fn file_label(name: &str, sensitive: bool, caption: &str) -> String {
    match (sensitive, is_image(name)) {
        (true, true) => "📷 Sensitive photo".into(),
        (true, false) => "📎 Sensitive file".into(),
        (false, true) => format!("📷 {}", if caption.is_empty() { "Photo" } else { caption }),
        (false, false) => format!("📎 {}", if caption.is_empty() { name } else { caption }),
    }
}

/// A one-line preview of a stored message.
pub fn preview(e: &HistoryEntry) -> String {
    match &e.file {
        Some(f) => file_label(&f.name, f.sensitive, &e.text),
        None => e.text.lines().next().unwrap_or("").to_owned(),
    }
}
