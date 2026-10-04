//! UniFFI bindings for embedding a Threnody node in apps (Android via
//! Kotlin, iOS via Swift, and Python for tests and scripting).
//!
//! The API is deliberately small and blocking: each call runs on the
//! node's own Tokio runtime, so apps call it from a background thread and
//! drain events with [`ThrenodyNode::next_event`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use threnody_core::store::{Home, Lookup};
use threnody_core::{AppMessage, Fingerprint, PublicIdentity, safety_number};
use threnody_net::{AcceptPolicy, Event, Node, NodeConfig};
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;

uniffi::setup_scaffolding!();

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

/// A contact as an app shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ContactInfo {
    pub fingerprint: String,
    pub name: Option<String>,
    pub mutually_approved: bool,
    pub verified: bool,
    pub account: Option<String>,
    pub connected: bool,
}

/// One stored message.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HistoryEntry {
    pub at_ms: u64,
    pub outgoing: bool,
    pub device: String,
    pub text: String,
    pub disappearing: bool,
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
    File {
        peer: String,
        name: String,
        data: Vec<u8>,
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
            msg: AppMessage::File { name, data, .. },
        } => NodeEvent::File {
            peer: fp(&peer),
            name,
            data,
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
        other => NodeEvent::Other {
            description: format!("{other:?}"),
        },
    }
}

impl ThrenodyNode {
    fn resolve(&self, peer: &str) -> Result<PublicIdentity> {
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
    /// open a protected one.
    #[uniffi::constructor]
    pub fn open(home: String, passphrase: Option<String>) -> Result<Arc<Self>> {
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
        let (node, events) = {
            let _guard = rt.enter();
            Node::new(NodeConfig {
                home,
                identity,
                policy: AcceptPolicy::Anyone,
                constant_rate: None,
                tunnel_port: None,
            })
            .map_err(fail)?
        };
        Ok(Arc::new(Self {
            rt,
            node,
            events: Mutex::new(events),
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

    /// Starts listening; returns the bound address.
    pub fn listen(&self, addr: String) -> Result<String> {
        self.rt
            .block_on(self.node.listen(&addr))
            .map(|a| a.to_string())
            .map_err(fail)
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

    /// Sends text to every device of `peer`'s account: live where
    /// connected (reaching `peer` through relays if need be), sealed for
    /// mailboxes otherwise; recorded in history.
    /// Returns how many devices it reached.
    pub fn send_text(&self, peer: String, text: String) -> Result<u32> {
        let p = self.resolve(&peer)?;
        // No session: try to reach it (directly or through relays) before
        // falling back to sealed delivery.
        let _ = self.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), self.node.reach_peer(&p)).await
        });
        let _guard = self.rt.enter();
        let r = self.node.send_text(&p, &text).map_err(fail)?;
        Ok(u32::try_from(r.live + r.sealed).unwrap_or(u32::MAX))
    }

    /// The last `limit` messages with `peer` (oldest first).
    pub fn history(&self, peer: String, limit: u32) -> Result<Vec<HistoryEntry>> {
        let p = self.resolve(&peer)?;
        let h = self
            .node
            .history(self.node.conversation_for(&p))
            .map_err(fail)?;
        Ok(h.recent(limit as usize)
            .iter()
            .map(|e| HistoryEntry {
                at_ms: e.at_ms,
                outgoing: e.outgoing,
                device: PublicIdentity::from_bytes(&e.device)
                    .map(|d| fp(&d))
                    .unwrap_or_default(),
                text: e.text.clone(),
                disappearing: e.expires_at_ms.is_some(),
            })
            .collect())
    }

    /// Sets the disappearing-message timer with `peer` (`None` = off).
    pub fn set_disappearing(&self, peer: String, seconds: Option<u32>) -> Result<()> {
        let p = self.resolve(&peer)?;
        self.node.set_timer(&p, seconds).map_err(fail)
    }

    pub fn set_approval(&self, peer: String, approved: bool) -> Result<()> {
        let p = self.resolve(&peer)?;
        let _guard = self.rt.enter();
        self.node.set_approval(&p, approved).map_err(fail)
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
                name: c.petname.clone(),
                mutually_approved: c.mutually_approved(),
                verified: c.verified,
                account: c.account.map(|a| a.fingerprint().to_string()),
                connected: live.contains(&c.key),
            })
            .collect()
    }

    /// The 60-digit safety number to compare with `peer` out of band.
    pub fn safety_number(&self, peer: String) -> Result<String> {
        let p = self.resolve(&peer)?;
        Ok(safety_number(&self.node.identity(), &p))
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

    /// Waits up to `timeout_ms` for the next event.
    pub fn next_event(&self, timeout_ms: u32) -> Option<NodeEvent> {
        let mut rx = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Build the timer inside the runtime (it needs the reactor).
        self.rt
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(u64::from(timeout_ms)), rx.recv()).await
            })
            .ok()
            .flatten()
            .map(convert)
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

    #[test]
    fn two_embedded_nodes_chat() {
        let dir = tempfile::tempdir().unwrap();
        let alice = ThrenodyNode::open(dir.path().join("a").display().to_string(), None).unwrap();
        let bob = ThrenodyNode::open(
            dir.path().join("b").display().to_string(),
            Some("pw".into()),
        )
        .unwrap();
        let addr = bob.listen("127.0.0.1:0".into()).unwrap();
        let bob_fp = alice.connect(bob.invite_link(addr)).unwrap();
        assert_eq!(bob_fp, bob.device_fingerprint());
        wait(&bob, |e| matches!(e, NodeEvent::Connected { .. }));

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
        assert_eq!(h.len(), 1);
        assert!(h[0].outgoing && h[0].text == "hello from an app");
        assert_eq!(
            bob.history(alice.device_fingerprint(), 10).unwrap()[0].text,
            "hello from an app"
        );
        assert_eq!(
            alice.safety_number(bob_fp).unwrap(),
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
        assert!(ThrenodyNode::open(dir.path().join("b").display().to_string(), None).is_err());
    }
}
