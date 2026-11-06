//! Scheduled messages ("send later"): text to a contact or a group that
//! this node sends at a time the user picked.
//!
//! - **Device-local.** The schedule lives on the device it was made on,
//!   encrypted in the state file [`STATE`] (deleted while nothing is
//!   pending). It is never synced to our other devices and never backed up;
//!   they see a scheduled message only once it has been sent, like any
//!   other message we send.
//! - **Sent once.** An item's id is also the sent message's local id (and
//!   with it the id it carries on the wire). Before sending, the node looks
//!   for an outgoing history entry with that id ([`Node::already_sent`]),
//!   so a crash between sending and saving can't send it twice. Items being
//!   sent are never written as such: after a restart they wait again and
//!   are checked as above.
//! - **Gated.** Nothing is sent until the app says it has applied the
//!   user's settings ([`Node::start_scheduled_sending`]), so an overdue item
//!   can't go out at startup without the user's disappearing timer.
//! - **Never more than a day late** without the user saying so: such an
//!   item is marked missed instead ([`MISSED_AFTER_MS`]).
//! - **Jitter.** Each item fires a little after its time
//!   ([`Schedule::jitter`]), never before, so sends don't land on exact
//!   minute marks.
//!
//! On the wire a scheduled message is an ordinary one. See Appendix C,
//! "Scheduled messages".

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use const_cbor::Decoder;
use threnody_core::cbor::{self, fixed_bytes, read_map};
use threnody_core::history::ConversationId;
use threnody_core::message::{Mention, Quote};
use threnody_core::{PublicIdentity, now_ms};

use crate::error::{NetError, Result};
use crate::history::{Extras, local_id};
use crate::node::{Event, Node, lock};

/// Where the schedule is kept (encrypted).
pub(crate) const STATE: &str = "scheduled";
/// Most items pending at once, per identity.
pub const MAX_SCHEDULED: usize = 256;
/// Most bytes of text one item may hold.
pub const MAX_SCHEDULED_TEXT: usize = crate::delivery::MAX_PERSISTED;
/// Most bytes of text all items together may hold: the whole file is
/// encrypted and written again on every change.
pub const MAX_SCHEDULED_BYTES: usize = 1024 * 1024;
/// How far ahead an item may be scheduled.
pub const MAX_AHEAD_MS: u64 = 366 * 24 * 3600 * 1000;
/// An item this late isn't sent: the user decides.
pub const MISSED_AFTER_MS: u64 = 24 * 3600 * 1000;
/// The most an item fires after its time.
pub const MAX_JITTER_MS: u64 = 30_000;
/// The longest the scheduler sleeps between looks at the wall clock (a
/// machine that was suspended catches up within this).
pub(crate) const CHECK_EVERY: Duration = Duration::from_secs(15);
/// How long to try for a session with the recipient before sending (it
/// is sealed for mailboxes, or held, if there's none by then).
pub(crate) const REACH_FIRST: Duration = Duration::from_secs(15);

const VERSION: u8 = 1;

/// Whom an item goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// The device key used when scheduling, and the conversation (account
    /// or device id, as in `ConversationId::Peer`) it belonged to then.
    Peer {
        device: [u8; 32],
        conv: [u8; 32],
    },
    Group([u8; 16]),
}

impl Target {
    fn to_bytes(self) -> Vec<u8> {
        match self {
            Self::Peer { device, conv } => [&[0u8][..], &device, &conv].concat(),
            Self::Group(g) => [&[1u8][..], &g].concat(),
        }
    }

    fn from_bytes(b: &[u8]) -> Option<Self> {
        match b.split_first()? {
            (0, rest) if rest.len() == 64 => Some(Self::Peer {
                device: rest[..32].try_into().ok()?,
                conv: rest[32..].try_into().ok()?,
            }),
            (1, g) => g.try_into().ok().map(Self::Group),
            _ => None,
        }
    }

    /// The device it was scheduled to, for a contact.
    pub fn device(&self) -> Option<PublicIdentity> {
        match self {
            Self::Peer { device, .. } => PublicIdentity::from_bytes(device).ok(),
            Self::Group(_) => None,
        }
    }

    /// The group, for a group.
    pub fn group(&self) -> Option<[u8; 16]> {
        match self {
            Self::Group(g) => Some(*g),
            Self::Peer { .. } => None,
        }
    }
}

/// Why an item wasn't sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// It would have gone out more than [`MISSED_AFTER_MS`] late.
    Missed,
    /// The contact was blocked or removed (here or on another device).
    Contact,
    /// We're no longer in the group.
    Group,
    /// Sending failed otherwise; sending again may work.
    Error,
    /// This device was removed from our account: contacts no longer take
    /// it as us.
    Removed,
}

impl Failure {
    fn code(self) -> u8 {
        match self {
            Self::Missed => 1,
            Self::Contact => 2,
            Self::Group => 3,
            Self::Error => 4,
            Self::Removed => 5,
        }
    }

