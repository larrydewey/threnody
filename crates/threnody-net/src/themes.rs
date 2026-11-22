//! Chat themes (Appendix W): the shared look of a chat with a contact,
//! set by either side and kept on every device of both. Groups carry
//! theirs inside MLS (see `threnody-groups`), through
//! [`Node::take_theme`] here.

use threnody_core::history::ConversationId;
use threnody_core::notice::{Notice, ThemeChange};
use threnody_core::theme::Theme;
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::node::{Event, Node};

impl Node {
    /// `conv`'s shared theme, if one was set (it may be plain).
    pub fn chat_theme(&self, conv: ConversationId) -> Option<Theme> {
        self.history(conv).ok()?.theme
    }

    /// Whether the user keeps their own plain look in `conv`.
    pub fn own_look(&self, conv: ConversationId) -> bool {
        self.history(conv).is_ok_and(|h| h.own_look)
    }

    /// Keeps (or stops keeping) the user's own plain look in `conv`, on
    /// this device; never sent to anyone.
    pub fn set_own_look(&self, conv: ConversationId, own: bool) -> Result<()> {
        let _lock = threnody_core::history::history_lock();
        let mut h = self.history(conv)?;
        h.own_look = own;
        self.shared
            .home
            .save_history(self.identity_ref(), conv, &h)
            .map_err(NetError::from)
    }

    /// Sets the theme of our chat with `peer` (a group picture in it is
    /// dropped: a contact has their own). It goes to their devices and
    /// ours, and the chat notes the change.
    pub fn set_chat_theme(&self, peer: &PublicIdentity, mut theme: Theme) -> Result<()> {
        theme.picture = None;
        theme.set_ms = now_ms();
        theme
            .check()
            .map_err(|e| NetError::NotAllowed(format!("theme: {e}")))?;
        let conv = self.conversation_for(peer);
        let bytes = theme.encode()?;
        self.take_theme(conv, theme, *self.identity().as_bytes(), true);
        let msg = AppMessage::Theme {
            conversation: conv.to_bytes(),
            theme: bytes,
        };
        for d in self.own_devices() {
            self.send_change(&d, &msg);
        }
        let devices: Vec<PublicIdentity> = match self.account_of(peer) {
            Some(a) => a.state().devices.iter().map(|(d, _)| *d).collect(),
            None => vec![*peer],
        };
        for d in devices {
            if d != self.identity() {
                self.send_change(&d, &msg);
            }
        }
        Ok(())
    }

    /// Takes `theme` for `conv` if it is newer than the one there, from
    /// `by` (ours when `outgoing`), and notes the change in the chat.
    /// Returns whether it took it.
    pub fn take_theme(
        &self,
        conv: ConversationId,
        theme: Theme,
        by: [u8; 32],
        outgoing: bool,
    ) -> bool {
        let before = {
            let _lock = threnody_core::history::history_lock();
            let Ok(mut h) = self.history(conv) else {
                return false;
            };
            let before = h.theme.clone();
            if !h.take_theme(Some(theme.clone())) {
                return false;
            }
            if self
                .shared
                .home
                .save_history(self.identity_ref(), conv, &h)
                .is_err()
            {
                return false;
            }
            before
        };
        let change = if theme.is_plain() {
            ThemeChange::Plain
        } else if before.as_ref().map(|b| (&b.background, b.accent))
            == Some((&theme.background, theme.accent))
        {
            ThemeChange::Picture
        } else {
            ThemeChange::Look
        };
        self.add_notice(conv, outgoing, by, Notice::Theme { change, by });
        true
    }

    /// A `Theme` from `from`: the contact setting our chat's theme, or one
    /// of our devices passing on a chat's.
    pub(crate) fn on_theme(&self, from: &PublicIdentity, conversation: &[u8], bytes: &[u8]) {
        let Ok(mut theme) = Theme::decode(bytes) else {
            return;
        };
        let own = self.is_own_device(from);
        let conv = if own {
            match ConversationId::from_bytes(conversation) {
                // Our sibling's chat with *us* is our chat with it.
                Some(ConversationId::Peer(k)) if k == *self.identity().as_bytes() => {
                    self.conversation_for(from)
                }
                // Groups go by MLS.
                Some(ConversationId::Group(_)) | None => return,
                Some(c) => c,
            }
        } else {
            if !self.is_accepted(from) {
                return;
            }
            self.conversation_for(from)
        };
        // A contact has no group picture to set.
        theme.picture = None;
        // Not ahead of our clock: a theme "from the future" would pin
        // itself against every later change.
        theme.set_ms = theme.set_ms.min(now_ms());
        if self.take_theme(conv, theme, *from.as_bytes(), own) {
            self.emit(Event::HistorySynced {
                from: *from,
                added: 0,
            });
        }
    }
}
