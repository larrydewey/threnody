//! Scheduled messages ("send later") for apps: text to a contact or a
//! group, sent by the node when due (see `threnody_net::schedule`).
//!
//! Nothing is sent until the app calls `start_scheduled_sending`, right
//! after applying the user's settings. Group items reach the MLS engine
//! here: the node hands them over (`ScheduledChanged` with a group) and
//! the event pump, or `run_due_scheduled`, sends them.

use std::time::Duration;

use threnody_core::PublicIdentity;
use threnody_core::history::ConversationId;
use threnody_groups::GroupId;
use threnody_net::history::Extras;
use threnody_net::schedule::{Failure, Scheduled, Status, Target};

use crate::groups::hex;
use crate::{MentionInfo, QuoteInfo, Result, ThrenodyNode, entry_at, fail, fp};

/// Why a scheduled message wasn't sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ScheduleFailure {
    /// It would have gone out more than a day late.
    Missed,
    /// The contact was blocked or removed.
    Contact,
    /// We're no longer in the group.
    Group,
    /// Sending failed otherwise ("send now" retries).
    Error,
    /// This device was removed from our account.
    Removed,
}

impl From<Failure> for ScheduleFailure {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Missed => Self::Missed,
            Failure::Contact => Self::Contact,
            Failure::Group => Self::Group,
            Failure::Error => Self::Error,
            Failure::Removed => Self::Removed,
        }
    }
}

/// A scheduled message, as an app lists it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ScheduledInfo {
    pub id: u64,
    /// The contact's device it was scheduled to, for a contact.
    pub peer: Option<String>,
    /// Hex group id, for a group.
    pub group: Option<String>,
    pub text: String,
    /// When the user asked for it (ms since the epoch).
    pub due_ms: u64,
    pub created_ms: u64,
    /// What it replies to.
    pub quote: Option<QuoteInfo>,
    /// Set once it couldn't be sent (it stays until the user acts).
    pub failed: Option<ScheduleFailure>,
    /// Being sent right now: it can't be edited, moved or deleted.
    pub sending: bool,
}

fn info(s: &Scheduled) -> ScheduledInfo {
    ScheduledInfo {
        id: s.id,
        peer: s.target.device().as_ref().map(fp),
        group: s.target.group().as_ref().map(hex),
        text: s.text.clone(),
        due_ms: s.due_ms,
        created_ms: s.created_ms,
        quote: s.quote.as_ref().map(|q| QuoteInfo {
            id: q.id,
            author: PublicIdentity::from_bytes(&q.author)
                .map(|d| fp(&d))
                .unwrap_or_default(),
            text: q.text.clone(),
        }),
        failed: match s.status {
            Status::Failed(f) => Some(f.into()),
            Status::Waiting | Status::Sending => None,
        },
        sending: s.status == Status::Sending,
    }
}

/// A full hex group id, for groups we may no longer be in.
fn parse_group(s: &str) -> Option<GroupId> {
    if s.len() != 32 {
        return None;
    }
    let b: Vec<u8> = (0..32)
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    b.try_into().ok()
}

impl ThrenodyNode {
    /// A group by id (prefix), or by full id once we've left it.
    fn scheduled_group(&self, group: &str) -> Result<GroupId> {
        self.group_id(group)
            .or_else(|e| parse_group(&group.to_ascii_lowercase()).ok_or(e))
    }

