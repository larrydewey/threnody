//! Notices: lines in a chat's history that say what happened in it,
//! rather than what someone wrote (a message deleted, a call, a member
//! joining). They are kept with the history, on this device and the
//! user's others, and drawn as a quiet centred line.
//!
//! Each is stored at the history entry's key 23 as a CBOR map: kind at key
//! 0, then the kind's own fields. A kind this version doesn't know decodes
//! as nothing, and the entry's `text` (always a plain sentence) stands in.

use crate::cbor;
use crate::error::{Error, Result};

const KIND_DELETED: u64 = 1;
const KIND_CALL: u64 = 2;
const KIND_MEMBER: u64 = 3;
const KIND_ROLE: u64 = 4;
const KIND_TIMER: u64 = 5;
const KIND_SECURITY: u64 = 6;

const CALL_VIDEO: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Where a message was until it was deleted for everyone: by its
    /// sender, or (on our own messages) by us.
    Deleted,
    /// A call with the contact (the entry's `outgoing` says who called):
    /// its id (so it's noted once), whether video, how it went, and how
    /// long it lasted once answered.
    Call {
        call: u64,
        video: bool,
        outcome: CallOutcome,
        duration_s: u32,
    },
    /// Someone joined a group, left it, or was added or removed (`by`:
    /// who did it, when known).
    Member {
        change: MemberChange,
        member: [u8; 32],
        by: Option<[u8; 32]>,
    },
    /// A group member became owner (0), admin (1) or a plain member (2).
    Role { member: [u8; 32], role: u8 },
    /// Disappearing messages were set (seconds; 0: off). The entry's
    /// `outgoing` says whether we did it.
    Timer { secs: u32 },
    /// A change to who's at the other end: a contact's new or removed
    /// device, or our verifying them.
    Security {
        change: SecurityChange,
        device: Option<[u8; 32]>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberChange {
    Added = 0,
    Left = 1,
    Removed = 2,
    /// We joined (the member is us).
    Joined = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecurityChange {
    /// The contact added a device to their account: its safety number is
    /// new until compared.
    NewDevice = 0,
    DeviceRemoved = 1,
    /// We compared safety numbers and marked them verified.
    Verified = 2,
}

/// The short form of a device's fingerprint (`ABCD-EFGH`), for when no
/// name is known.
pub fn short_name(device: &[u8; 32]) -> String {
    crate::PublicIdentity::from_bytes(device)
        .map(|p| p.fingerprint().to_string().chars().take(9).collect())
        .unwrap_or_else(|_| "Someone".into())
}

/// A timer's length in words: "5 minutes", "1 day", "4 weeks".
fn period(secs: u32) -> String {
    let (n, unit) = match secs {
        s if s % 604_800 == 0 => (s / 604_800, "week"),
        s if s % 86_400 == 0 => (s / 86_400, "day"),
        s if s % 3600 == 0 => (s / 3600, "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// How a call went, as the device noting it saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    /// Answered here (then it has a duration).
    Answered = 0,
    /// Rang here and no one answered: counts as unread.
    Missed = 1,
    /// Declined: by us here or on another of our devices (incoming), or
    /// by them (outgoing).
    Declined = 2,
    /// They were in another call (outgoing), or we were (incoming: missed).
    Busy = 3,
    /// Our call rang out unanswered.
    NoAnswer = 4,
    /// We hung up before it was answered.
    Cancelled = 5,
    /// It couldn't connect, or its media stopped.
    Failed = 6,
    /// Another of our devices answered it.
    AnsweredElsewhere = 7,
}

impl CallOutcome {
    fn from_wire(v: u64) -> Self {
        match v {
            1 => Self::Missed,
            2 => Self::Declined,
            3 => Self::Busy,
            4 => Self::NoAnswer,
            5 => Self::Cancelled,
            6 => Self::Failed,
            7 => Self::AnsweredElsewhere,
            _ => Self::Answered,
        }
    }
}

/// A call's length in words: "45 s", "12 min", "1 h 5 min".
fn duration(s: u32) -> String {
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s / 60),
        _ if s % 3600 / 60 == 0 => format!("{} h", s / 3600),
        _ => format!("{} h {} min", s / 3600, s % 3600 / 60),
    }
}

impl Notice {
    /// The notice in a plain sentence, as the entry's text: what any
    /// version shows. People are named by their short fingerprint; see
    /// [`Notice::describe`] to name them.
    pub fn text(&self, outgoing: bool) -> String {
        self.describe(outgoing, &short_name)
    }

    /// The notice in a plain sentence, naming each device by `name` (its
    /// contact name, or "You" for ours).
    pub fn describe(&self, outgoing: bool, name: &dyn Fn(&[u8; 32]) -> String) -> String {
        match *self {
            Self::Deleted if outgoing => "You deleted this message".into(),
            Self::Deleted => "This message was deleted".into(),
            Self::Call {
                video,
                outcome,
                duration_s,
                ..
            } => {
                let kind = if video { "video call" } else { "voice call" };
                let mut cap = kind.to_owned();
                cap[..1].make_ascii_uppercase();
                match (outcome, outgoing) {
                    (CallOutcome::Answered, _) => format!("{cap} · {}", duration(duration_s)),
                    (CallOutcome::Missed, _) => format!("Missed {kind}"),
                    (CallOutcome::Declined, true) => format!("{cap} · declined"),
                    (CallOutcome::Declined, false) => format!("You declined a {kind}"),
                    (CallOutcome::Busy, true) => format!("{cap} · they were busy"),
                    (CallOutcome::Busy, false) => {
                        format!("Missed {kind} · you were on another call")
                    }
                    (CallOutcome::NoAnswer, _) => format!("{cap} · no answer"),
                    (CallOutcome::Cancelled, _) => format!("Cancelled {kind}"),
                    (CallOutcome::Failed, _) => format!("{cap} · couldn't connect"),
                    (CallOutcome::AnsweredElsewhere, _) => {
                        format!("{cap} · answered on another device")
                    }
                }
            }
            Self::Member { change, member, by } => {
                let who = name(&member);
                let by = by.filter(|b| *b != member).map(|b| name(&b));
                match (change, by) {
                    (MemberChange::Joined, _) => "You joined the group".into(),
                    (MemberChange::Added, Some(by)) => format!("{by} added {who}"),
                    (MemberChange::Added, None) => format!("{who} joined"),
                    (MemberChange::Left, _) => format!("{who} left"),
                    (MemberChange::Removed, Some(by)) => format!("{by} removed {who}"),
                    (MemberChange::Removed, None) => format!("{who} was removed"),
                }
            }
            Self::Role { member, role } => {
                let who = name(&member);
                match (role, who == "You") {
                    (0, true) => "You're now an owner".into(),
                    (1, true) => "You're now an admin".into(),
                    (_, true) => "You're now a member".into(),
                    (0, false) => format!("{who} is now an owner"),
                    (1, false) => format!("{who} is now an admin"),
                    _ => format!("{who} is no longer an admin"),
                }
            }
            Self::Timer { secs } => {
                let who = if outgoing { "You" } else { "They" };
                if secs == 0 {
                    format!("{who} turned off disappearing messages")
                } else {
                    format!("{who} set messages to disappear after {}", period(secs))
                }
            }
            Self::Security { change, .. } => match change {
                SecurityChange::NewDevice => "This contact added a new device. Compare safety \
                                              numbers to be sure it's them."
                    .into(),
                SecurityChange::DeviceRemoved => "This contact removed a device".into(),
                SecurityChange::Verified => "You verified the safety number".into(),
            },
        }
    }

    /// Whether this notice, from the contact's side, counts as unread: a
    /// call that rang here unanswered.
    pub fn is_unread_kind(&self) -> bool {
        matches!(
            self,
            Self::Call {
                outcome: CallOutcome::Missed | CallOutcome::Busy,
                ..
            }
        )
    }
}

impl Notice {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(16, |e| {
            match *self {
                Self::Deleted => {
                    e.map_len(1)?.u8(0)?.uint(KIND_DELETED)?;
                }
                Self::Call {
                    call,
                    video,
                    outcome,
                    duration_s,
                } => {
                    e.map_len(4 + usize::from(video))?.u8(0)?.uint(KIND_CALL)?;
                    e.u8(1)?.u64(call)?;
                    if video {
                        e.u8(2)?.uint(CALL_VIDEO)?;
                    }
                    e.u8(3)?.uint(outcome as u64)?;
                    e.u8(4)?.u32(duration_s)?;
                }
                Self::Member { change, member, by } => {
                    e.map_len(3 + usize::from(by.is_some()))?
                        .u8(0)?
                        .uint(KIND_MEMBER)?;
                    e.u8(1)?.uint(change as u64)?;
                    e.u8(2)?.bytes(&member)?;
                    if let Some(b) = by {
                        e.u8(5)?.bytes(&b)?;
                    }
                }
                Self::Role { member, role } => {
                    e.map_len(3)?.u8(0)?.uint(KIND_ROLE)?;
                    e.u8(2)?.bytes(&member)?;
                    e.u8(4)?.u32(u32::from(role))?;
                }
                Self::Timer { secs } => {
                    e.map_len(2)?.u8(0)?.uint(KIND_TIMER)?;
                    e.u8(4)?.u32(secs)?;
                }
                Self::Security { change, device } => {
                    e.map_len(2 + usize::from(device.is_some()))?
                        .u8(0)?
                        .uint(KIND_SECURITY)?;
                    e.u8(1)?.uint(change as u64)?;
                    if let Some(d) = device {
                        e.u8(2)?.bytes(&d)?;
                    }
                }
            }
            Ok(())
        })
    }

    /// The notice in `b`; `None` for a kind this version doesn't know.
    pub fn decode(b: &[u8]) -> Result<Option<Self>> {
        let mut dec = const_cbor::Decoder::new(b);
        let mut kind = None;
        let (mut one, mut two, mut three, mut four, mut five) = (0, None, 0, 0, None);
        cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u64()?),
                1 => one = d.u64()?,
                // A flag word (calls), or a device.
                2 => {
                    two = Some(match d.peek_major() {
                        Some(const_cbor::Major::Bytes) => Err(cbor::fixed_bytes::<32>(d)?),
                        _ => Ok(d.u64()?),
                    });
                }
                3 => three = d.u64()?,
                4 => four = d.u32()?,
                5 => five = Some(cbor::fixed_bytes::<32>(d)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        let (flags, device) = match two {
            Some(Ok(f)) => (f, None),
            Some(Err(d)) => (0, Some(d)),
            None => (0, None),
        };
        let (call, outcome, duration_s) = (one, three, four);
        cbor::finish(&dec)?;
        Ok(match kind.ok_or(Error::Malformed("notice kind"))? {
            KIND_DELETED => Some(Self::Deleted),
            KIND_CALL => Some(Self::Call {
                call,
                video: flags & CALL_VIDEO != 0,
                outcome: CallOutcome::from_wire(outcome),
                duration_s,
            }),
            KIND_MEMBER => Some(Self::Member {
                change: match one {
                    1 => MemberChange::Left,
                    2 => MemberChange::Removed,
                    3 => MemberChange::Joined,
                    _ => MemberChange::Added,
                },
                member: device.ok_or(Error::Malformed("notice member"))?,
                by: five,
            }),
            KIND_ROLE => Some(Self::Role {
                member: device.ok_or(Error::Malformed("notice member"))?,
                role: u8::try_from(four).unwrap_or(2),
            }),
            KIND_TIMER => Some(Self::Timer { secs: four }),
            KIND_SECURITY => Some(Self::Security {
                change: match one {
                    1 => SecurityChange::DeviceRemoved,
                    2 => SecurityChange::Verified,
                    _ => SecurityChange::NewDevice,
                },
                device,
            }),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_round_trip_and_unknown_kinds_are_skipped() {
        let b = Notice::Deleted.encode().unwrap();
        assert_eq!(Notice::decode(&b).unwrap(), Some(Notice::Deleted));
        // A later kind, with a field this version doesn't know.
        let later = cbor::to_vec(16, |e| {
            e.map_len(2)?.u8(0)?.uint(99)?.u8(1)?.u32(7)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(Notice::decode(&later).unwrap(), None);
        assert!(Notice::decode(&[0xa0]).is_err(), "a notice has a kind");
    }

    #[test]
    fn group_and_security_notices_round_trip_and_name_people() {
        let (a, b) = ([1; 32], [2; 32]);
        let name = |d: &[u8; 32]| {
            if *d == a {
                "Alice".to_owned()
            } else {
                "You".to_owned()
            }
        };
        let member = |change, member, by| Notice::Member { change, member, by };
        let cases = [
            (member(MemberChange::Added, b, Some(a)), "Alice added You"),
            (member(MemberChange::Left, a, Some(a)), "Alice left"),
            (
                member(MemberChange::Removed, a, Some(b)),
                "You removed Alice",
            ),
            (Notice::Role { member: a, role: 1 }, "Alice is now an admin"),
            (Notice::Role { member: b, role: 0 }, "You're now an owner"),
            (
                Notice::Timer { secs: 86_400 },
                "You set messages to disappear after 1 day",
            ),
            (
                Notice::Timer { secs: 0 },
                "You turned off disappearing messages",
            ),
            (
                Notice::Security {
                    change: SecurityChange::Verified,
                    device: Some(a),
                },
                "You verified the safety number",
            ),
        ];
        for (n, said) in cases {
            assert_eq!(Notice::decode(&n.encode().unwrap()).unwrap(), Some(n));
            assert_eq!(n.describe(true, &name), said);
        }
        assert_eq!(
            Notice::Timer { secs: 300 }.text(false),
            "They set messages to disappear after 5 minutes"
        );
    }

    #[test]
    fn calls_round_trip_and_read_as_sentences() {
        let call = |video, outcome, duration_s| Notice::Call {
            call: 7,
            video,
            outcome,
            duration_s,
        };
        for n in [
            call(true, CallOutcome::Answered, 754),
            call(false, CallOutcome::Missed, 0),
            call(false, CallOutcome::AnsweredElsewhere, 0),
        ] {
            assert_eq!(Notice::decode(&n.encode().unwrap()).unwrap(), Some(n));
        }
        assert_eq!(
            call(true, CallOutcome::Answered, 754).text(true),
            "Video call · 12 min"
        );
        assert_eq!(
            call(false, CallOutcome::Answered, 45).text(false),
            "Voice call · 45 s"
        );
        assert_eq!(
            call(false, CallOutcome::Answered, 3900).text(false),
            "Voice call · 1 h 5 min"
        );
        assert_eq!(
            call(false, CallOutcome::Missed, 0).text(false),
            "Missed voice call"
        );
        assert_eq!(
            call(true, CallOutcome::Declined, 0).text(false),
            "You declined a video call"
        );
        assert_eq!(
            call(false, CallOutcome::NoAnswer, 0).text(true),
            "Voice call · no answer"
        );
        assert!(call(false, CallOutcome::Missed, 0).is_unread_kind());
        assert!(!call(false, CallOutcome::Answered, 3).is_unread_kind());
        assert_eq!(Notice::Deleted.text(true), "You deleted this message");
    }
}
