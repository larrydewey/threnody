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
    /// version shows.
    pub fn text(&self, outgoing: bool) -> String {
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
            }
            Ok(())
        })
    }

    /// The notice in `b`; `None` for a kind this version doesn't know.
    pub fn decode(b: &[u8]) -> Result<Option<Self>> {
        let mut dec = const_cbor::Decoder::new(b);
        let mut kind = None;
        let (mut call, mut flags, mut outcome, mut duration_s) = (0, 0, 0, 0);
        cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u64()?),
                1 => call = d.u64()?,
                2 => flags = d.u64()?,
                3 => outcome = d.u64()?,
                4 => duration_s = d.u32()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        cbor::finish(&dec)?;
        Ok(match kind.ok_or(Error::Malformed("notice kind"))? {
            KIND_DELETED => Some(Self::Deleted),
            KIND_CALL => Some(Self::Call {
                call,
                video: flags & CALL_VIDEO != 0,
                outcome: CallOutcome::from_wire(outcome),
                duration_s,
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
