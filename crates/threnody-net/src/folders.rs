//! Chat folders, pinned and archived chats, shared by our own devices
//! (see `threnody_core::folders`). The newest copy wins: each change
//! stamps the whole record and goes to our connected devices, and a
//! device that connects gets ours.

use threnody_core::folders::Folders;
use threnody_core::history::ConversationId;
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::node::{Event, Node, lock};

/// Where the record is kept (encrypted).
pub(crate) const STATE: &str = "folders";

impl Node {
    /// The folders, pinned and archived chats.
    pub fn folders(&self) -> Folders {
        lock(&self.shared.folders).clone()
    }

    /// Replaces them (stamped now), keeps them, and tells our other devices.
    pub fn set_folders(&self, f: Folders) {
        let mut f = f.trimmed();
        {
            let mut cur = lock(&self.shared.folders);
            // Never older than what we have, even with a clock behind.
            f.updated_ms = now_ms().max(cur.updated_ms + 1);
            *cur = f.clone();
        }
        self.shared.save_state(STATE, f.encode());
        if let Some(m) = folders_message(&f) {
            for p in self.sessions().into_iter().map(|s| s.peer) {
                if self.is_own_device(&p) {
                    let _ = self.send(&p, m.clone());
                }
            }
        }
    }

    /// Changes them with `f`; false leaves them as they were.
    pub fn update_folders(&self, f: impl FnOnce(&mut Folders) -> bool) {
        let mut cur = self.folders();
        if f(&mut cur) {
            self.set_folders(cur);
        }
    }

    /// Takes `conv` out of the archive (a new message there brings it back).
    pub(crate) fn unarchive(&self, conv: ConversationId) {
        if lock(&self.shared.folders).archived.contains(&conv) {
            self.update_folders(|f| {
                f.archived.retain(|c| *c != conv);
                true
            });
        }
    }

    /// What to send one of our devices as its session starts.
    pub(crate) fn folders_hello(&self) -> Option<AppMessage> {
        let f = self.folders();
        (f.updated_ms != 0).then(|| folders_message(&f)).flatten()
    }

    /// Our own device `from` sent its folders: keep them if newer.
    pub(crate) fn on_folders(&self, from: PublicIdentity, bytes: &[u8]) {
        if !self.is_own_device(&from) {
            return;
        }
        let Ok(theirs) = Folders::decode(bytes) else {
            return;
        };
        {
            let mut cur = lock(&self.shared.folders);
            if theirs.updated_ms <= cur.updated_ms {
                return;
            }
            *cur = theirs.clone();
        }
        self.shared.save_state(STATE, theirs.encode());
        self.emit(Event::FoldersChanged { from });
    }

    /// Notes that the user looked at `conv`: its messages are read.
    pub fn mark_seen(&self, conv: ConversationId) {
        let _ = self
            .shared
            .home
            .mark_seen(self.identity_ref(), conv, now_ms());
    }
}

fn folders_message(f: &Folders) -> Option<AppMessage> {
    crate::account::folders_message(f.encode().ok()?)
}
