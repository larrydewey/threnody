//! UniFFI bindings for embedding a Threnody node in apps (Android via
//! Kotlin, iOS via Swift, and Python for tests and scripting).
//!
//! The API is deliberately small and blocking: each call runs on the
//! node's own Tokio runtime, so apps call it from a background thread and
//! drain events with [`ThrenodyNode::next_event`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use threnody_core::history::FileNote;
use threnody_core::store::{Home, Lookup};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity, safety_number};
use threnody_net::history::OutgoingFile;
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;

uniffi::setup_scaffolding!();

mod groups;
mod persona;
mod volunteer;
pub use persona::{PersonaRecord, ProfileAttr};
pub use volunteer::{
    CredentialAskRecord, CredentialOfferRecord, CredentialRecord, DirectoryRecord,
};

pub use groups::{GroupInfo, GroupInvite};
use threnody_groups::node::GroupNode;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ThrenodyError {
    // Not `message`: that would clash with `Throwable.message` in Kotlin.
    #[error("{reason}")]
    Failed { reason: String },
}

fn fail(e: impl std::fmt::Display) -> ThrenodyError {
    ThrenodyError::Failed {
        reason: e.to_string(),
    }
}

type Result<T> = std::result::Result<T, ThrenodyError>;

/// Cover-traffic interval a node starts with (see `set_cover_traffic`).
pub const DEFAULT_COVER_MS: u32 = 2000;

/// A contact as an app shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ContactInfo {
    pub fingerprint: String,
    pub name: Option<String>,
    pub mutually_approved: bool,
    pub verified: bool,
    pub account: Option<String>,
    pub connected: bool,
    /// We want its messages (else they're requests); see `accept_contact`.
    pub accepted: bool,
    pub blocked: bool,
    /// What it shares of its profile with us, as it describes itself.
    pub shared_profile: Vec<ProfileAttr>,
    /// The main identity it proved it is (it reached us as a persona),
    /// and the invite it gave to reach that identity.
    pub revealed: Option<String>,
    pub revealed_invite: Option<String>,
    /// We approved them (regardless of whether they approved us back).
    pub local_approved: bool,
}

/// A device of our account.
/// Internet reachability (Appendix N), for status displays.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReachInfo {
    pub enabled: bool,
    /// Joined the public DHT.
    pub online: bool,
    /// Our addresses as contacts would dial them.
    pub addresses: Vec<String>,
    /// Our NAT gives a new outside port per destination: direct paths
    /// are unlikely.
    pub symmetric: bool,
    /// Contacts being hole-punched right now.
    pub punching: u32,
}

/// WireGuard tunnel info for this device.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WireguardInfo {
    /// This device's WireGuard static public key (base64).
    pub public_key: String,
    /// This device's overlay IPv6 ULA address.
    pub overlay: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DeviceInfo {
    pub fingerprint: String,
    pub name: String,
    pub this_device: bool,
}

/// One stored message.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HistoryEntry {
    pub at_ms: u64,
    pub outgoing: bool,
    pub device: String,
    pub text: String,
    pub disappearing: bool,
    /// Set when the entry is a file transfer (then `text` is empty).
    pub file: Option<FileInfo>,
    /// An outgoing message a device of the recipient acknowledged (for a
    /// group, every recipient did).
    pub delivered: bool,
    /// An outgoing message that the recipient displayed: "seen".
    pub read: bool,
    /// The id to delete it by (`delete_messages`); 0 = none, use
    /// `delete_entry` with `at_ms` and `device`.
    pub id: u64,
    /// The text was edited after sending.
    pub edited: bool,
    /// For outgoing group messages: how many members it went to, and how
    /// many acknowledged their copy (forwarded or mailbox copies don't
    /// count until the member itself acknowledges).
    pub recipients: u32,
    pub delivered_to: u32,
    /// Reactions, one per emoji in the order first added.
    pub reactions: Vec<ReactionInfo>,
}

/// One emoji on a message: how many reacted with it, and whether we did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReactionInfo {
    pub emoji: String,
    pub count: u32,
    pub mine: bool,
}

/// A file in history: its name, size and where the app saved it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub location: Option<String>,
    /// Marked sensitive by its sender: show it covered until opened.
    pub sensitive: bool,
    /// Files sent together share an album id (0 = alone); the caption is
    /// the text of the album's first entry.
    pub album: u64,
}

/// What goes with a file: whether it is sensitive, its caption, and the
/// album it belongs to (0 = alone; see `album_id`).
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct FileOptions {
    pub sensitive: bool,
    pub caption: String,
    pub album: u64,
}

/// A fresh album id for photos or files sent together.
#[uniffi::export]
pub fn album_id() -> u64 {
    threnody_net::history::album_id()
}

/// A QR code as a square of modules, row by row (`true` = dark).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct QrMatrix {
    pub size: u32,
    pub dark: Vec<bool>,
}

/// Encodes `text` (an invite link or link code) as a QR code for apps to draw.
#[uniffi::export]
pub fn qr_matrix(text: String) -> Result<QrMatrix> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(fail)?;
    Ok(QrMatrix {
        size: u32::try_from(code.width()).map_err(fail)?,
        dark: code
            .to_colors()
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect(),
    })
}

/// Looks for a QR code in a greyscale image (a camera frame's luma
/// plane: `width` × `height` pixels, rows `row_stride` bytes apart) and
/// returns the text of the first one that decodes.
#[uniffi::export]
pub fn decode_qr(width: u32, height: u32, row_stride: u32, luma: Vec<u8>) -> Option<String> {
    let (w, h, stride) = (width as usize, height as usize, row_stride as usize);
    if w == 0 || h == 0 || stride < w || luma.len() < stride * (h - 1) + w {
        return None;
    }
    let mut img = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| luma[y * stride + x]);
    img.detect_grids()
        .into_iter()
        .find_map(|g| g.decode().ok().map(|(_, text)| text))
}

/// Whether the identity in `home` is sealed with a passphrase (false when
/// there is no identity yet). Call before `open` to decide what to pass.
#[uniffi::export]
pub fn identity_is_sealed(home: String) -> Result<bool> {
    let home = Home::new(home);
    if !home.has_identity() {
        return Ok(false);
    }
    home.identity_is_sealed().map_err(fail)
}

/// Adds, changes or removes (`new = None`) the passphrase sealing the
/// identity in `home`. Use it while no node has `home` open.
#[uniffi::export]
pub fn change_passphrase(home: String, current: Option<String>, new: Option<String>) -> Result<()> {
    Home::new(home)
        .change_passphrase(
            current.as_deref().map(str::as_bytes),
            new.as_deref().map(str::as_bytes),
        )
        .map_err(fail)
}

/// An approved contact heard over Bluetooth that this device should dial.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BleDial {
    pub peer: String,
    pub psm: u16,
}

/// Things the app should react to.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NodeEvent {
    Connected {
        peer: String,
        via: Option<String>,
    },
    Disconnected {
        peer: String,
        reason: String,
    },
    Message {
        peer: String,
        text: String,
        offline: bool,
    },
    /// `id` is the sender's id for it: pass it to `record_received_file`,
    /// with the caption, sensitivity and album.
    File {
        peer: String,
        name: String,
        data: Vec<u8>,
        id: u64,
        sensitive: bool,
        caption: String,
        album: u64,
    },
    /// `peer` edited a message: reload its conversation.
    MessageEdited {
        peer: String,
    },
    /// `peer` deleted messages (its own, for everyone; or, from our own
    /// device, ones we deleted there): reload its conversation.
    MessagesDeleted {
        peer: String,
        count: u32,
    },
    ApprovalChanged {
        peer: String,
        mutual: bool,
    },
    AccountChanged {
        account: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    DeviceLinked {
        device: String,
    },
    ThisDeviceRemoved,
    /// A nearby approved peer created a Wi-Fi Direct group for us: join
    /// it (network name + passphrase), then `connect` to
    /// `threnody://<peer>@<addr>`.
    WifiDirectOffer {
        peer: String,
        ssid: String,
        passphrase: String,
        addr: String,
    },
    /// A nearby approved peer asks us to create a group and offer it.
    WifiDirectRequested {
        peer: String,
    },
    /// A message from someone not accepted yet (it's in history): show it
    /// among requests, not conversations. `text` or `file` (a name).
    MessageRequest {
        peer: String,
        text: Option<String>,
        file: Option<String>,
    },
    /// `peer` acknowledged one of our messages: reload the history of its
    /// conversation (`group` if set, else the 1:1 one with `peer`).
    Delivered {
        peer: String,
        group: Option<String>,
    },
    /// We joined a group (after accepting, or automatically when a
    /// mutually approved contact invited us).
    GroupJoined {
        group: String,
        name: String,
        owner: String,
    },
    /// Someone who isn't a mutually approved contact invites us: ask the
    /// user, then `accept_group_invite` or `decline_group_invite`.
    GroupInvited {
        group: String,
        name: String,
        from: String,
    },
    GroupMembersChanged {
        group: String,
        added: Vec<String>,
        removed: Vec<String>,
    },
    /// The owner removed us.
    GroupLeft {
        group: String,
    },
    /// A member's role was changed.
    GroupRoleChanged {
        group: String,
        member: String,
        role: u8, // 0=Owner, 1=Admin, 2=Member
    },
    /// `ours`: sent by another device of our own account.
    GroupMessage {
        group: String,
        from: String,
        text: String,
        ours: bool,
    },
    /// A file from a group member: save it, then call
    /// `record_received_group_file`.
    /// `id` is the sender's id for it: pass it to `record_received_group_file`.
    GroupFile {
        id: u64,
        group: String,
        from: String,
        name: String,
        data: Vec<u8>,
        ours: bool,
        sensitive: bool,
        caption: String,
        album: u64,
    },
    /// `peer` says it displayed our outgoing messages `ids` (their
    /// sender-ids, as recorded on our side): ticks update on reload.
    Read {
        peer: String,
    },
    /// `peer` changed reactions in our chat, or in `group`; reload it.
    Reacted {
        peer: String,
        group: Option<String>,
    },
    /// `peer` changed what it shares of its profile; reload contacts.
    ProfileChanged {
        peer: String,
    },
    /// `peer` is (or isn't) typing in its conversation with us: show (or
    /// hide) the typing indicator bubble.
    Typing {
        peer: String,
        active: bool,
    },
    /// `peer` (a persona, to us) proved it is `identity`; `invite` reaches
    /// that identity. Also kept as `ContactInfo::revealed`.
    IdentityRevealed {
        peer: String,
        identity: String,
        invite: Option<String>,
    },
    /// Our own device `from` shared history; reload conversations.
    HistorySynced {
        from: String,
        added: u32,
    },
    /// Progress with relay directories or volunteering, for logs.
    VolunteerNote {
        note: String,
    },
    /// `peer` offers us a credential: `accept_credential_offer(id)` or
    /// `decline_credential(id)`.
    CredentialOffered {
        offer: CredentialOfferRecord,
    },
    /// A credential we accepted arrived.
    CredentialReceived {
        peer: String,
        schema: String,
    },
    /// `peer` asks us to prove attributes: `present_credential` or
    /// `decline_credential`.
    CredentialAsked {
        ask: CredentialAskRecord,
    },
    /// `peer` proved attributes we asked for (exchange `id`). The
    /// pseudonym is the same each time that credential is shown to us.
    CredentialPresented {
        id: u64,
        peer: String,
        issuer: String,
        schema: String,
        attributes: Vec<ProfileAttr>,
        pseudonym: String,
    },
    CredentialFailed {
        id: u64,
        peer: String,
        reason: String,
    },
    /// A mutually approved peer offered a WireGuard tunnel. The app
    /// should apply this to its WireGuard interface (or userspace
    /// implementation like boringtun on Android).
    TunnelUp {
        peer: String,
        wg_public: Vec<u8>,
        endpoint: String,
        overlay: String,
        psk: Vec<u8>,
    },
    /// A tunnel peer was removed (approval revoked or tunnel port changed).
    TunnelDown {
        peer: String,
    },
    /// Anything else, described for logs.
    Other {
        description: String,
    },
}