    fn from_code(c: u64) -> Self {
        match c {
            1 => Self::Missed,
            2 => Self::Contact,
            3 => Self::Group,
            5 => Self::Removed,
            _ => Self::Error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Waiting,
    /// Being sent now (never written to disk: it waits again after a
    /// restart, and is checked against history before it goes).
    Sending,
    Failed(Failure),
}

/// One scheduled message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scheduled {
    /// Also the sent message's local id (see [`local_id`]).
    pub id: u64,
    pub target: Target,
    /// When the user asked for it (ms since the epoch); what's shown.
    pub due_ms: u64,
    /// When it goes: `due_ms` plus jitter.
    pub fire_ms: u64,
    pub created_ms: u64,
    pub text: String,
    pub quote: Option<Quote>,
    /// Who it mentions (groups only).
    pub mentions: Vec<Mention>,
    pub status: Status,
}

/// The schedule of one identity.
#[derive(Debug, Default)]
pub(crate) struct Schedule {
    items: Vec<Scheduled>,
    /// Due group items for the app's MLS engine (see
    /// [`Node::take_due_group_scheduled`]).
    pub to_groups: Vec<Scheduled>,
}

/// A node's schedule and its timer.
#[derive(Default)]
pub(crate) struct ScheduleState {
    pub items: Mutex<Schedule>,
    /// Wakes the scheduler when the schedule changes.
    pub kick: tokio::sync::Notify,
    /// Set by [`Node::start_scheduled_sending`] once the app has applied
    /// its settings.
    pub started: AtomicBool,
}

impl ScheduleState {
    pub fn new(items: Schedule) -> Self {
        Self {
            items: Mutex::new(items),
            ..Self::default()
        }
    }
}

/// Checks text to schedule.
fn check_text(text: &str) -> std::result::Result<(), &'static str> {
    if text.trim().is_empty() {
        return Err("Nothing to send");
    }
    if text.len() > MAX_SCHEDULED_TEXT {
        return Err("Too long to schedule");
    }
    Ok(())
}

/// Checks a time to schedule for.
fn check_time(due_ms: u64, now: u64) -> std::result::Result<(), &'static str> {
    if due_ms <= now {
        return Err("Pick a time in the future");
    }
    if due_ms > now.saturating_add(MAX_AHEAD_MS) {
        return Err("Pick a time within a year");
    }
    Ok(())
}

/// The mentions of `m` (made for `old`) whose characters read the same in
/// `new`, so an edit keeps only those it left alone.
fn keep_mentions(old: &str, new: &str, m: Vec<Mention>) -> Vec<Mention> {
    let (old, new): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let span = |s: &[char], m: &Mention| {
        let start = m.start as usize;
        s.get(start..start.checked_add(m.len as usize)?)
            .map(<[char]>::to_vec)
    };
    let kept = m
        .into_iter()
        .filter(|m| span(&old, m).is_some_and(|o| span(&new, m) == Some(o)))
        .collect();
    Mention::valid(kept, &new.iter().collect::<String>())
}

impl Schedule {
    /// A random delay for an item `lead_ms` ahead: at most
    /// [`MAX_JITTER_MS`] and a tenth of the lead.
    pub(crate) fn jitter(lead_ms: u64) -> u64 {
        let max = MAX_JITTER_MS.min(lead_ms / 10);
        u64::from_le_bytes(threnody_core::crypto::random_bytes()) % (max + 1)
    }

    fn text_bytes(&self) -> usize {
        self.items.iter().map(|s| s.text.len()).sum()
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Scheduled> {
        self.items.iter_mut().find(|s| s.id == id)
    }

    pub fn items(&self) -> &[Scheduled] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Adds `s`, due at `s.due_ms`, with fresh jitter; checks its text,
    /// its time and the caps.
    pub fn add(&mut self, mut s: Scheduled, now: u64) -> std::result::Result<(), &'static str> {
        check_text(&s.text)?;
        check_time(s.due_ms, now)?;
        if self.items.len() >= MAX_SCHEDULED {
            return Err("Too many scheduled messages");
        }
        if self.text_bytes() + s.text.len() > MAX_SCHEDULED_BYTES {
            return Err("Too much text scheduled already");
        }
        s.fire_ms = s.due_ms + Self::jitter(s.due_ms - now);
        s.status = Status::Waiting;
        self.items.push(s);
        Ok(())
    }

    /// The waiting items whose time has come: (to send, missed). Those to
    /// send are marked sending (so they are handed out once); those more
    /// than [`MISSED_AFTER_MS`] late fail. Each list in firing order.
    pub fn due(&mut self, now: u64) -> (Vec<Scheduled>, Vec<Scheduled>) {
        let (mut send, mut missed) = (Vec::new(), Vec::new());
        for s in &mut self.items {
            if s.status != Status::Waiting || s.fire_ms > now {
                continue;
            }
            if now > s.due_ms.saturating_add(MISSED_AFTER_MS) {
                s.status = Status::Failed(Failure::Missed);
                missed.push(s.clone());
            } else {
                s.status = Status::Sending;
                send.push(s.clone());
            }
        }
        send.sort_by_key(|s| s.fire_ms);
        missed.sort_by_key(|s| s.fire_ms);
        (send, missed)
    }

    /// When the next waiting item fires.
    pub fn next_fire(&self) -> Option<u64> {
        self.items
            .iter()
            .filter(|s| s.status == Status::Waiting)
            .map(|s| s.fire_ms)
            .min()
    }

    /// How many items are being sent.
    pub fn sending(&self) -> usize {
        self.items
            .iter()
            .filter(|s| s.status == Status::Sending)
            .count()
    }

    /// Ends item `id`'s send: gone when sent, else failed with the reason.
    /// Returns the item.
    pub fn finish(&mut self, id: u64, r: std::result::Result<(), Failure>) -> Option<Scheduled> {
        match r {
            Ok(()) => {
                let i = self.items.iter().position(|s| s.id == id)?;
                Some(self.items.remove(i))
            }
            Err(f) => {
                let s = self.get_mut(id)?;
                s.status = Status::Failed(f);
                Some(s.clone())
            }
        }
    }