    /// Sends the due group items the node handed over.
    pub(crate) fn send_due_group_scheduled(&self) {
        for s in self.node.take_due_group_scheduled() {
            let Target::Group(g) = s.target else {
                continue;
            };
            // Left meanwhile (the user dropped it): not sent.
            let r = self
                .node
                .scheduled_send_gate(s.id, || {
                    if self.node.already_sent(ConversationId::Group(g), s.id) {
                        return Ok(());
                    }
                    if !self.in_group(&g) {
                        return Err(Failure::Group);
                    }
                    let _guard = self.rt.enter();
                    let extras = Extras {
                        quote: s.quote.clone(),
                        mentions: s.mentions.clone(),
                        forwarded: false,
                    };
                    // A member: any failure is not "left the group".
                    self.group_node()
                        .send_text_with_id(&self.node, &g, &s.text, extras, s.id)
                        .map_err(|_| Failure::Error)
                })
                .unwrap_or(Ok(()));
            self.node.finish_scheduled(s.id, r);
        }
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Lets the node send scheduled messages when they're due. Call it
    /// right after applying the user's settings (disappearing timer,
    /// cover traffic): until then nothing scheduled goes out.
    pub fn start_scheduled_sending(&self) {
        let _guard = self.rt.enter();
        self.node.start_scheduled_sending();
    }

    /// Schedules `text` for the contact `peer` at `due_ms` (ms since the
    /// epoch), replying to the message `quote_at_ms` and `quote_device`
    /// name, if set. Returns its id.
    pub fn schedule_text(
        &self,
        peer: String,
        text: String,
        due_ms: u64,
        quote_at_ms: Option<u64>,
        quote_device: Option<String>,
    ) -> Result<u64> {
        let p = self.resolve(&peer)?;
        let quote = match (quote_at_ms, &quote_device) {
            (Some(at), Some(d)) => self
                .node
                .quote_of(self.node.conversation_for(&p), entry_at(at, d)),
            _ => None,
        };
        self.node
            .schedule_text(&p, &text, quote, due_ms)
            .map(|s| s.id)
            .map_err(fail)
    }

    /// Schedules `text` for `group` at `due_ms`, mentioning `mentions` and
    /// replying as in `schedule_text`. Returns its id.
    pub fn schedule_group_text(
        &self,
        group: String,
        text: String,
        due_ms: u64,
        quote_at_ms: Option<u64>,
        quote_device: Option<String>,
        mentions: Vec<MentionInfo>,
    ) -> Result<u64> {
        let g = self.group_id(&group)?;
        if !self.in_group(&g) {
            return Err(fail("not a member of this group"));
        }
        let quote = match (quote_at_ms, &quote_device) {
            (Some(at), Some(d)) => self
                .node
                .quote_of(ConversationId::Group(g), entry_at(at, d)),
            _ => None,
        };
        let mentions = self.group_mentions(&g, &mentions);
        self.node
            .schedule_group_text(g, &text, quote, mentions, due_ms)
            .map(|s| s.id)
            .map_err(fail)
    }

    /// The chat with `peer`'s scheduled messages, soonest first.
    pub fn scheduled(&self, peer: String) -> Result<Vec<ScheduledInfo>> {
        let p = self.resolve(&peer)?;
        let conv = self.node.conversation_for(&p);
        Ok(self.node.scheduled_in(conv).iter().map(info).collect())
    }

    /// `group`'s scheduled messages, soonest first.
    pub fn group_scheduled(&self, group: String) -> Result<Vec<ScheduledInfo>> {
        let g = self.scheduled_group(&group)?;
        Ok(self
            .node
            .scheduled_in(ConversationId::Group(g))
            .iter()
            .map(info)
            .collect())
    }

    /// Every scheduled message, soonest first.
    pub fn all_scheduled(&self) -> Vec<ScheduledInfo> {
        self.node.all_scheduled().iter().map(info).collect()
    }

    /// When the next scheduled message is sent (ms since the epoch), to
    /// set a wake-up alarm for.
    pub fn next_scheduled_ms(&self) -> Option<u64> {
        self.node.next_scheduled_ms()
    }

    /// How many scheduled messages are being sent right now: an app that
    /// would end its process (to sleep until woken) waits for them.
    pub fn scheduled_sending(&self) -> u32 {
        u32::try_from(self.node.scheduled_sending()).unwrap_or(u32::MAX)
    }

    /// Replaces a scheduled message's text; false if it's gone or being
    /// sent.
    pub fn edit_scheduled(&self, id: u64, text: String) -> Result<bool> {
        self.node.edit_scheduled(id, &text).map_err(fail)
    }

    /// Moves a scheduled message to `due_ms`; one that failed waits again.
    pub fn reschedule(&self, id: u64, due_ms: u64) -> Result<bool> {
        self.node.reschedule(id, due_ms).map_err(fail)
    }

    /// Deletes a scheduled message; false if it's gone or being sent.
    pub fn cancel_scheduled(&self, id: u64) -> bool {
        self.node.cancel_scheduled(id)
    }

    /// Sends a scheduled message (waiting, or failed) now.
    pub fn send_scheduled_now(&self, id: u64) -> bool {
        self.node.send_scheduled_now(id)
    }

    /// Sends what's due, for an app woken to do so (an alarm, a background
    /// task), and waits, at most `timeout_ms`, until it has gone and been
    /// acknowledged. Opens the gate as `start_scheduled_sending` does.
    /// Returns how many messages it handled.
    pub fn run_due_scheduled(&self, timeout_ms: u32) -> u32 {
        let budget = Duration::from_millis(u64::from(timeout_ms));
        let deadline = std::time::Instant::now() + budget;
        // What the scheduler is already sending isn't handed out again.
        // Spawned: when the budget runs out we stop waiting, but the sends
        // go on (dropping a JoinHandle doesn't abort its task).
        let node = self.node.clone();
        let pass = self
            .rt
            .spawn(async move { node.send_due_scheduled().await });
        let n = self
            .rt
            .block_on(async { tokio::time::timeout(budget, pass).await })
            .ok()
            .and_then(std::result::Result::ok)
            .unwrap_or(0);
        self.send_due_group_scheduled();
        self.start_scheduled_sending();
        // Let what was sent be acknowledged (or sealed) before the app
        // sleeps again.
        while std::time::Instant::now() < deadline {
            self.send_due_group_scheduled();
            let waiting = self.node.scheduled_sending() > 0
                || self
                    .node
                    .sessions()
                    .iter()
                    .any(|s| self.node.unacked(&s.peer) > 0);
            if !waiting {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}