/// A platform byte pipe (e.g. an Android Bluetooth L2CAP socket),
/// implemented in Kotlin / Swift. Called from a Rust worker thread.
#[uniffi::export(with_foreign)]
pub trait ByteLink: Send + Sync {
    /// Writes bytes to the remote end; returns false once the link is dead.
    fn send(&self, data: Vec<u8>) -> bool;
    /// Closes the link (Rust is done with it). Not `close`: that clashes
    /// with `AutoCloseable.close` in the generated Kotlin.
    fn disconnect(&self);
}

/// The app's handle for feeding a link's received bytes into the node.
#[derive(uniffi::Object)]
pub struct LinkHandle {
    tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>,
}

#[uniffi::export]
impl LinkHandle {
    /// Bytes read from the platform socket.
    pub fn receive(&self, data: Vec<u8>) {
        if let Some(tx) = self
            .tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = tx.send(data);
        }
    }

    /// The platform socket hit end-of-stream or an error.
    pub fn closed(&self) {
        self.tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

#[derive(uniffi::Object)]
pub struct ThrenodyNode {
    rt: Runtime,
    node: Node,
    events: Mutex<UnboundedReceiver<Event>>,
    groups: Mutex<GroupNode>,
    /// Events produced while handling another (group traffic), not yet returned.
    queued: Mutex<VecDeque<NodeEvent>>,
}

/// Groups an entry's reactions by emoji; `me` is our reactor id.
fn reactions(e: &threnody_core::history::Entry, me: &[u8; 32]) -> Vec<ReactionInfo> {
    let mut out: Vec<ReactionInfo> = Vec::new();
    for (who, emoji) in &e.reactions {
        match out.iter_mut().find(|r| r.emoji == *emoji) {
            Some(r) => {
                r.count += 1;
                r.mine |= who == me;
            }
            None => out.push(ReactionInfo {
                emoji: emoji.clone(),
                count: 1,
                mine: who == me,
            }),
        }
    }
    out
}

fn history_entries(entries: &[threnody_core::history::Entry], me: [u8; 32]) -> Vec<HistoryEntry> {
    entries
        .iter()
        .map(|e| HistoryEntry {
            reactions: reactions(e, &me),
            at_ms: e.at_ms,
            outgoing: e.outgoing,
            device: PublicIdentity::from_bytes(&e.device)
                .map(|d| fp(&d))
                .unwrap_or_default(),
            text: e.text.clone(),
            disappearing: e.expires_at_ms.is_some(),
            file: e.file.as_ref().map(|f| FileInfo {
                name: f.name.clone(),
                size: f.size,
                location: f.location.clone(),
                sensitive: f.sensitive,
                album: f.album,
            }),
            delivered: e.delivered,
            read: e.read_ms != 0,
            id: e.message_id(),
            edited: e.edited_ms != 0,
            recipients: e.recipients,
            delivered_to: u32::try_from(e.delivered_to.len()).unwrap_or(u32::MAX),
        })
        .collect()
}

fn fp(p: &PublicIdentity) -> String {
    p.fingerprint().to_string()
}

fn text_of(msg: &AppMessage) -> Option<String> {
    match msg {
        AppMessage::Text { body, .. } => Some(body.clone()),
        _ => None,
    }
}

fn convert(e: Event) -> NodeEvent {
    match e {
        Event::Connected { peer, via, .. } => NodeEvent::Connected {
            peer: fp(&peer),
            via: via.as_ref().map(fp),
        },
        Event::Disconnected { peer, reason } => NodeEvent::Disconnected {
            peer: fp(&peer),
            reason,
        },
        Event::Message { peer, msg } if text_of(&msg).is_some() => NodeEvent::Message {
            peer: fp(&peer),
            text: text_of(&msg).unwrap_or_default(),
            offline: false,
        },
        Event::OfflineMessage { from, msg, .. } if text_of(&msg).is_some() => NodeEvent::Message {
            peer: fp(&from),
            text: text_of(&msg).unwrap_or_default(),
            offline: true,
        },
        Event::Message {
            peer,
            msg:
                AppMessage::File {
                    name,
                    data,
                    id,
                    sensitive,
                    caption,
                    album,
                    ..
                },
        } => NodeEvent::File {
            peer: fp(&peer),
            name,
            data,
            id,
            sensitive,
            caption,
            album,
        },
        Event::MessageEdited { peer, .. } => NodeEvent::MessageEdited { peer: fp(&peer) },
        Event::MessagesDeleted { peer, count } => NodeEvent::MessagesDeleted {
            peer: fp(&peer),
            count: u32::try_from(count).unwrap_or(u32::MAX),
        },
        Event::ApprovalChanged { peer, mutual, .. } => NodeEvent::ApprovalChanged {
            peer: fp(&peer),
            mutual,
        },
        Event::AccountChanged {
            account,
            added,
            removed,
        } => NodeEvent::AccountChanged {
            account: account.fingerprint().to_string(),
            added: added.iter().map(fp).collect(),
            removed: removed.iter().map(fp).collect(),
        },
        Event::DeviceLinked { device } => NodeEvent::DeviceLinked {
            device: fp(&device),
        },
        Event::ThisDeviceRemoved => NodeEvent::ThisDeviceRemoved,
        Event::MessageRequest { peer, msg } => NodeEvent::MessageRequest {
            peer: fp(&peer),
            text: text_of(&msg),
            file: match msg {
                AppMessage::File { name, .. } => Some(name),
                _ => None,
            },
        },
        Event::Read { peer, .. } => NodeEvent::Read { peer: fp(&peer) },
        Event::ProfileChanged { peer } => NodeEvent::ProfileChanged { peer: fp(&peer) },
        Event::Typing { peer, active } => NodeEvent::Typing {
            peer: fp(&peer),
            active,
        },
        Event::Reacted { peer, .. } => NodeEvent::Reacted {
            peer: fp(&peer),
            group: None,
        },
        Event::IdentityRevealed {
            peer,
            identity,
            invite,
        } => NodeEvent::IdentityRevealed {
            peer: fp(&peer),
            identity: fp(&identity),
            invite,
        },
        Event::VolunteerNote { note } => NodeEvent::VolunteerNote { note },
        Event::CredentialOffered { offer } => NodeEvent::CredentialOffered {
            offer: volunteer::offer_record(offer),
        },
        Event::CredentialReceived { peer, schema } => NodeEvent::CredentialReceived {
            peer: fp(&peer),
            schema,
        },
        Event::CredentialAsked { ask } => NodeEvent::CredentialAsked {
            ask: volunteer::ask_record(ask),
        },
        Event::CredentialPresented { peer, id, verified } => NodeEvent::CredentialPresented {
            id,
            peer: fp(&peer),
            issuer: fp(&verified.issuer),
            schema: verified.schema,
            attributes: persona::attrs(&verified.attributes),
            pseudonym: verified
                .pseudonym
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        },
        Event::CredentialFailed { peer, id, reason } => NodeEvent::CredentialFailed {
            id,
            peer: fp(&peer),
            reason,
        },
        Event::HistorySynced { from, added } => NodeEvent::HistorySynced {
            from: fp(&from),
            added: u32::try_from(added).unwrap_or(u32::MAX),
        },
        Event::Delivered { peer, group, .. } => NodeEvent::Delivered {
            peer: fp(&peer),
            group: group.map(|g| g.iter().map(|b| format!("{b:02x}")).collect()),
        },
        Event::WifiDirectOffer { peer, offer } => NodeEvent::WifiDirectOffer {
            peer: fp(&peer),
            ssid: offer.ssid,
            passphrase: offer.passphrase,
            addr: offer.addr,
        },
        Event::WifiDirectRequested { peer } => NodeEvent::WifiDirectRequested { peer: fp(&peer) },
        Event::TunnelUp {
            peer,
            wg_public,
            endpoint,
            overlay,
            psk,
        } => NodeEvent::TunnelUp {
            peer: fp(&peer),
            wg_public: wg_public.to_vec(),
            endpoint: endpoint.to_string(),
            overlay: overlay.to_string(),
            psk: psk.0.to_vec(),
        },
        Event::TunnelDown { peer, .. } => NodeEvent::TunnelDown { peer: fp(&peer) },
        other => NodeEvent::Other {
            description: format!("{other:?}"),
        },
    }
}

impl ThrenodyNode {
    fn group_node(&self) -> MutexGuard<'_, GroupNode> {
        self.groups
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Deletes the entry at `at_ms` from `device` (a fingerprint) in `conv`.
    pub(crate) fn delete_any(
        &self,
        conv: threnody_core::history::ConversationId,
        at_ms: u64,
        device: &str,
    ) -> u32 {
        let dev = self.node.history(conv).ok().and_then(|h| {
            h.entries()
                .iter()
                .find(|e| {
                    e.at_ms == at_ms
                        && PublicIdentity::from_bytes(&e.device).is_ok_and(|d| fp(&d) == device)
                })
                .map(|e| e.device)
        });
        dev.map_or(0, |d| {
            u32::try_from(self.node.delete_entry(conv, at_ms, d)).unwrap_or(u32::MAX)
        })
    }

    fn queued(&self) -> MutexGuard<'_, VecDeque<NodeEvent>> {
        self.queued
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Tries for a while to get a session with `p` (directly or through
    /// relays); callers find out whether it worked when they send.
    fn try_reach(&self, p: &PublicIdentity) {
        let _ = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), self.node.reach_peer(p)).await
        });
    }

    pub(crate) fn resolve(&self, peer: &str) -> Result<PublicIdentity> {
        match self.node.contacts().find(peer) {
            Lookup::Found(c) => Ok(c.key),
            Lookup::None => Err(fail(format!("no contact matches {peer:?}"))),
            Lookup::Ambiguous(_) => Err(fail(format!("{peer:?} matches several contacts"))),
        }
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Opens (creating on first use) the node stored in `home`. A
    /// passphrase protects a newly created identity and is required to
    /// open a protected one. When `tunnel_port` is set, the node offers
    /// WireGuard tunnels to mutually approved peers on that UDP port.
    #[uniffi::constructor]
    pub fn open(
        home: String,
        passphrase: Option<String>,
        tunnel_port: Option<u16>,
    ) -> Result<Arc<Self>> {
        let home = Home::new(home);
        let pw = passphrase.as_deref().map(str::as_bytes);
        let identity = if home.has_identity() {
            home.load_identity(pw).map_err(fail)?
        } else {
            home.create_identity(pw).map_err(fail)?
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(fail)?;
        let groups = GroupNode::load(&home, &identity).map_err(fail)?;
        let (node, events) = {
            let _guard = rt.enter();
            Node::new(NodeConfig {
                home,
                identity,
                policy: AcceptPolicy::Anyone,
                // Metadata protection is on unless the app turns it off.
                constant_rate: Some(Duration::from_millis(u64::from(DEFAULT_COVER_MS))),
                tunnel_port,
            })
            .map_err(fail)?
        };
        Ok(Arc::new(Self {
            rt,
            node,
            events: Mutex::new(events),
            groups: Mutex::new(groups),
            queued: Mutex::new(VecDeque::new()),
        }))
    }

    pub fn device_fingerprint(&self) -> String {
        fp(&self.node.identity())
    }

    pub fn account_fingerprint(&self) -> String {
        self.node.account().id().fingerprint().to_string()
    }

    /// `threnody://…` link others can connect with.
    pub fn invite_link(&self, addr: String) -> String {
        format!(
            "threnody://{}@{addr}",
            self.node.identity().fingerprint().compact()
        )
    }

    /// Starts listening; returns the bound address. QUIC (UDP) listens on
    /// the same port, for IPv4 and IPv6, and with it reaching contacts
    /// across the internet starts (unless turned off; see `set_reach_internet`).
    pub fn listen(&self, addr: String) -> Result<String> {
        let bound = self.rt.block_on(self.node.listen(&addr)).map_err(fail)?;
        let quic = self.rt.block_on(async {
            if !bound.ip().is_unspecified() {
                return self.node.listen_quic(&bound.to_string()).await;
            }
            match self
                .node
                .listen_quic(&format!("[::]:{}", bound.port()))
                .await
            {
                Ok(a) => Ok(a),
                Err(_) => {
                    self.node
                        .listen_quic(&format!("0.0.0.0:{}", bound.port()))
                        .await
                }
            }
        });
        // On loopback there is nobody to reach.
        if quic.is_ok() && !bound.ip().is_loopback() {
            let _guard = self.rt.enter();
            let _ = self
                .node
                .start_reach(threnody_net::reach::ReachConfig::default());
        }
        Ok(bound.to_string())
    }

    /// Reach contacts across the internet: addresses published (encrypted,
    /// for approved contacts only) in the public BitTorrent DHT, and hole
    /// punching through both sides' routers. On by default; strangers in
    /// the DHT see this device's IP address, not who it talks to. Always
    /// off for anonymous identities.
    pub fn set_reach_internet(&self, on: bool) {
        let _guard = self.rt.enter();
        self.node.set_reach(on);
    }

    pub fn reach_internet(&self) -> bool {
        self.node.reach_enabled()
    }

    pub fn reachability(&self) -> ReachInfo {
        let r = self.node.reachability();
        ReachInfo {
            enabled: r.enabled,
            online: r.online,
            addresses: r.candidates.iter().map(|c| c.addr.to_string()).collect(),
            symmetric: r.symmetric,
            punching: u32::try_from(r.punching.len()).unwrap_or(u32::MAX),
        }
    }

    /// Whether the app is in the foreground: contacts are looked for more
    /// often then.
    pub fn set_foreground(&self, on: bool) {
        self.node.set_foreground(on);
    }

    /// The default network changed (another Wi-Fi, mobile data). Call it
    /// the moment the platform says so: this side knows first, so it
    /// redials through the standby path (`to_standby`: the new default is
    /// the network the standby socket is bound to), tells contacts where
    /// it is now, and punches. `mobile`: the new default is mobile data.
    pub fn network_changed(&self, to_standby: bool, mobile: bool) {
        let _guard = self.rt.enter();
        self.node.set_on_mobile(mobile);
        self.node.network_switched(to_standby);
    }

    /// Makes the standby socket and returns its file descriptor; bind it
    /// to the network that isn't the default (mobile data while on Wi-Fi,
    /// or a Wi-Fi that just appeared while on mobile data) with
    /// `Network.bindSocket`, then call `standby_bound`.
    pub fn standby_socket(&self) -> Result<i32> {
        self.node.standby_socket().map_err(fail)
    }

    /// The standby socket is bound; `ipv6` are the standby network's
    /// global IPv6 addresses. Binding a new one replaces the old (sessions
    /// already running over the old one carry on).
    pub fn standby_bound(&self, ipv6: Vec<String>) -> Result<()> {
        let _guard = self.rt.enter();
        let ips = ipv6.iter().filter_map(|a| a.parse().ok()).collect();
        self.node.standby_bound(ips).map_err(fail)
    }

    /// The standby network went away (or became the default).
    pub fn standby_lost(&self) {
        self.node.standby_lost();
    }

    /// Keep holes open through the standby path, so losing Wi-Fi costs a
    /// fraction of a second. Costs mobile radio time: turn it on while the
    /// app is open, a chat was active recently, or Wi-Fi is weakening.
    pub fn set_standby_warm(&self, on: bool) {
        self.node.set_standby_warm(on);
    }

    /// Gap between keepalives on standby holes, in seconds.
    pub fn set_standby_keepalive(&self, secs: u32) {
        self.node
            .set_standby_keepalive(Duration::from_secs(u64::from(secs)));
    }

    /// Enables or disables WireGuard tunnel offering on `port`. When
    /// enabled, the node offers tunnels to mutually approved peers and
    /// emits `TunnelUp` events when peers offer tunnels to us. Changing
    /// this restarts tunnel state.
    pub fn set_tunnel_port(&self, port: Option<u16>) -> Result<()> {
        let _guard = self.rt.enter();
        self.node.set_tunnel_port(port).map_err(fail)
    }

    /// Returns the current tunnel port if enabled.
    pub fn tunnel_port(&self) -> Option<u16> {
        self.node.tunnel_port()
    }

    /// Returns this device's WireGuard static public key (base64) and
    /// overlay address. The overlay address is an IPv6 ULA in fd00::/8
    /// derived from the identity key.
    pub fn wireguard_info(&self) -> WireguardInfo {
        let keys = threnody_core::tunnel::WgKeys::derive(self.node.identity_ref());
        let overlay = threnody_core::tunnel::overlay_addr(&self.node.identity());
        WireguardInfo {
            public_key: keys.public_base64(),
            overlay: overlay.to_string(),
        }
    }

    /// Returns a wg-quick config for the current tunnel peers. The app
    /// should write this to a file and bring up the interface (Linux:
    /// `wg-quick up`; Android: pass to boringtun).
    pub fn wireguard_config(&self) -> String {
        let keys = threnody_core::tunnel::WgKeys::derive(self.node.identity_ref());
        let port = self.node.tunnel_port().unwrap_or(51820);
        let overlay = threnody_core::tunnel::overlay_addr(&self.node.identity());
        let mut s = format!(
            "# Generated by Threnody. Bring up with: wg-quick up <this file>\n\
[Interface]\n\
PrivateKey = {}\n\
ListenPort = {}\n\
Address = {}/128\n",
            keys.secret_base64().as_str(),
            port,
            overlay
        );
        for peer in self.node.tunnel_peers_full() {
            let psk = threnody_core::tunnel::base64(&peer.psk.0[..]);
            let peer_overlay = peer.overlay;
            s.push_str(&format!(
                "\n[Peer]\n\
PublicKey = {}\n\
PresharedKey = {}\n\
Endpoint = {}\n\
AllowedIPs = {}/128\n\
PersistentKeepalive = 25\n",
                threnody_core::tunnel::base64(&peer.wg_public),
                psk,
                peer.endpoint,
                peer_overlay
            ));
        }
        s
    }

    /// Applies current tunnel peers to a running WireGuard interface
    /// (Linux: `wg set`; Android: via boringtun). Requires
    /// `CAP_NET_ADMIN` or root on Linux; on Android this pushes to the
    /// userspace WireGuard implementation.
    pub fn wireguard_apply(&self, iface: String) -> Result<()> {
        let _guard = self.rt.enter();
        self.node.apply_wireguard(&iface).map_err(fail)
    }

    /// Starts the userspace WireGuard implementation (boringtun) for
    /// platforms without kernel WireGuard support (Android, iOS, unprivileged).
    /// Returns the local UDP port being listened on.
    #[cfg(feature = "boringtun")]
    pub fn wireguard_start_userspace(&self) -> Result<u16> {
        let _guard = self.rt.enter();
        self.node.start_wg_userspace().map_err(fail)
    }

    /// Stops the userspace WireGuard implementation.
    #[cfg(feature = "boringtun")]
    pub fn wireguard_stop_userspace(&self) {
        let _guard = self.rt.enter();
        self.node.stop_wg_userspace();
    }

    /// Sends a packet through the userspace WireGuard tunnel to a peer.
    #[cfg(feature = "boringtun")]
    pub fn wireguard_userspace_send(&self, peer: String, data: Vec<u8>) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.wg_userspace_send(&p, &data).map_err(fail)
    }

    /// We want to reach `peer` (its chat is open): look for it across the
    /// internet now.
    pub fn seek(&self, peer: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        let devices: Vec<PublicIdentity> = match self.node.account_of(&p) {
            Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
            None => vec![p],
        };
        for d in &devices {
            self.node.seek(d);
        }
        Ok(())
    }

    /// Connects to an invite link, `host:port`, contact or fingerprint;
    /// returns the peer's fingerprint. When the direct path fails (or
    /// there is none), it goes through approved relays, whatever links
    /// they are on (spec §7.3).
    pub fn connect(&self, target: String) -> Result<String> {
        let (addr, pin) = if let Some(rest) = target.strip_prefix("threnody://") {
            let (f, a) = rest
                .split_once('@')
                .ok_or_else(|| fail("bad invite link"))?;
            (
                Some(a.to_owned()),
                Some(f.parse::<Fingerprint>().map_err(fail)?),
            )
        } else if let Ok(f) = target.parse::<Fingerprint>() {
            let addr = self
                .node
                .contacts()
                .iter()
                .find(|c| c.key.fingerprint() == f)
                .and_then(|c| c.last_addr.clone());
            (addr, Some(f))
        } else if let Lookup::Found(c) = self.node.contacts().find(&target) {
            (c.last_addr.clone(), Some(c.key.fingerprint()))
        } else {
            (Some(target), None)
        };
        self.rt
            .block_on(self.node.reach(addr.as_deref(), pin))
            .map(|p| fp(&p))
            .map_err(fail)
    }

    /// Dials every mutually approved contact that isn't connected, at its
    /// last known address or through relays. Returns at once; sessions
    /// show up as `Connected` events. Apps call it on start and when the
    /// network comes back.
    pub fn reconnect(&self) {
        let live: Vec<PublicIdentity> = self.node.sessions().iter().map(|s| s.peer).collect();
        for c in self.node.contacts().iter() {
            if !c.mutually_approved() || live.contains(&c.key) {
                continue;
            }
            let (node, peer) = (self.node.clone(), c.key);
            self.rt.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(30), node.reach_peer(&peer)).await;
            });
        }
    }

    /// Sends text to every device of `peer`'s account: live where
    /// connected (reaching `peer` through relays if need be), sealed for
    /// mailboxes, or else held until a session comes up; recorded in
    /// history. Returns how many devices it reached or will.
    pub fn send_text(&self, peer: String, text: String) -> Result<u32> {
        let p = self.resolve(&peer)?;
        // No session: try to reach it before falling back to sealed delivery.
        self.node.seek(&p);
        self.try_reach(&p);
        let _guard = self.rt.enter();
        let r = self.node.send_text(&p, &text).map_err(fail)?;
        Ok(u32::try_from(r.live + r.sealed + r.queued).unwrap_or(u32::MAX))
    }

    /// Sends a file (at most `max_file_size` bytes) to `peer`, reaching
    /// them directly or through relays; fails if they can't be reached.
    /// It is recorded in history with `location`, where the file lives on
    /// this device (a path or URI).
    pub fn send_file(
        &self,
        peer: String,
        name: String,
        data: Vec<u8>,
        location: Option<String>,
        options: FileOptions,
    ) -> Result<()> {
        let p = self.resolve(&peer)?;
        if data.len() > threnody_core::message::MAX_FILE {
            return Err(fail(format!(
                "file larger than {} bytes",
                threnody_core::message::MAX_FILE
            )));
        }
        // Files aren't sealed for mailboxes: they wait for a live session.
        self.try_reach(&p);
        let _guard = self.rt.enter();
        self.node
            .send_file(
                &p,
                OutgoingFile {
                    name,
                    data,
                    location,
                    sensitive: options.sensitive,
                    caption: options.caption,
                    album: options.album,
                },
            )
            .map_err(fail)
    }

    /// Records a received file (from a `File` event) in history once the
    /// app has saved it at `location`.
    pub fn record_received_file(
        &self,
        peer: String,
        name: String,
        size: u64,
        location: Option<String>,
        id: u64,
        options: FileOptions,
    ) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.record_received_file(
            &p,
            FileNote {
                name,
                size,
                location,
                sensitive: options.sensitive,
                album: options.album,
            },
            &options.caption,
            id,
        );
        Ok(())
    }

    /// Whether images lose their metadata (location, camera, times)
    /// before they are sent. On by default.
    pub fn set_strip_metadata(&self, on: bool) {
        self.node.set_strip_metadata(on);
    }

    pub fn strip_metadata(&self) -> bool {
        self.node.strip_metadata()
    }

    /// Deletes messages (by `HistoryEntry.id`) with `peer`, here and on
    /// our other devices; with `everyone`, our own among them also on
    /// `peer`'s devices that support it. Returns how many went here.
    pub fn delete_messages(&self, peer: String, ids: Vec<u64>, everyone: bool) -> Result<u32> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        let n = self.node.delete_messages(&p, &ids, everyone);
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Edits our own text message `id` to `peer`, everywhere that supports
    /// it. False if there's no such message of ours.
    pub fn edit_message(&self, peer: String, id: u64, body: String) -> Result<bool> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        Ok(self.node.edit_message(&p, id, &body))
    }

    /// Deletes one entry without an id (older messages, group messages)
    /// from `peer`'s conversation, on this device.
    pub fn delete_entry(&self, peer: String, at_ms: u64, device: String) -> Result<u32> {
        let p = self.resolve(&peer)?;
        let conv = self.node.conversation_for(&p);
        Ok(self.delete_any(conv, at_ms, &device))
    }

    /// The largest file `send_file` accepts.
    pub fn max_file_size(&self) -> u64 {
        threnody_core::message::MAX_FILE as u64
    }

    /// The last `limit` messages with `peer` (oldest first).
    pub fn history(&self, peer: String, limit: u32) -> Result<Vec<HistoryEntry>> {
        let p = self.resolve(&peer)?;
        let h = self
            .node
            .history(self.node.conversation_for(&p))
            .map_err(fail)?;
        let me = self.node.reactor_of(&self.node.identity());
        Ok(history_entries(h.recent(limit as usize), me))
    }

    /// The disappearing timer messages with `peer` get now (`None` = off):
    /// its own setting, else the default.
    pub fn disappearing(&self, peer: String) -> Result<Option<u32>> {
        Ok(self.node.timer(&self.resolve(&peer)?))
    }

    /// The timer for chats without their own (on by default: a week).
    pub fn default_disappearing(&self) -> Option<u32> {
        self.node.default_timer()
    }

    pub fn set_default_disappearing(&self, seconds: Option<u32>) {
        self.node.set_default_timer(seconds);
    }

    /// Sets the disappearing-message timer with `peer` (`None` = off).
    pub fn set_disappearing(&self, peer: String, seconds: Option<u32>) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.set_timer(&p, seconds).map_err(fail)
    }

    /// Accepts `peer`'s message requests (its whole account).
    pub fn accept_contact(&self, peer: String) -> Result<()> {
        self.node.accept_contact(&self.resolve(&peer)?);
        Ok(())
    }

    /// Blocks `peer` (its whole account): sessions refused, conversation
    /// deleted. Approving it again unblocks.
    pub fn block_contact(&self, peer: String) -> Result<()> {
        self.node.block_contact(&self.resolve(&peer)?);
        Ok(())
    }

    /// Deletes a message request; they can write again as a new request.
    pub fn delete_request(&self, peer: String) -> Result<()> {
        self.node.delete_request(&self.resolve(&peer)?);
        Ok(())
    }

    /// Adds (or with `add` false, takes away) our `emoji` on message `id`
    /// (`HistoryEntry.id`) in our chat with `peer`. Several emoji each are
    /// fine. Returns false if there's no such message.
    pub fn react(&self, peer: String, id: u64, emoji: String, add: bool) -> Result<bool> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        Ok(self.node.react(&p, id, &emoji, add))
    }

    /// Clears the messages with `peer`, here and on our other devices;
    /// the contact stays.
    pub fn clear_conversation(&self, peer: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.clear_conversation(&p);
        Ok(())
    }

    /// Deletes the conversation with `peer`: the contact (every device of
    /// its account) and the history, here and on our other devices. They
    /// can write again, as a new request.
    pub fn delete_conversation(&self, peer: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.delete_conversation(&p);
        Ok(())
    }

    pub fn set_approval(&self, peer: String, approved: bool) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.set_approval(&p, approved).map_err(fail)
    }

    /// Tells `peer` whether we are typing in its conversation with us.
    /// Rides the same session — direct, relayed or over the internet — so
    /// it reaches them even on an LTE/5G-only connection.
    pub fn set_typing(&self, peer: String, active: bool) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.set_typing(&p, active).map_err(fail)
    }

    /// Marks the incoming messages of `peer` we've now displayed —
    /// `ids` are their sender-ids — and tells `peer`'s devices, so our
    /// own ticks can show "seen". No-op without a live session.
    pub fn report_read(&self, peer: String, ids: Vec<u64>) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.report_read(&p, &ids).map_err(fail)
    }

    pub fn set_name(&self, peer: String, name: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.update_contacts(|c| {
            if let Some(c) = c.get_mut(&p) {
                c.petname = Some(name);
            }
        });
        Ok(())
    }

    pub fn contacts(&self) -> Vec<ContactInfo> {
        let live: Vec<PublicIdentity> = self.node.sessions().iter().map(|s| s.peer).collect();
        self.node
            .contacts()
            .iter()
            .map(|c| ContactInfo {
                fingerprint: fp(&c.key),
                // Our name for it, else the name it shares once accepted.
                name: c.petname.clone().or_else(|| {
                    c.shared_name()
                        .filter(|_| c.accepted && !c.blocked)
                        .map(str::to_owned)
                }),
                shared_profile: persona::attrs(&c.profile),
                revealed: c.revealed.as_ref().map(|(id, _)| fp(id)),
                revealed_invite: c.revealed.as_ref().and_then(|(_, i)| i.clone()),
                mutually_approved: c.mutually_approved(),
                verified: c.verified,
                account: c.account.map(|a| a.fingerprint().to_string()),
                connected: live.contains(&c.key),
                accepted: self.node.is_accepted(&c.key),
                blocked: c.blocked,
                local_approved: c.local_approved,
            })
            .collect()
    }

    /// The 60-digit safety number to compare with `peer` out of band.
    pub fn safety_number(&self, peer: String) -> Result<String> {
        let p = self.resolve(&peer)?;
        Ok(safety_number(&self.node.identity(), &p))
    }

    /// Records that the safety number with `peer` was compared out of band.
    pub fn mark_verified(&self, peer: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.update_contacts(|c| {
            if let Some(c) = c.get_mut(&p) {
                c.verified = true;
            }
        });
        Ok(())
    }

    /// Cover traffic (spec §9 layer 1): each session sends one padded frame
    /// every `interval_ms`, filler when idle, so timing shows nothing.
    /// `None` turns it off. Each frame is about 2.7 kB. On by default.
    pub fn set_cover_traffic(&self, interval_ms: Option<u32>) {
        self.node
            .set_constant_rate(interval_ms.map(|ms| Duration::from_millis(u64::from(ms.max(10)))));
    }

    pub fn cover_traffic_ms(&self) -> Option<u32> {
        self.node
            .constant_rate()
            .map(|d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX))
    }

    /// Reach contacts through onion circuits first when two approved
    /// relays make one possible (spec §9 layer 2). On by default.
    pub fn set_onion_first(&self, on: bool) {
        self.node.set_prefer_onion(on);
    }

    pub fn onion_first(&self) -> bool {
        self.node.prefer_onion()
    }

    /// The devices of our account, in the order they joined.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let me = self.node.identity();
        self.node
            .account()
            .state()
            .devices
            .iter()
            .map(|(d, name)| DeviceInfo {
                fingerprint: fp(d),
                name: name.clone(),
                this_device: *d == me,
            })
            .collect()
    }

    /// Renames a device of our account (`device` = fingerprint, or ours);
    /// every sibling and contact learns the name.
    pub fn rename_device(&self, device: String, name: String) -> Result<()> {
        let d = self
            .node
            .account()
            .state()
            .devices
            .iter()
            .map(|(d, _)| *d)
            .find(|d| fp(d) == device || d.fingerprint().compact() == device)
            .ok_or_else(|| fail("not a device of this account"))?;
        let _guard = self.rt.enter();
        self.node.rename_device(&d, &name).map_err(fail)
    }

    /// A one-time code for a new device to join this account.
    pub fn create_link_code(&self, addr: String) -> String {
        self.node.create_link_code(addr).to_string()
    }

    /// Joins the account of the device that showed `code`; returns the
    /// account fingerprint.
    pub fn link_with(&self, code: String) -> Result<String> {
        let code = code.parse().map_err(fail)?;
        self.rt
            .block_on(self.node.link_with(&code))
            .map(|a| a.fingerprint().to_string())
            .map_err(fail)
    }

    /// Runs a session over a platform byte pipe. `outbound` picks the
    /// handshake role (the side that opened the connection initiates).
    /// `transport` labels it ("ble", …); `expect` pins a fingerprint.
    /// The outcome arrives as a `Connected` (or `Other`) event.
    pub fn attach_link(
        &self,
        link: Arc<dyn ByteLink>,
        outbound: bool,
        transport: String,
        remote: String,
        expect: Option<String>,
    ) -> Result<Arc<LinkHandle>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let expect = expect
            .map(|f| f.parse::<Fingerprint>())
            .transpose()
            .map_err(fail)?;
        let label: &'static str = match transport.as_str() {
            "ble" => "ble",
            "wifi-direct" => "wifi-direct",
            _ => "link",
        };
        let (ours, theirs) = tokio::io::duplex(1 << 18);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let handle = Arc::new(LinkHandle {
            tx: Mutex::new(Some(tx)),
        });
        // Pump: session bytes -> platform, platform bytes -> session.
        let pump_link = Arc::clone(&link);
        self.rt.spawn(async move {
            let (mut rd, mut wr) = tokio::io::split(ours);
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                tokio::select! {
                    n = rd.read(&mut buf) => {
                        let Ok(n) = n else { break };
                        if n == 0 { break }
                        let data = buf[..n].to_vec();
                        let l = Arc::clone(&pump_link);
                        let ok = tokio::task::spawn_blocking(move || l.send(data)).await.unwrap_or(false);
                        if !ok { break }
                    }
                    inbound = rx.recv() => match inbound {
                        Some(d) => if wr.write_all(&d).await.is_err() { break },
                        None => break,
                    },
                }
            }
            let l = Arc::clone(&pump_link);
            let _ = tokio::task::spawn_blocking(move || l.disconnect()).await;
        });
        let node = self.node.clone();
        self.rt.spawn(async move {
            let r = if outbound {
                node.connect_stream(theirs, label, remote, expect).await
            } else {
                node.accept_stream(theirs, label, remote).await
            };
            if r.is_err() {
                link.disconnect();
            }
        });
        Ok(handle)
    }

    /// Service data to advertise under the Threnody Bluetooth UUID: a
    /// private beacon carrying `psm`. Replace it at least every minute.
    pub fn ble_beacon(&self, psm: u16) -> Vec<u8> {
        self.node.ble_beacon(psm)
    }

    /// The PSM in any Threnody advert (readable without being a contact).
    pub fn ble_advert_psm(&self, data: Vec<u8>) -> Option<u16> {
        threnody_core::discovery::beacon_port(&data)
    }

    /// Checks heard service data. Returns the contact to dial (with
    /// `attach_link(outbound = true, expect = peer)`) if it is an approved
    /// contact this device should connect to now.
    pub fn ble_heard(&self, data: Vec<u8>) -> Option<BleDial> {
        self.node.ble_heard(&data).map(|(peer, psm)| BleDial {
            peer: peer.fingerprint().to_string(),
            psm,
        })
    }

    /// Offers `peer` (a nearby, mutually approved contact) the Wi-Fi Direct
    /// group we created; `addr` is where we listen inside it.
    pub fn offer_wifi_direct(
        &self,
        peer: String,
        ssid: String,
        passphrase: String,
        addr: String,
    ) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node
            .offer_wifi_direct(
                &p,
                threnody_net::DirectOffer {
                    ssid,
                    passphrase,
                    addr,
                },
            )
            .map_err(fail)
    }

    /// Asks `peer` to create a Wi-Fi Direct group and offer it to us.
    pub fn request_wifi_direct(&self, peer: String) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.request_wifi_direct(&p).map_err(fail)
    }

    /// Waits up to `timeout_ms` for the next event.
    pub fn next_event(&self, timeout_ms: u32) -> Option<NodeEvent> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            if let Some(e) = self.queued().pop_front() {
                return Some(e);
            }
            let next = {
                let mut rx = self
                    .events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Build the timer inside the runtime (it needs the reactor).
                self.rt
                    .block_on(async { tokio::time::timeout_at(deadline, rx.recv()).await })
                    .ok()
                    .flatten()?
            };
            // Group traffic goes to the MLS engine; what it yields is queued.
            match next {
                Event::Message {
                    peer,
                    msg: AppMessage::Group(payload),
                }
                | Event::OfflineMessage {
                    from: peer,
                    msg: AppMessage::Group(payload),
                    ..
                } => self.group_incoming(peer, &payload),
                // A member acknowledged a group message we forwarded: the
                // sender gets a receipt; nothing for the app to show.
                Event::Delivered {
                    peer,
                    local_id,
                    group: Some(group),
                    relay_for: Some(origin),
                } => {
                    let _guard = self.rt.enter();
                    self.group_node()
                        .relayed(&self.node, &peer, &group, local_id, &origin);
                }
                other => {
                    if let Event::Connected { peer, .. } = &other {
                        self.group_connected(peer);
                    }
                    return Some(convert(other));
                }
            }
        }
    }

    /// Stops listening and closes every session.
    pub fn shutdown(&self) {
        self.node.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(node: &ThrenodyNode, pred: impl Fn(&NodeEvent) -> bool) -> NodeEvent {
        for _ in 0..200 {
            if let Some(e) = node.next_event(100)
                && pred(&e)
            {
                return e;
            }
        }
        panic!("event did not arrive");
    }

    /// Renders `text` as a QR code into a `w`×`h` greyscale frame with
    /// `scale`-pixel modules, offset and with some noise, as a camera sees it.
    fn frame(text: &str, w: usize, h: usize, scale: usize, stride: usize) -> Vec<u8> {
        let q = qr_matrix(text.into()).unwrap();
        let n = q.size as usize;
        // Uneven light (a gradient) and sensor noise (±8, from an LCG).
        let mut seed = 0x2545_f491_u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (seed >> 16) as usize % 17
        };
        let mut img = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                img[y * stride + x] = (150 + (x + y) / 16 + noise()).min(255) as u8;
            }
        }
        let (ox, oy) = (37, 23);
        for y in 0..n * scale {
            for x in 0..n * scale {
                if q.dark[(y / scale) * n + x / scale] {
                    img[(oy + y) * stride + ox + x] = (30 + noise()) as u8;
                }
            }
        }
        img
    }

    #[test]
    fn qr_codes_decode_from_camera_frames() {
        let link = "threnody://Q3XRE4SF7Q5CWSTAFB7MZ58T6BJBCN55@192.168.100.107:7450";
        let (w, h, stride) = (640, 480, 704);
        let img = frame(link, w, h, 6, stride);
        assert_eq!(
            decode_qr(w as u32, h as u32, stride as u32, img.clone()).as_deref(),
            Some(link)
        );
        // No code, or a truncated buffer: nothing, and no panic.
        assert_eq!(
            decode_qr(w as u32, h as u32, stride as u32, vec![128; stride * h]),
            None
        );
        assert_eq!(
            decode_qr(w as u32, h as u32, stride as u32, img[..1000].to_vec()),
            None
        );
        assert_eq!(decode_qr(0, 0, 0, vec![]), None);
    }

    #[test]
    fn qr_matrix_is_square() {
        let q = qr_matrix("threnody://ABC@192.0.2.7:7450".into()).unwrap();
        assert!(q.size >= 21);
        assert_eq!(q.dark.len(), (q.size * q.size) as usize);
        assert!(q.dark[0], "finder pattern corner is dark");
    }

    #[test]
    fn personas_share_profiles_and_reveal_through_apps() {
        let dir = tempfile::tempdir().unwrap();
        let me =
            ThrenodyNode::open(dir.path().join("me").display().to_string(), None, None).unwrap();
        let bob =
            ThrenodyNode::open(dir.path().join("b").display().to_string(), None, None).unwrap();
        let addr = bob.listen("127.0.0.1:0".into()).unwrap();
        let rec = me
            .create_persona("market".into(), None, Some("pw".into()))
            .unwrap();
        assert_eq!(me.personas().unwrap(), vec![rec.clone()]);
        let p = ThrenodyNode::open(rec.home.clone(), Some("pw".into()), None).unwrap();
        assert!(p.is_persona() && !me.is_persona() && p.personas().is_err());
        assert_ne!(p.device_fingerprint(), me.device_fingerprint());

        let b_fp = p.connect(bob.invite_link(addr)).unwrap();
        let p_fp = p.device_fingerprint();
        wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));
        bob.accept_contact(p_fp.clone()).unwrap();
        p.set_profile(vec![
            ProfileAttr {
                key: "name".into(),
                value: "Stall 12".into(),
            },
            ProfileAttr {
                key: "email".into(),
                value: "s@example.org".into(),
            },
        ])
        .unwrap();
        p.set_shared_with(b_fp.clone(), vec!["name".into()])
            .unwrap();
        assert_eq!(
            p.shared_with(b_fp.clone()).unwrap(),
            vec!["name".to_owned()]
        );
        wait(&bob, |e| matches!(e, NodeEvent::ProfileChanged { .. }));
        let seen = bob
            .contacts()
            .into_iter()
            .find(|c| c.fingerprint == p_fp)
            .unwrap();
        assert_eq!(seen.name.as_deref(), Some("Stall 12"));
        assert_eq!(seen.shared_profile.len(), 1);

        me.reveal_through(p.clone(), b_fp, Some("threnody://ME@10.0.0.2:7450".into()))
            .unwrap();
        let e = wait(&bob, |e| matches!(e, NodeEvent::IdentityRevealed { .. }));
        assert_eq!(
            e,
            NodeEvent::IdentityRevealed {
                peer: p_fp.clone(),
                identity: me.device_fingerprint(),
                invite: Some("threnody://ME@10.0.0.2:7450".into()),
            }
        );
        let seen = bob
            .contacts()
            .into_iter()
            .find(|c| c.fingerprint == p_fp)
            .unwrap();
        assert_eq!(seen.revealed, Some(me.device_fingerprint()));

        p.shutdown();
        drop(p);
        me.burn_persona(rec.id).unwrap();
        assert!(me.personas().unwrap().is_empty());
        assert!(!std::path::Path::new(&rec.home).exists());
    }

    #[test]
    fn two_embedded_nodes_chat() {
        let dir = tempfile::tempdir().unwrap();
        let alice =
            ThrenodyNode::open(dir.path().join("a").display().to_string(), None, None).unwrap();
        let bob = ThrenodyNode::open(
            dir.path().join("b").display().to_string(),
            Some("pw".into()),
            None,
        )
        .unwrap();
        let addr = bob.listen("127.0.0.1:0".into()).unwrap();
        let bob_fp = alice.connect(bob.invite_link(addr)).unwrap();
        assert_eq!(bob_fp, bob.device_fingerprint());
        // Metadata protection is on unless turned off.
        assert_eq!(alice.cover_traffic_ms(), Some(DEFAULT_COVER_MS));
        assert!(alice.onion_first());
        wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));

        alice
            .send_text(bob_fp.clone(), "hello from an app".into())
            .unwrap();
        // Bob never contacted Alice: her first message is a request.
        let e = wait(&bob, |e| matches!(e, NodeEvent::MessageRequest { .. }));
        assert_eq!(
            e,
            NodeEvent::MessageRequest {
                peer: alice.device_fingerprint(),
                text: Some("hello from an app".into()),
                file: None,
            }
        );
        assert!(bob.contacts().iter().any(|c| !c.accepted));
        bob.accept_contact(alice.device_fingerprint()).unwrap();
        assert!(bob.contacts().iter().all(|c| c.accepted));
        alice
            .send_text(bob_fp.clone(), "hello from an app".into())
            .unwrap();
        let e = wait(&bob, |e| matches!(e, NodeEvent::Message { .. }));
        assert_eq!(
            e,
            NodeEvent::Message {
                peer: alice.device_fingerprint(),
                text: "hello from an app".into(),
                offline: false
            }
        );
        alice
            .send_file(
                bob_fp.clone(),
                "notes.txt".into(),
                b"some notes".to_vec(),
                Some("/tmp/notes.txt".into()),
                FileOptions {
                    sensitive: true,
                    caption: "read these".into(),
                    album: 0,
                },
            )
            .unwrap();
        let f = wait(&bob, |e| matches!(e, NodeEvent::File { .. }));
        assert!(
            matches!(f, NodeEvent::File { name, data, sensitive: true, caption, .. }
                if name == "notes.txt" && data == b"some notes" && caption == "read these")
        );
        let too_big = vec![0u8; usize::try_from(alice.max_file_size()).unwrap() + 1];
        assert!(
            alice
                .send_file(
                    bob_fp.clone(),
                    "big".into(),
                    too_big,
                    None,
                    FileOptions::default()
                )
                .is_err()
        );
        bob.record_received_file(
            alice.device_fingerprint(),
            "notes.txt".into(),
            10,
            None,
            0,
            FileOptions {
                sensitive: true,
                caption: "read these".into(),
                album: 0,
            },
        )
        .unwrap();
        let last = bob
            .history(alice.device_fingerprint(), 1)
            .unwrap()
            .pop()
            .unwrap();
        assert!(last.text == "read these" && last.file.unwrap().sensitive);
        alice.mark_verified(bob_fp.clone()).unwrap();
        assert!(
            alice
                .contacts()
                .iter()
                .any(|c| c.fingerprint == bob_fp && c.verified)
        );
        alice.set_approval(bob_fp.clone(), true).unwrap();
        bob.set_approval(alice.device_fingerprint(), true).unwrap();
        wait(&alice, |e| {
            matches!(e, NodeEvent::ApprovalChanged { mutual: true, .. })
        });
        let contacts = alice.contacts();
        assert!(
            contacts
                .iter()
                .any(|c| c.fingerprint == bob_fp && c.mutually_approved && c.connected)
        );
        let h = alice.history(bob_fp.clone(), 10).unwrap();
        assert_eq!(h.len(), 3, "the request, the same text again, the file");
        assert!(h[0].outgoing && h[0].text == "hello from an app");
        let f = h[2].file.as_ref().unwrap();
        assert!(h[2].outgoing && f.name == "notes.txt" && f.size == 10);
        // Bob acknowledged both (the Delivered events went by above).
        for _ in 0..100 {
            if alice
                .history(bob_fp.clone(), 10)
                .unwrap()
                .iter()
                .all(|e| e.delivered)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let h = alice.history(bob_fp.clone(), 10).unwrap();
        assert!(h.iter().all(|e| e.delivered), "{h:?}");
        assert_eq!(f.location.as_deref(), Some("/tmp/notes.txt"));
        let hb = bob.history(alice.device_fingerprint(), 10).unwrap();
        assert!(!hb[2].outgoing && hb[2].file.as_ref().is_some_and(|f| f.name == "notes.txt"));
        assert_eq!(
            bob.history(alice.device_fingerprint(), 10).unwrap()[0].text,
            "hello from an app"
        );
        assert_eq!(
            alice.safety_number(bob_fp.clone()).unwrap(),
            bob.safety_number(alice.device_fingerprint()).unwrap()
        );
        alice.shutdown();
        bob.shutdown();

        // A session over a foreign byte pipe (as Android's L2CAP sockets use).
        struct Pipe(Mutex<Option<Arc<LinkHandle>>>);
        impl ByteLink for Pipe {
            fn send(&self, data: Vec<u8>) -> bool {
                match self.0.lock().unwrap().as_ref() {
                    Some(h) => {
                        h.receive(data);
                        true
                    }
                    None => false,
                }
            }
            fn disconnect(&self) {
                if let Some(h) = self.0.lock().unwrap().take() {
                    h.closed();
                }
            }
        }
        let (to_bob, to_alice) = (
            Arc::new(Pipe(Mutex::new(None))),
            Arc::new(Pipe(Mutex::new(None))),
        );
        let ha = alice
            .attach_link(
                to_bob.clone(),
                true,
                "ble".into(),
                "bob-radio".into(),
                Some(bob.device_fingerprint()),
            )
            .unwrap();
        let hb = bob
            .attach_link(
                to_alice.clone(),
                false,
                "ble".into(),
                "alice-radio".into(),
                None,
            )
            .unwrap();
        *to_bob.0.lock().unwrap() = Some(hb);
        *to_alice.0.lock().unwrap() = Some(ha);
        let e = wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));
        assert_eq!(
            e,
            NodeEvent::Connected {
                peer: alice.device_fingerprint(),
                via: None
            }
        );
        alice
            .send_text(bob.device_fingerprint(), "over the radio".into())
            .unwrap();
        let m = wait(&bob, |e| matches!(e, NodeEvent::Message { .. }));
        assert!(matches!(m, NodeEvent::Message { text, .. } if text == "over the radio"));

        // A protected identity needs its passphrase.
        drop(bob);
        let b_home = dir.path().join("b").display().to_string();
        assert!(ThrenodyNode::open(b_home.clone(), None, None).is_err());
        assert!(identity_is_sealed(b_home.clone()).unwrap());
        assert!(!identity_is_sealed(dir.path().join("nobody").display().to_string()).unwrap());

        // Sealing an existing identity keeps it (as an app does when it
        // moves the key under the platform keystore).
        let a_home = dir.path().join("a").display().to_string();
        let a_fp = alice.device_fingerprint();
        drop(alice);
        assert!(!identity_is_sealed(a_home.clone()).unwrap());
        change_passphrase(a_home.clone(), None, Some("from keystore".into())).unwrap();
        assert!(identity_is_sealed(a_home.clone()).unwrap());
        assert!(change_passphrase(a_home.clone(), Some("wrong".into()), None).is_err());
        let alice = ThrenodyNode::open(a_home, Some("from keystore".into()), None).unwrap();
        assert_eq!(alice.device_fingerprint(), a_fp);
        assert_eq!(
            alice.history(bob_fp.clone(), 10).unwrap()[0].text,
            "hello from an app",
            "state still readable"
        );
    }

    /// Pumps several nodes' events (every node must run for group
    /// handshakes to progress), keeping each node's events until asked for.
    struct Pump<'a> {
        nodes: Vec<&'a ThrenodyNode>,
        seen: Vec<VecDeque<NodeEvent>>,
    }

    impl<'a> Pump<'a> {
        fn new(nodes: &[&'a ThrenodyNode]) -> Self {
            Self {
                nodes: nodes.to_vec(),
                seen: vec![VecDeque::new(); nodes.len()],
            }
        }

        /// Runs every node for a while, keeping their events.
        fn settle(&mut self) {
            for _ in 0..40 {
                for (i, n) in self.nodes.iter().enumerate() {
                    if let Some(e) = n.next_event(5) {
                        self.seen[i].push_back(e);
                    }
                }
            }
        }

        /// The first event of node `target` matching `pred` (earlier ones are dropped).
        fn until(&mut self, target: usize, pred: impl Fn(&NodeEvent) -> bool) -> NodeEvent {
            for _ in 0..400 {
                while let Some(e) = self.seen[target].pop_front() {
                    if pred(&e) {
                        return e;
                    }
                }
                for (i, n) in self.nodes.iter().enumerate() {
                    if let Some(e) = n.next_event(5) {
                        self.seen[i].push_back(e);
                    }
                }
            }
            panic!("event did not arrive");
        }
    }

    #[test]
    fn reconnect_redials_approved_contacts_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = |n: &str| dir.path().join(n).display().to_string();
        let alice = ThrenodyNode::open(path("a"), None, None).unwrap();
        let addr = alice.listen("127.0.0.1:0".into()).unwrap();
        let bob = ThrenodyNode::open(path("b"), None, None).unwrap();
        let a_fp = bob.connect(alice.invite_link(addr)).unwrap();
        let b_fp = bob.device_fingerprint();
        wait(&alice, |e| matches!(e, NodeEvent::Connected { .. }));
        alice.set_approval(b_fp.clone(), true).unwrap();
        bob.set_approval(a_fp.clone(), true).unwrap();
        wait(&bob, |e| {
            matches!(e, NodeEvent::ApprovalChanged { mutual: true, .. })
        });

        bob.shutdown();
        drop(bob);
        let bob = ThrenodyNode::open(path("b"), None, None).unwrap();
        assert!(bob.contacts().iter().all(|c| !c.connected));
        bob.reconnect();
        let e = wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));
        assert!(matches!(e, NodeEvent::Connected { peer, .. } if peer == a_fp));
    }

    #[test]
    fn groups_invite_chat_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        // Group handshakes take many round trips: opt out of cover traffic
        // (on by default) so this test about group logic runs quickly.
        let open = |n: &str| {
            let node =
                ThrenodyNode::open(dir.path().join(n).display().to_string(), None, None).unwrap();
            node.set_cover_traffic(None);
            node
        };
        let (alice, bob, carol) = (open("a"), open("b"), open("c"));
        let addr = alice.listen("127.0.0.1:0".into()).unwrap();
        let a_fp = alice.device_fingerprint();
        for n in [&bob, &carol] {
            n.connect(alice.invite_link(addr.clone())).unwrap();
        }
        let mut all = Pump::new(&[&*alice, &*bob, &*carol]);
        let (b_fp, c_fp) = (bob.device_fingerprint(), carol.device_fingerprint());
        all.until(
            0,
            |e| matches!(e, NodeEvent::Connected { peer, .. } if *peer == c_fp),
        );
        // Bob approves Alice mutually, so he joins without asking.
        alice.set_approval(b_fp.clone(), true).unwrap();
        bob.set_approval(a_fp.clone(), true).unwrap();
        all.until(1, |e| {
            matches!(e, NodeEvent::ApprovalChanged { mutual: true, .. })
        });

        let g = alice.create_group("climbing".into()).unwrap();
        alice.invite_to_group(g.clone(), b_fp.clone()).unwrap();
        let joined = all.until(1, |e| matches!(e, NodeEvent::GroupJoined { .. }));
        assert_eq!(
            joined,
            NodeEvent::GroupJoined {
                group: g.clone(),
                name: "climbing".into(),
                owner: a_fp.clone()
            }
        );

        // Carol isn't approved: she is asked first.
        alice.invite_to_group(g.clone(), c_fp.clone()).unwrap();
        all.until(
            2,
            |e| matches!(e, NodeEvent::GroupInvited { from, .. } if *from == a_fp),
        );
        assert_eq!(carol.group_invites().len(), 1);
        // The invitation survives Carol restarting before she answers.
        drop(all);
        carol.shutdown();
        drop(carol);
        let carol = open("c");
        assert_eq!(carol.group_invites()[0].name, "climbing");
        carol.connect(alice.invite_link(addr.clone())).unwrap();
        let mut all = Pump::new(&[&*alice, &*bob, &*carol]);
        carol.accept_group_invite(g[..6].into()).unwrap();
        all.until(2, |e| matches!(e, NodeEvent::GroupJoined { .. }));
        all.until(1, |e| matches!(e, NodeEvent::GroupMembersChanged { added, .. } if *added == [c_fp.clone()]));
        assert!(carol.group_invites().is_empty());
        let info = &bob.groups()[0];
        assert_eq!(info.members.len(), 3);
        assert!(!info.owned && alice.groups()[0].owned);

        // Carol has no session with Bob and no relay they both approve:
        // Alice, the owner, forwards her messages to him.
        carol.send_group_text(g.clone(), "hi all".into()).unwrap();
        for i in [0, 1] {
            let m = all.until(i, |e| matches!(e, NodeEvent::GroupMessage { .. }));
            assert_eq!(
                m,
                NodeEvent::GroupMessage {
                    group: g.clone(),
                    from: c_fp.clone(),
                    text: "hi all".into(),
                    ours: false,
                }
            );
        }
        assert_eq!(bob.group_history(g.clone(), 10).unwrap()[0].text, "hi all");
        assert!(carol.group_history(g.clone(), 10).unwrap()[0].outgoing);
        // Alice acknowledges her own copy, and forwards Bob's: when Bob
        // acknowledges it, Alice sends Carol a receipt. Both count.
        let mine = &carol.group_history(g.clone(), 10).unwrap()[0];
        assert_eq!(mine.recipients, 2);
        for _ in 0..2 {
            all.until(
                2,
                |e| matches!(e, NodeEvent::Delivered { group: Some(x), .. } if *x == g),
            );
        }
        let mine = &carol.group_history(g.clone(), 10).unwrap()[0];
        assert!(mine.delivered_to == 2 && mine.delivered, "{mine:?}");
        // The owner reaches both directly: delivered.
        alice
            .send_group_text(g.clone(), "from the owner".into())
            .unwrap();
        for i in [1, 2] {
            all.until(
                i,
                |e| matches!(e, NodeEvent::GroupMessage { text, .. } if text == "from the owner"),
            );
        }
        all.until(0, |e| matches!(e, NodeEvent::Delivered { .. }));
        all.until(0, |e| matches!(e, NodeEvent::Delivered { .. }));
        let sent = alice.group_history(g.clone(), 10).unwrap();
        let sent = sent.last().unwrap();
        assert!(
            sent.delivered && sent.delivered_to == 2 && sent.recipients == 2,
            "{sent:?}"
        );

        // Files go to the group the same way; each member saves, then records.
        alice
            .send_group_file(
                g.clone(),
                "route.gpx".into(),
                vec![7; 3000],
                None,
                FileOptions {
                    sensitive: true,
                    caption: "tomorrow".into(),
                    album: 0,
                },
            )
            .unwrap();
        for (i, member) in [(1, &bob), (2, &carol)] {
            let NodeEvent::GroupFile {
                id,
                group,
                from,
                name,
                data,
                ours,
                sensitive,
                caption,
                album,
            } = all.until(i, |e| matches!(e, NodeEvent::GroupFile { .. }))
            else {
                unreachable!()
            };
            assert_eq!(
                (&group, &from, name.as_str(), data.len(), ours),
                (&g, &a_fp, "route.gpx", 3000, false)
            );
            assert!(sensitive && caption == "tomorrow" && album == 0);
            let options = FileOptions {
                sensitive,
                caption,
                album,
            };
            member
                .record_received_group_file(group, from, name, 3000, Some("/x".into()), id, options)
                .unwrap();
            let h = member.group_history(g.clone(), 10).unwrap();
            let f = h.last().unwrap().file.clone().unwrap();
            assert_eq!((f.name.as_str(), f.size), ("route.gpx", 3000));
            assert!(f.sensitive && h.last().unwrap().text == "tomorrow");
            assert_eq!(
                h.last().unwrap().id,
                id,
                "group messages carry the sender's id"
            );
        }
        // Bob reacts twice to Alice's file; everyone sees both, by Bob.
        let id = bob.group_history(g.clone(), 10).unwrap().last().unwrap().id;
        assert!(
            bob.react_in_group(g.clone(), id, "👍".into(), true)
                .unwrap()
        );
        assert!(
            bob.react_in_group(g.clone(), id, "🎉".into(), true)
                .unwrap()
        );
        for _ in 0..2 {
            all.until(0, |e| {
                matches!(e, NodeEvent::Reacted { group: Some(_), .. })
            });
        }
        let mine = alice.group_history(g.clone(), 10).unwrap();
        let r = &mine.last().unwrap().reactions;
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|x| x.count == 1 && !x.mine));
        assert!(
            bob.group_history(g.clone(), 10)
                .unwrap()
                .last()
                .unwrap()
                .reactions
                .iter()
                .all(|x| x.mine)
        );
        assert!(
            !bob.react_in_group(g.clone(), 999, "👍".into(), true)
                .unwrap()
        );
        let h = alice.group_history(g.clone(), 10).unwrap();
        assert_eq!(h.last().unwrap().file.as_ref().unwrap().name, "route.gpx");

        // Store and forward: Bob is away when Carol writes; Alice holds
        // her message until he is back.
        drop(all);
        bob.shutdown();
        drop(bob);
        let mut all = Pump::new(&[&*alice, &*carol]);
        all.until(
            0,
            |e| matches!(e, NodeEvent::Disconnected { peer, .. } if *peer == b_fp),
        );
        carol
            .send_group_text(g.clone(), "while you were out".into())
            .unwrap();
        // Let Alice take the forward (and hold it) before Bob is back.
        all.settle();
        drop(all);
        let bob = open("b");
        let mut all = Pump::new(&[&*alice, &*bob, &*carol]);
        bob.reconnect();
        let m = all.until(1, |e| matches!(e, NodeEvent::GroupMessage { .. }));
        assert!(
            matches!(m, NodeEvent::GroupMessage { from, text, .. } if from == c_fp && text == "while you were out")
        );

        // Only the owner removes members.
        assert!(bob.remove_from_group(g.clone(), c_fp.clone()).is_err());
        alice.remove_from_group(g.clone(), c_fp.clone()).unwrap();
        all.until(2, |e| matches!(e, NodeEvent::GroupLeft { .. }));
        all.until(1, |e| matches!(e, NodeEvent::GroupMembersChanged { removed, .. } if *removed == [c_fp.clone()]));
        assert!(carol.groups().is_empty());

        // Groups survive a restart.
        drop(all);
        bob.shutdown();
        drop(bob);
        assert_eq!(open("b").groups()[0].members.len(), 2);
    }

    /// Large frames over a slow, chunked, back-pressured link (like an
    /// L2CAP channel), in both directions at once.
    #[test]
    fn large_frames_cross_a_slow_link_both_ways() {
        use std::sync::mpsc::{SyncSender, sync_channel};
        struct Radio(SyncSender<Vec<u8>>);
        impl ByteLink for Radio {
            fn send(&self, data: Vec<u8>) -> bool {
                // Blocks when the "air" is full, as a socket write would.
                data.chunks(247).all(|c| self.0.send(c.to_vec()).is_ok())
            }
            fn disconnect(&self) {}
        }
        let dir = tempfile::tempdir().unwrap();
        let alice =
            ThrenodyNode::open(dir.path().join("a").display().to_string(), None, None).unwrap();
        let bob =
            ThrenodyNode::open(dir.path().join("b").display().to_string(), None, None).unwrap();
        let (a_tx, a_rx) = sync_channel::<Vec<u8>>(16);
        let (b_tx, b_rx) = sync_channel::<Vec<u8>>(16);
        let ha = alice
            .attach_link(Arc::new(Radio(a_tx)), true, "ble".into(), "b".into(), None)
            .unwrap();
        let hb = bob
            .attach_link(Arc::new(Radio(b_tx)), false, "ble".into(), "a".into(), None)
            .unwrap();
        std::thread::spawn(move || {
            for c in a_rx {
                hb.receive(c);
            }
        });
        std::thread::spawn(move || {
            for c in b_rx {
                ha.receive(c);
            }
        });
        wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));
        wait(&alice, |e| matches!(e, NodeEvent::Connected { .. }));
        // Bob answered Alice's call: he accepts her before she writes.
        bob.accept_contact(alice.device_fingerprint()).unwrap();
        let (a_id, b_id) = (alice.node.identity(), bob.node.identity());
        let file = |n: &str| AppMessage::File {
            sent_ms: 0,
            name: n.into(),
            data: vec![7u8; 400_000],
            id: 0,
            sensitive: false,
            caption: String::new(),
            album: 0,
        };
        alice.node.send(&b_id, file("to-bob")).unwrap();
        bob.node.send(&a_id, file("to-alice")).unwrap();
        alice
            .send_text(bob.device_fingerprint(), "after".into())
            .unwrap();
        let got = |n: &ThrenodyNode, name: &str| {
            let e = wait(n, |e| matches!(e, NodeEvent::File { .. }));
            assert!(
                matches!(e, NodeEvent::File { name: x, data, .. } if x == name && data.len() == 400_000)
            );
        };
        got(&bob, "to-bob");
        got(&alice, "to-alice");
        wait(&bob, |e| matches!(e, NodeEvent::Message { .. }));
    }
}