    /// Replaces item `id`'s text (not while it's being sent), keeping its
    /// quote and the mentions the edit left alone.
    pub fn edit(&mut self, id: u64, text: &str) -> std::result::Result<bool, &'static str> {
        check_text(text)?;
        let others: usize = self
            .items
            .iter()
            .filter(|s| s.id != id)
            .map(|s| s.text.len())
            .sum();
        let Some(s) = self.get_mut(id).filter(|s| s.status != Status::Sending) else {
            return Ok(false);
        };
        if others + text.len() > MAX_SCHEDULED_BYTES {
            return Err("Too much text scheduled already");
        }
        s.mentions = keep_mentions(&s.text, text, std::mem::take(&mut s.mentions));
        text.clone_into(&mut s.text);
        Ok(true)
    }

    /// Moves item `id` (not while it's being sent) to `due_ms`, with new
    /// jitter; a failed item waits again.
    pub fn reschedule(
        &mut self,
        id: u64,
        due_ms: u64,
        now: u64,
    ) -> std::result::Result<bool, &'static str> {
        check_time(due_ms, now)?;
        let Some(s) = self.get_mut(id).filter(|s| s.status != Status::Sending) else {
            return Ok(false);
        };
        s.due_ms = due_ms;
        s.fire_ms = due_ms + Self::jitter(due_ms - now);
        s.status = Status::Waiting;
        Ok(true)
    }

    /// Makes item `id` (waiting or failed) due now, without jitter.
    pub fn send_now(&mut self, id: u64, now: u64) -> bool {
        let Some(s) = self.get_mut(id).filter(|s| s.status != Status::Sending) else {
            return false;
        };
        s.due_ms = now;
        s.fire_ms = now;
        s.status = Status::Waiting;
        true
    }

    /// Deletes item `id`, unless it's being sent.
    pub fn cancel(&mut self, id: u64) -> Option<Scheduled> {
        let i = self
            .items
            .iter()
            .position(|s| s.id == id && s.status != Status::Sending)?;
        Some(self.items.remove(i))
    }

    /// Keeps the items `keep` chooses (and those being sent, which finish
    /// first); returns the others.
    pub fn retain(&mut self, keep: impl Fn(&Scheduled) -> bool) -> Vec<Scheduled> {
        let (kept, gone) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|s| s.status == Status::Sending || keep(s));
        self.items = kept;
        gone
    }

    /// Fails every waiting item with `f`; returns them.
    pub fn fail_waiting(&mut self, f: Failure) -> Vec<Scheduled> {
        let mut out = Vec::new();
        for s in &mut self.items {
            if s.status == Status::Waiting {
                s.status = Status::Failed(f);
                out.push(s.clone());
            }
        }
        out
    }

    /// Stored as `{ 0: 1, 1: [* item] }`, each item `{ 0: id, 1: target,
    /// 2: due, 3: fire, 4: created, 5: text, ? 6: quote, ? 7: mentions,
    /// ? 8: failure }` (Appendix C). Items being sent are written as
    /// waiting.
    pub fn encode(&self) -> threnody_core::Result<Vec<u8>> {
        let hint = 64 + self.text_bytes() + self.items.len() * 256;
        cbor::to_vec(hint, |e| {
            e.map_len(2)?.u8(0)?.u8(VERSION)?;
            e.u8(1)?.array_len(self.items.len())?;
            for s in &self.items {
                let failed = match s.status {
                    Status::Failed(f) => Some(f),
                    _ => None,
                };
                let n = 6
                    + usize::from(s.quote.is_some())
                    + usize::from(!s.mentions.is_empty())
                    + usize::from(failed.is_some());
                e.map_len(n)?;
                e.u8(0)?.u64(s.id)?;
                e.u8(1)?.bytes(&s.target.to_bytes())?;
                e.u8(2)?.u64(s.due_ms)?;
                e.u8(3)?.u64(s.fire_ms)?;
                e.u8(4)?.u64(s.created_ms)?;
                e.u8(5)?.str(&s.text)?;
                if let Some(q) = &s.quote {
                    e.u8(6)?
                        .array_len(3)?
                        .u64(q.id)?
                        .bytes(&q.author)?
                        .str(&q.text)?;
                }
                if !s.mentions.is_empty() {
                    e.u8(7)?.array_len(s.mentions.len())?;
                    for m in &s.mentions {
                        e.array_len(3)?.u32(m.start)?.u32(m.len)?.bytes(&m.who)?;
                    }
                }
                if let Some(f) = failed {
                    e.u8(8)?.u8(f.code())?;
                }
            }
            Ok(())
        })
    }

    /// Reads what [`Schedule::encode`] stored. Lenient: items it can't
    /// read are skipped, unknown keys ignored, and damage stops the read
    /// with what was read so far; at most [`MAX_SCHEDULED`] items.
    pub fn decode(b: &[u8]) -> Self {
        let mut out = Self::default();
        if b.is_empty() {
            return out;
        }
        let mut dec = Decoder::new(b);
        let _ = read_map(&mut dec, |k, d| {
            match k {
                0 => {
                    d.u64()?;
                }
                1 => {
                    for _ in 0..d.array_len()? {
                        if let Some(s) = decode_item(d)?
                            && out.items.len() < MAX_SCHEDULED
                            && out.items.iter().all(|o| o.id != s.id)
                        {
                            out.items.push(s);
                        }
                    }
                }
                _ => return Ok(false),
            }
            Ok(true)
        });
        out
    }
}

