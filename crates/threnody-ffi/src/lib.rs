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

    /// Connects to an invite link or `host:port`; returns the peer's fingerprint.
    pub fn connect(&self, target: String) -> Result<String> {
        let (addr, pin) = match target.strip_prefix("threnody://") {
            Some(rest) => {
                let (f, a) = rest
                    .split_once('@')
                    .ok_or_else(|| fail("bad invite link"))?;
                (a.to_owned(), Some(f.parse::<Fingerprint>().map_err(fail)?))
            }
            None => (target, None),
        };
        self.rt
            .block_on(self.node.connect(&addr, pin))
            .map(|p| fp(&p))
            .map_err(fail)
    }

    /// Sends text to every device of `peer`'s account: live where
    /// connected, sealed for mailboxes otherwise; recorded in history.
    /// Returns how many devices it reached.
    pub fn send_text(&self, peer: String, text: String) -> Result<u32> {
        let p = self.resolve(&peer)?;
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

        // A protected identity needs its passphrase.
        drop(bob);
        assert!(ThrenodyNode::open(dir.path().join("b").display().to_string(), None).is_err());
    }
}
