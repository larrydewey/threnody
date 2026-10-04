//! Sending text to accounts, and recording history (spec §6.1, §6.4).

use std::time::Duration;

use threnody_core::history::{ConversationId, Entry, FileNote, History};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::delivery::Tag;
use crate::error::{NetError, Result};
use crate::node::{Event, Node};

/// A random non-zero id for an outgoing history entry.
pub fn local_id() -> u64 {
    u64::from_le_bytes(threnody_core::crypto::random_bytes()).max(1)
}

/// How often expired messages are swept from disk.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// What happened to one outgoing message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SendReport {
    pub live: usize,
    pub sealed: usize,
    pub unreachable: usize,
}

impl Node {
    /// The conversation a peer device belongs to: its account if known,
    /// otherwise the device itself.
    pub fn conversation_for(&self, peer: &PublicIdentity) -> ConversationId {
        match self.account_of(peer) {
            Some(a) if !self.is_own_device(peer) => ConversationId::Peer(a.id().0),
            _ => ConversationId::Peer(*peer.as_bytes()),
        }
    }

    /// Sends `body` to every device of `peer`'s account (live where
    /// connected, sealed for mailboxes otherwise), applying and recording
    /// the conversation's disappearing-message timer.
    pub fn send_text(&self, peer: &PublicIdentity, body: &str) -> Result<SendReport> {
        let conv = self.conversation_for(peer);
        let timer = self.history(conv).map(|h| h.timer_s).unwrap_or(None);
        let msg = AppMessage::Text {
            sent_ms: now_ms(),
            body: body.to_owned(),
            expires_in_s: timer,
        };
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) if !self.is_own_device(peer) => {
                a.state().devices.iter().map(|(d, _)| *d).collect()
            }
            _ => vec![*peer],
        };
        let local_id = local_id();
        let mut r = SendReport::default();
        for d in &devices {
            let tag = Tag {
                local_id,
                ..Tag::NONE
            };
            if self.send_tagged(d, msg.clone(), tag).is_ok() {
                r.live += 1;
            } else if self.can_send_offline(d) && self.send_offline(d, &msg).is_ok() {
                r.sealed += 1;
            } else {
                r.unreachable += 1;
            }
        }
        if r.live + r.sealed == 0 {
            return Err(NetError::NoRoute(format!(
                "{} (no session and no prekeys; try a relay)",
                peer.fingerprint()
            )));
        }
        let now = now_ms();
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing: true,
                device: *self.identity().as_bytes(),
                text: body.to_owned(),
                offline: false,
                expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
                file: None,
                local_id,
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
            },
        );
        Ok(r)
    }

    /// Sends a file to `peer` (who must be reachable live: files aren't
    /// sealed for mailboxes) and records it in history with `location`,
    /// where it lives on this device.
    pub fn send_file(
        &self,
        peer: &PublicIdentity,
        name: &str,
        data: Vec<u8>,
        location: Option<String>,
    ) -> Result<()> {
        if data.len() > threnody_core::message::MAX_FILE {
            return Err(NetError::Protocol(threnody_core::Error::Malformed(
                "file too large",
            )));
        }
        let local_id = local_id();
        let size = data.len() as u64;
        let msg = AppMessage::File {
            sent_ms: now_ms(),
            name: name.to_owned(),
            data,
        };
        self.send_tagged(
            peer,
            msg,
            Tag {
                local_id,
                ..Tag::NONE
            },
        )?;
        self.append_file(
            peer,
            true,
            FileNote {
                name: name.to_owned(),
                size,
                location,
            },
            local_id,
        );
        Ok(())
    }

    /// Records `peer`'s acknowledgement of the entry `tag` names, and
    /// tells the app if that changed anything. For a message forwarded on
    /// another member's behalf, only tells the app (to send a receipt).
    pub fn mark_delivered(&self, peer: &PublicIdentity, tag: Tag) {
        if let Some(origin) = tag.relay_for {
            if let Ok(origin) = PublicIdentity::from_bytes(&origin) {
                self.emit(Event::Delivered {
                    peer: *peer,
                    local_id: tag.local_id,
                    group: tag.group,
                    relay_for: Some(origin),
                });
            }
            return;
        }
        let conv = match tag.group {
            Some(g) => ConversationId::Group(g),
            None => self.conversation_for(peer),
        };
        if let Ok(true) = self.shared.home.mark_delivered(
            self.identity_ref(),
            conv,
            tag.local_id,
            *peer.as_bytes(),
            now_ms(),
        ) {
            self.emit(Event::Delivered {
                peer: *peer,
                local_id: tag.local_id,
                group: tag.group,
                relay_for: None,
            });
        }
    }

    /// Appends an entry to a conversation's history.
    pub fn append(&self, conv: ConversationId, entry: Entry) {
        let now = entry.at_ms;
        let _ = self
            .shared
            .home
            .append_history(self.identity_ref(), conv, entry, now);
    }

    /// Sets the disappearing-message timer for the conversation with
    /// `peer` (`None` turns it off). Applies to messages sent from now on.
    pub fn set_timer(&self, peer: &PublicIdentity, secs: Option<u32>) -> Result<()> {
        let conv = self.conversation_for(peer);
        let mut h = self.history(conv)?;
        h.timer_s = secs;
        self.shared
            .home
            .save_history(self.identity_ref(), conv, &h)
            .map_err(NetError::from)
    }

    pub fn history(&self, conv: ConversationId) -> Result<History> {
        self.shared
            .home
            .load_history(self.identity_ref(), conv, now_ms())
            .map_err(NetError::from)
    }

    /// Appends one entry; `timer` is the message's own expiry in seconds.
    pub fn record(
        &self,
        conv: ConversationId,
        device: [u8; 32],
        outgoing: bool,
        text: &str,
        offline: bool,
        timer: Option<u32>,
    ) {
        let now = now_ms();
        let entry = Entry {
            at_ms: now,
            outgoing,
            device,
            text: text.to_owned(),
            offline,
            expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
            file: None,
            local_id: 0,
            delivered: false,
            recipients: 0,
            delivered_to: Vec::new(),
        };
        let _ = self
            .shared
            .home
            .append_history(self.identity_ref(), conv, entry, now);
    }

    /// Records a file received from (or sent to) `peer`'s conversation,
    /// with where the app saved it. It follows the conversation's
    /// disappearing-message timer. Sending through [`Node::send_file`]
    /// records it already.
    pub fn record_file(&self, peer: &PublicIdentity, outgoing: bool, file: FileNote) {
        self.append_file(peer, outgoing, file, 0);
    }

    fn append_file(&self, peer: &PublicIdentity, outgoing: bool, file: FileNote, local_id: u64) {
        let conv = self.conversation_for(peer);
        let timer = self.history(conv).map(|h| h.timer_s).unwrap_or(None);
        let now = now_ms();
        let device = if outgoing {
            *self.identity().as_bytes()
        } else {
            *peer.as_bytes()
        };
        self.append(
            conv,
            Entry {
                at_ms: now,
                outgoing,
                device,
                text: String::new(),
                offline: false,
                expires_at_ms: timer.map(|s| now + u64::from(s) * 1000),
                file: Some(file),
                local_id,
                delivered: false,
                recipients: 0,
                delivered_to: Vec::new(),
            },
        );
    }

    /// Records an incoming text, adopting the sender's timer setting.
    pub(crate) fn record_incoming(&self, from: &PublicIdentity, msg: &AppMessage, offline: bool) {
        let AppMessage::Text {
            body, expires_in_s, ..
        } = msg
        else {
            return;
        };
        let conv = self.conversation_for(from);
        if let Ok(mut h) = self.history(conv)
            && h.timer_s != *expires_in_s
        {
            h.timer_s = *expires_in_s;
            let _ = self.shared.home.save_history(self.identity_ref(), conv, &h);
            self.emit(Event::TimerChanged {
                peer: *from,
                secs: *expires_in_s,
            });
        }
        self.record(conv, *from.as_bytes(), false, body, offline, *expires_in_s);
    }

    /// Starts the background sweep of expired history.
    pub(crate) fn start_history_sweep(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        node.shared.home.sweep_history(node.identity_ref(), now_ms());
                    }
                    () = &mut closed => break,
                }
            }
        });
    }
}