/// One stored item: `None` if it reads but makes no sense (skipped), an
/// error if the bytes are damaged.
fn decode_item(d: &mut Decoder<'_>) -> threnody_core::Result<Option<Scheduled>> {
    let (mut id, mut target, mut due, mut fire, mut created, mut text) =
        (None, None, None, None, None, None);
    let mut quote = None;
    let mut mentions = Vec::new();
    let mut status = Status::Waiting;
    let mut bad = false;
    read_map(d, |k, d| {
        match k {
            0 => id = Some(d.u64()?),
            1 => {
                target = Target::from_bytes(d.bytes()?);
                bad |= target.is_none();
            }
            2 => due = Some(d.u64()?),
            3 => fire = Some(d.u64()?),
            4 => created = Some(d.u64()?),
            5 => text = Some(d.str()?.to_owned()),
            6 => {
                if d.array_len()? != 3 {
                    return Err(threnody_core::Error::Malformed("scheduled quote"));
                }
                let id = d.u64()?;
                let author = fixed_bytes::<32>(d)?;
                quote = Some(Quote::new(id, author, d.str()?));
            }
            7 => {
                for _ in 0..d.array_len()? {
                    if d.array_len()? != 3 {
                        return Err(threnody_core::Error::Malformed("scheduled mention"));
                    }
                    let (start, len) = (d.u32()?, d.u32()?);
                    let who = fixed_bytes::<32>(d)?;
                    mentions.push(Mention { start, len, who });
                }
            }
            8 => status = Status::Failed(Failure::from_code(d.u64()?)),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    let (Some(id), Some(target), Some(due_ms), Some(fire_ms), Some(created_ms), Some(text)) =
        (id, target, due, fire, created, text)
    else {
        return Ok(None);
    };
    if bad || id == 0 || text.len() > MAX_SCHEDULED_TEXT {
        return Ok(None);
    }
    let mentions = Mention::valid(mentions, &text);
    Ok(Some(Scheduled {
        id,
        target,
        due_ms,
        fire_ms,
        created_ms,
        text,
        quote,
        mentions,
        status,
    }))
}

impl Node {
    /// Saves the schedule (or deletes its file once it's empty).
    fn persist_schedule(&self, s: &Schedule) {
        if s.is_empty() {
            if let Err(e) = self.shared.home.remove_state(STATE) {
                eprintln!("threnody: failed to remove {STATE}: {e}");
            }
        } else {
            self.shared.save_state(STATE, s.encode());
        }
    }

    /// Runs `f` on the schedule, saves it, and wakes the scheduler.
    fn schedule_mut<R>(&self, f: impl FnOnce(&mut Schedule) -> R) -> R {
        let r = {
            let mut s = lock(&self.shared.schedule.items);
            let r = f(&mut s);
            self.persist_schedule(&s);
            r
        };
        self.shared.schedule.kick.notify_one();
        r
    }

    fn scheduled_changed(&self, target: Target) {
        self.emit(Event::ScheduledChanged {
            peer: target.device(),
            group: target.group(),
        });
    }

    fn scheduled_failed(&self, s: &Scheduled, reason: Failure) {
        self.emit(Event::ScheduledFailed {
            peer: s.target.device(),
            group: s.target.group(),
            id: s.id,
            reason,
        });
    }

    fn target_of(&self, id: u64) -> Option<Target> {
        lock(&self.shared.schedule.items)
            .items()
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.target)
    }

    fn add_scheduled(&self, s: Scheduled) -> Result<Scheduled> {
        let now = now_ms();
        let id = s.id;
        self.schedule_mut(|sch| sch.add(s, now))
            .map_err(|e| NetError::NotAllowed(e.into()))?;
        let s = lock(&self.shared.schedule.items)
            .items()
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or(NetError::Closed)?;
        self.scheduled_changed(s.target);
        Ok(s)
    }

    /// Schedules `body` (replying to `quote`) for `peer`'s account at
    /// `due_ms` (ms since the epoch). `peer` must be a contact we haven't
    /// blocked.
    pub fn schedule_text(
        &self,
        peer: &PublicIdentity,
        body: &str,
        quote: Option<Quote>,
        due_ms: u64,
    ) -> Result<Scheduled> {
        let allowed = lock(&self.shared.contacts)
            .get(peer)
            .is_some_and(|c| !c.blocked);
        if !allowed || *peer == self.identity() {
            return Err(NetError::NotAllowed(
                "only contacts can get scheduled messages".into(),
            ));
        }
        let ConversationId::Peer(conv) = self.conversation_for(peer) else {
            return Err(NetError::NotAllowed("not a contact".into()));
        };
        self.add_scheduled(Scheduled {
            id: local_id(),
            target: Target::Peer {
                device: *peer.as_bytes(),
                conv,
            },
            due_ms,
            fire_ms: due_ms,
            created_ms: now_ms(),
            text: body.to_owned(),
            quote,
            mentions: Vec::new(),
            status: Status::Waiting,
        })
    }

    /// Schedules `body` for `group` at `due_ms`, replying to `quote` and
    /// mentioning `mentions`. The app's MLS engine sends it when it's due
    /// (see [`Node::take_due_group_scheduled`]).
    pub fn schedule_group_text(
        &self,
        group: [u8; 16],
        body: &str,
        quote: Option<Quote>,
        mentions: Vec<Mention>,
        due_ms: u64,
    ) -> Result<Scheduled> {
        self.add_scheduled(Scheduled {
            id: local_id(),
            target: Target::Group(group),
            due_ms,
            fire_ms: due_ms,
            created_ms: now_ms(),
            text: body.to_owned(),
            quote,
            mentions: Mention::valid(mentions, body),
            status: Status::Waiting,
        })
    }

    /// Every item, soonest first.
    pub fn all_scheduled(&self) -> Vec<Scheduled> {
        let mut v = lock(&self.shared.schedule.items).items().to_vec();
        v.sort_by_key(|s| (s.due_ms, s.created_ms));
        v
    }

    /// The conversation an item belongs to now (its stored one, unless
    /// its device has since been learned to be in another).
    fn conversations_of(&self, s: &Scheduled) -> Vec<ConversationId> {
        match s.target {
            Target::Group(g) => vec![ConversationId::Group(g)],
            Target::Peer { device, conv } => {
                let mut v = vec![ConversationId::Peer(conv)];
                if let Ok(d) = PublicIdentity::from_bytes(&device) {
                    let now = self.conversation_for(&d);
                    if !v.contains(&now) {
                        v.push(now);
                    }
                }
                v
            }
        }
    }

    /// The items of conversation `conv`, soonest first.
    pub fn scheduled_in(&self, conv: ConversationId) -> Vec<Scheduled> {
        // Copied out first: `conversation_for` locks contacts and accounts.
        self.all_scheduled()
            .into_iter()
            .filter(|s| self.conversations_of(s).contains(&conv))
            .collect()
    }

    /// When the next item fires (ms since the epoch).
    pub fn next_scheduled_ms(&self) -> Option<u64> {
        lock(&self.shared.schedule.items).next_fire()
    }

    /// Replaces item `id`'s text; false if there's no such item, or it's
    /// being sent.
    pub fn edit_scheduled(&self, id: u64, body: &str) -> Result<bool> {
        let done = self
            .schedule_mut(|s| s.edit(id, body))
            .map_err(|e| NetError::NotAllowed(e.into()))?;
        if done && let Some(t) = self.target_of(id) {
            self.scheduled_changed(t);
        }
        Ok(done)
    }

    /// Moves item `id` to `due_ms`; a failed item waits again.
    pub fn reschedule(&self, id: u64, due_ms: u64) -> Result<bool> {
        let now = now_ms();
        let done = self
            .schedule_mut(|s| s.reschedule(id, due_ms, now))
            .map_err(|e| NetError::NotAllowed(e.into()))?;
        if done && let Some(t) = self.target_of(id) {
            self.scheduled_changed(t);
        }
        Ok(done)
    }

    /// Deletes item `id`; false if there's none, or it's being sent.
    pub fn cancel_scheduled(&self, id: u64) -> bool {
        let gone = self.schedule_mut(|s| s.cancel(id));
        if let Some(s) = &gone {
            self.scheduled_changed(s.target);
        }
        gone.is_some()
    }

    /// Sends item `id` (waiting, or failed) as soon as possible.
    pub fn send_scheduled_now(&self, id: u64) -> bool {
        let done = self.schedule_mut(|s| s.send_now(id, now_ms()));
        if done && let Some(t) = self.target_of(id) {
            self.scheduled_changed(t);
        }
        done
    }

    /// Whether conversation `conv` holds our message with local id `id`.
    pub fn already_sent(&self, conv: ConversationId, id: u64) -> bool {
        self.history(conv)
            .is_ok_and(|h| h.entries().iter().any(|e| e.outgoing && e.local_id == id))
    }

    /// The device to send a contact's item to: the one it was scheduled
    /// to while it's still a live contact device, else another live device
    /// of the same conversation (a connected one first). `None` if the
    /// contact was blocked or removed: sending would add it back.
    fn route(&self, s: &Scheduled) -> Option<PublicIdentity> {
        let Target::Peer { device, conv } = s.target else {
            return None;
        };
        let stored = PublicIdentity::from_bytes(&device).ok()?;
        let contacts: Vec<(PublicIdentity, bool)> = lock(&self.shared.contacts)
            .iter()
            .map(|c| (c.key, c.blocked))
            .collect();
        let live = |d: &PublicIdentity| {
            contacts.iter().any(|(k, blocked)| k == d && !blocked) && !self.is_revoked(d)
        };
        if live(&stored) {
            return Some(stored);
        }
        // Anyone of that conversation blocked means the contact is.
        let conv = ConversationId::Peer(conv);
        let mut others: Vec<PublicIdentity> = Vec::new();
        for (d, blocked) in &contacts {
            if *d == stored || self.conversation_for(d) != conv {
                continue;
            }
            if *blocked {
                return None;
            }
            if !self.is_revoked(d) {
                others.push(*d);
            }
        }
        let connected: Vec<PublicIdentity> = self.sessions().iter().map(|s| s.peer).collect();
        others
            .iter()
            .find(|d| connected.contains(d))
            .or(others.first())
            .copied()
    }

    /// Sends one due item to a contact; returns how it went.
    async fn send_scheduled_peer(&self, s: Scheduled) -> std::result::Result<(), Failure> {
        let sent = |n: &Self| {
            n.conversations_of(&s)
                .iter()
                .any(|c| n.already_sent(*c, s.id))
        };
        if sent(self) {
            return Ok(());
        }
        let Some(p) = self.route(&s) else {
            return Err(Failure::Contact);
        };
        self.seek(&p);
        let _ = tokio::time::timeout(REACH_FIRST, self.reach_peer(&p)).await;
        if self.is_closed() {
            return Err(Failure::Error);
        }
        let node = self.clone();
        let extras = Extras {
            quote: s.quote.clone(),
            ..Extras::default()
        };
        let (text, id) = (s.text.clone(), s.id);
        // History I/O: off the runtime's (few) workers.
        match tokio::task::spawn_blocking(move || node.send_message_with(&p, &text, extras, id))
            .await
        {
            Ok(Ok(_)) => Ok(()),
            _ => Err(Failure::Error),
        }
    }

    /// Sends what's due: contacts' items here (each to a live device),
    /// group items handed to the app's MLS engine (see
    /// [`Node::take_due_group_scheduled`]). Items more than a day late
    /// fail as missed instead. Returns how many items it handled.
    pub async fn send_due_scheduled(&self) -> usize {
        if self.is_closed() {
            return 0;
        }
        // Removed from our account: contacts no longer take us as us.
        let me = self.identity();
        if lock(&self.shared.account).state().removed.contains(&me) {
            return self.fail_all_scheduled(Failure::Removed);
        }
        let (send, missed) = {
            let mut s = lock(&self.shared.schedule.items);
            let r = s.due(now_ms());
            if !r.1.is_empty() {
                self.persist_schedule(&s);
            }
            r
        };
        for s in &missed {
            self.scheduled_failed(s, Failure::Missed);
            self.scheduled_changed(s.target);
        }
        let mut handled = missed.len();
        let mut peers = tokio::task::JoinSet::new();
        for s in send {
            if let Target::Group(g) = s.target {
                lock(&self.shared.schedule.items).to_groups.push(s);
                self.emit(Event::ScheduledChanged {
                    peer: None,
                    group: Some(g),
                });
                handled += 1;
                continue;
            }
            let node = self.clone();
            peers.spawn(async move {
                let id = s.id;
                let r = node.send_scheduled_peer(s).await;
                if !node.is_closed() {
                    node.finish_scheduled(id, r);
                }
            });
        }
        while peers.join_next().await.is_some() {
            handled += 1;
        }
        handled
    }

    /// The due group items, for the app's MLS engine to send (each then
    /// reported with [`Node::finish_scheduled`]).
    pub fn take_due_group_scheduled(&self) -> Vec<Scheduled> {
        std::mem::take(&mut lock(&self.shared.schedule.items).to_groups)
    }

    /// Ends item `id`'s send: removed when sent, else marked failed (the
    /// user is told).
    pub fn finish_scheduled(&self, id: u64, r: std::result::Result<(), Failure>) {
        let Some(s) = self.schedule_mut(|sch| sch.finish(id, r)) else {
            return;
        };
        if let Err(f) = r {
            self.scheduled_failed(&s, f);
        }
        self.scheduled_changed(s.target);
    }

    /// How many items are being sent right now.
    pub fn scheduled_sending(&self) -> usize {
        let s = lock(&self.shared.schedule.items);
        s.sending() + s.to_groups.len()
    }

    /// Drops conversation `conv`'s items: the user blocked or deleted it,
    /// or left the group.
    pub fn drop_scheduled(&self, conv: ConversationId) {
        let ids: Vec<u64> = self.scheduled_in(conv).into_iter().map(|s| s.id).collect();
        if ids.is_empty() {
            return;
        }
        let gone = self.schedule_mut(|s| s.retain(|x| !ids.contains(&x.id)));
        if let Some(s) = gone.first() {
            self.scheduled_changed(s.target);
        }
    }

    /// Fails every waiting item with `f`; returns how many.
    pub(crate) fn fail_all_scheduled(&self, f: Failure) -> usize {
        let failed = self.schedule_mut(|s| s.fail_waiting(f));
        for s in &failed {
            self.scheduled_failed(s, f);
            self.scheduled_changed(s.target);
        }
        failed.len()
    }

    /// Starts sending scheduled messages when they're due. Apps call this
    /// once they have applied the user's settings (disappearing timer,
    /// cover traffic), so nothing goes out with the defaults a node opens
    /// with. Later calls only wake the scheduler.
    pub fn start_scheduled_sending(&self) {
        if self.shared.schedule.started.swap(true, Ordering::SeqCst) {
            self.shared.schedule.kick.notify_one();
            return;
        }
        self.start_scheduler();
    }

    fn start_scheduler(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            // No runtime to run on: let a later call try again.
            self.shared.schedule.started.store(false, Ordering::SeqCst);
            return;
        };
        let node = self.clone();
        handle.spawn(async move {
            let closed = node.closed();
            tokio::pin!(closed);
            loop {
                let wait = node.next_scheduled_ms().map_or(CHECK_EVERY, |t| {
                    Duration::from_millis(t.saturating_sub(now_ms())).min(CHECK_EVERY)
                });
                tokio::select! {
                    () = tokio::time::sleep(wait) => {
                        node.send_due_scheduled().await;
                    }
                    () = node.shared.schedule.kick.notified() => {}
                    () = &mut closed => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;

    fn item(id: u64, due_ms: u64) -> Scheduled {
        Scheduled {
            id,
            target: Target::Peer {
                device: [id as u8; 32],
                conv: [9; 32],
            },
            due_ms,
            fire_ms: due_ms,
            created_ms: NOW,
            text: format!("message {id}"),
            quote: None,
            mentions: Vec::new(),
            status: Status::Waiting,
        }
    }

    fn with(items: Vec<Scheduled>) -> Schedule {
        Schedule {
            items,
            to_groups: Vec::new(),
        }
    }

    #[test]
    fn encodes_and_decodes_every_field() {
        let mut full = item(1, NOW + 5000);
        full.target = Target::Group([4; 16]);
        full.text = "hi @Al and @Bo".into();
        full.quote = Some(Quote::new(77, [3; 32], "the plan"));
        full.mentions = vec![
            Mention {
                start: 3,
                len: 3,
                who: [5; 32],
            },
            Mention {
                start: 11,
                len: 3,
                who: [6; 32],
            },
        ];
        full.status = Status::Failed(Failure::Group);
        let plain = item(2, NOW + 9000);
        let mut sending = item(3, NOW + 1);
        sending.status = Status::Sending;
        let s = with(vec![full.clone(), plain.clone(), sending.clone()]);
        let back = Schedule::decode(&s.encode().unwrap());
        sending.status = Status::Waiting;
        assert_eq!(back.items, vec![full, plain, sending]);
    }

    #[test]
    fn damaged_or_unknown_state_is_skipped_not_fatal() {
        assert!(Schedule::decode(&[]).is_empty());
        assert!(Schedule::decode(&[0xff, 0x00, 0x13]).is_empty());
        let s = with(vec![item(1, NOW + 1), item(2, NOW + 2)]);
        let b = s.encode().unwrap();
        for cut in 0..b.len() {
            let d = Schedule::decode(&b[..cut]);
            assert!(d.items.len() <= 2);
            assert!(d.items.iter().all(|x| s.items.contains(x)));
        }
        // An unknown key in an item is ignored.
        let b = cbor::to_vec(256, |e| {
            e.map_len(2)?.u8(0)?.u8(1)?;
            e.u8(1)?.array_len(1)?;
            e.map_len(7)?;
            e.u8(0)?.u64(5)?;
            e.u8(1)?.bytes(&[[1u8].as_slice(), &[7; 16]].concat())?;
            e.u8(2)?.u64(NOW)?;
            e.u8(3)?.u64(NOW)?;
            e.u8(4)?.u64(NOW)?;
            e.u8(5)?.str("hello")?;
            e.u8(40)?.str("from the future")?;
            Ok(())
        })
        .unwrap();
        let d = Schedule::decode(&b);
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].text, "hello");
        assert_eq!(d.items[0].target, Target::Group([7; 16]));
    }

    #[test]
    fn due_marks_sending_once_in_fire_order() {
        let mut a = item(1, NOW + 300);
        a.fire_ms = NOW + 300;
        let mut b = item(2, NOW + 100);
        b.fire_ms = NOW + 100;
        let mut c = item(3, NOW + 10_000);
        c.fire_ms = NOW + 10_000;
        let mut s = with(vec![a, b, c]);
        let (send, missed) = s.due(NOW + 500);
        assert!(missed.is_empty());
        assert_eq!(send.iter().map(|x| x.id).collect::<Vec<_>>(), vec![2, 1]);
        assert!(send.iter().all(|x| x.status == Status::Sending));
        assert_eq!(s.sending(), 2);
        let (again, _) = s.due(NOW + 500);
        assert!(again.is_empty());
        assert_eq!(s.next_fire(), Some(NOW + 10_000));
    }

    #[test]
    fn missed_by_a_day_fails_instead_of_sending() {
        let mut s = with(vec![item(1, NOW)]);
        let (send, missed) = s.due(NOW + MISSED_AFTER_MS + 1);
        assert!(send.is_empty());
        assert_eq!(missed.len(), 1);
        assert_eq!(s.items[0].status, Status::Failed(Failure::Missed));
        let mut s = with(vec![item(1, NOW)]);
        let (send, missed) = s.due(NOW + MISSED_AFTER_MS);
        assert_eq!(send.len(), 1);
        assert!(missed.is_empty());
    }

    #[test]
    fn jitter_never_fires_early_and_scales_with_lead_time() {
        assert_eq!(Schedule::jitter(0), 0);
        for lead in [1, 9, 10, 5_000, 60_000, 300_000, 3_600_000] {
            for _ in 0..1000 {
                let j = Schedule::jitter(lead);
                assert!(j <= MAX_JITTER_MS.min(lead / 10), "{j} for {lead}");
            }
        }
        let mut s = Schedule::default();
        s.add(item(1, NOW + 3_600_000), NOW).unwrap();
        let x = &s.items[0];
        assert!(x.fire_ms >= x.due_ms && x.fire_ms <= x.due_ms + MAX_JITTER_MS);
    }

    #[test]
    fn caps_count_text_size_and_horizon() {
        let mut s = Schedule::default();
        let mut blank = item(1, NOW + 1000);
        blank.text = " \n ".into();
        assert!(s.add(blank, NOW).is_err());
        let mut long = item(1, NOW + 1000);
        long.text = "x".repeat(MAX_SCHEDULED_TEXT + 1);
        assert_eq!(s.add(long, NOW), Err("Too long to schedule"));
        assert_eq!(s.add(item(1, NOW), NOW), Err("Pick a time in the future"));
        assert!(s.add(item(1, NOW + MAX_AHEAD_MS + 1), NOW).is_err());
        s.add(item(1, NOW + MAX_AHEAD_MS), NOW).unwrap();
        for i in 2..=MAX_SCHEDULED as u64 {
            s.add(item(i, NOW + 1000), NOW).unwrap();
        }
        assert_eq!(
            s.add(item(999, NOW + 1000), NOW),
            Err("Too many scheduled messages")
        );
        // The total.
        let mut s = Schedule::default();
        let mut id = 1;
        while s.text_bytes() + MAX_SCHEDULED_TEXT <= MAX_SCHEDULED_BYTES {
            let mut x = item(id, NOW + 1000);
            x.text = "y".repeat(MAX_SCHEDULED_TEXT - 1);
            s.add(x, NOW).unwrap();
            id += 1;
        }
        let mut x = item(id, NOW + 1000);
        x.text = "y".repeat(MAX_SCHEDULED_TEXT);
        assert!(s.add(x, NOW).is_err());
        let room = MAX_SCHEDULED_BYTES - s.text_bytes();
        let mut small = item(id, NOW + 1000);
        small.text = "z".repeat(room);
        s.add(small, NOW).unwrap();
        // An edit past the total is refused too.
        assert!(s.edit(id, &"z".repeat(room + 1)).is_err());
        assert_eq!(s.edit(id, "short"), Ok(true));
    }

    #[test]
    fn peer_target_keeps_device_and_conversation() {
        let t = Target::Peer {
            device: [1; 32],
            conv: [2; 32],
        };
        assert_eq!(Target::from_bytes(&t.to_bytes()), Some(t));
        let s = with(vec![item(1, NOW + 1)]);
        assert_eq!(Schedule::decode(&s.encode().unwrap()).items, s.items);
        // A target of the device alone (33 bytes) is skipped.
        let b = cbor::to_vec(256, |e| {
            e.map_len(2)?.u8(0)?.u8(1)?;
            e.u8(1)?.array_len(2)?;
            for (id, target) in [
                (1u64, [&[0u8][..], &[7; 32]].concat()),
                (2, Target::Group([3; 16]).to_bytes()),
            ] {
                e.map_len(6)?;
                e.u8(0)?.u64(id)?;
                e.u8(1)?.bytes(&target)?;
                e.u8(2)?.u64(NOW)?;
                e.u8(3)?.u64(NOW)?;
                e.u8(4)?.u64(NOW)?;
                e.u8(5)?.str("hi")?;
            }
            Ok(())
        })
        .unwrap();
        let d = Schedule::decode(&b);
        assert_eq!(d.items.len(), 1);
        assert_eq!(d.items[0].id, 2);
    }

    #[test]
    fn fail_waiting_marks_only_waiting_items() {
        let mut sending = item(1, NOW);
        sending.status = Status::Sending;
        let mut missed = item(2, NOW);
        missed.status = Status::Failed(Failure::Missed);
        let mut s = with(vec![sending, missed, item(3, NOW)]);
        let failed = s.fail_waiting(Failure::Removed);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, 3);
        assert_eq!(s.items[0].status, Status::Sending);
        assert_eq!(s.items[1].status, Status::Failed(Failure::Missed));
        assert_eq!(s.items[2].status, Status::Failed(Failure::Removed));
    }

    #[test]
    fn unknown_failure_code_reads_as_error() {
        let b = cbor::to_vec(256, |e| {
            e.map_len(2)?.u8(0)?.u8(1)?;
            e.u8(1)?.array_len(1)?;
            e.map_len(7)?;
            e.u8(0)?.u64(1)?;
            e.u8(1)?.bytes(&Target::Group([3; 16]).to_bytes())?;
            e.u8(2)?.u64(NOW)?;
            e.u8(3)?.u64(NOW)?;
            e.u8(4)?.u64(NOW)?;
            e.u8(5)?.str("hi")?;
            e.u8(8)?.u64(99)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            Schedule::decode(&b).items[0].status,
            Status::Failed(Failure::Error)
        );
    }

    #[test]
    fn edit_reschedule_send_now_and_cancel_respect_status() {
        let mut failed = item(2, NOW);
        failed.status = Status::Failed(Failure::Missed);
        let mut sending = item(3, NOW);
        sending.status = Status::Sending;
        let mut s = with(vec![item(1, NOW + 1000), failed, sending]);
        for id in [1, 2] {
            assert_eq!(s.edit(id, "new"), Ok(true));
            assert_eq!(s.reschedule(id, NOW + 5000, NOW), Ok(true));
            assert_eq!(s.get_mut(id).unwrap().status, Status::Waiting);
        }
        assert_eq!(s.edit(3, "new"), Ok(false));
        assert_eq!(s.reschedule(3, NOW + 5000, NOW), Ok(false));
        assert!(s.cancel(3).is_none());
        assert!(!s.send_now(3, NOW));
        assert!(s.reschedule(1, NOW, NOW).is_err());
        assert!(s.send_now(1, NOW + 7));
        let x = s.get_mut(1).unwrap();
        assert_eq!((x.due_ms, x.fire_ms), (NOW + 7, NOW + 7));
        s.get_mut(2).unwrap().status = Status::Failed(Failure::Error);
        assert!(s.send_now(2, NOW));
        assert_eq!(s.get_mut(2).unwrap().status, Status::Waiting);
        assert_eq!(s.cancel(1).map(|x| x.id), Some(1));
        assert_eq!(s.items.len(), 2);
    }

    #[test]
    fn edits_keep_only_mentions_whose_text_is_unchanged() {
        let m = |start, who| Mention {
            start,
            len: 3,
            who: [who; 32],
        };
        assert!(
            keep_mentions(
                "hi @Al and @Bo",
                "hey @Al and @Bob!",
                vec![m(3, 1), m(11, 2)]
            )
            .is_empty()
        );
        assert_eq!(
            keep_mentions("hi @Al!", "hi @Al?", vec![m(3, 1)]),
            vec![m(3, 1)]
        );
        assert!(keep_mentions("hi @Al", "hi", vec![m(3, 1)]).is_empty());
    }

    #[test]
    fn retain_drops_one_conversations_items() {
        let mut g = item(1, NOW);
        g.target = Target::Group([1; 16]);
        let mut g_sending = item(2, NOW);
        g_sending.target = Target::Group([1; 16]);
        g_sending.status = Status::Sending;
        let mut s = with(vec![g, g_sending, item(3, NOW)]);
        let gone = s.retain(|x| x.target != Target::Group([1; 16]));
        assert_eq!(gone.iter().map(|x| x.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(s.items.iter().map(|x| x.id).collect::<Vec<_>>(), vec![2, 3]);
    }
}
