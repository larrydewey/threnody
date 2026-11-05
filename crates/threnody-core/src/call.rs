//! Voice and video calls (Appendix Q): signalling, and the keys and packet
//! format that protect call media.
//!
//! Signalling travels inside the ratchet as `AppMessage::Call`:
//!
//! ```text
//! CallMsg = { 0: op, 1: call uint, ? 2: secret bstr(32), ? 3: flags uint,
//!             ? 4: reason uint, ? 5: data bstr, ? 6: emoji tstr }
//! op: 1 offer (secret, flags), 2 ringing, 3 answer (secret, flags),
//!     4 hangup (reason), 5 update (flags), 6 signal (data),
//!     7 reaction (emoji)
//! flags: 1 = video
//! ```
//!
//! The caller and the callee each contribute a fresh 32-byte secret (in the
//! offer and the answer), so the media keys are new for every call, need
//! both sides, and inherit the ratchet's post-quantum hybrid secrecy. They
//! are not tied to the session, so a call survives the session being
//! replaced (a network change, say), and they are erased when the call
//! ends.
//!
//! Media goes as datagrams where the link has them (else as
//! `AppMessage::Media`), each sealed on its own:
//!
//! ```text
//! packet = call (8) || seq (8) || AEAD(key_dir, nonce = 0^4 || seq,
//!                                      ad = call || seq,
//!                                      stream (1) || len (2) || payload || 0^pad)
//! ```
//!
//! A group call (up to [`MAX_GROUP_CALL`] devices) is a leg like this
//! between every two participants, keyed from the group's MLS exporter
//! secret ([`MediaKeys::for_leg`]); its signalling ([`GroupCallMsg`]) goes
//! in group messages. A pair without a session of its own sends its
//! packets through the call's hub, wrapped as
//! `0^8 || device (32) || packet` (call id 0 is never a call's).
//!
//! `seq` counts up from 0 in each direction, so a nonce is never reused;
//! a receiver drops anything it has seen or that is too old for its
//! replay window. The plaintext is padded to a multiple of
//! [`PAD_TO`] bytes, so packet sizes say less about what was said (media
//! layers should also use constant-bitrate audio).

use const_cbor::Decoder;
use zeroize::Zeroizing;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::crypto::aead::Suite;
use crate::crypto::kdf::{self, label};
use crate::error::{Error, Result};

/// Largest media payload one packet carries. Packets too big for the
/// path's QUIC datagrams (about 1150 bytes before path MTU discovery,
/// 1400 after on most paths) go in the stream instead.
pub const MAX_PAYLOAD: usize = 1400;
/// Largest media-layer signalling message (an SDP, say).
pub const MAX_SIGNAL: usize = 64 * 1024;
/// Longest reaction, in bytes: one emoji, with its skin tone and joiners.
pub const MAX_REACTION: usize = 32;
/// Media plaintext (stream, length, payload) is padded to a multiple of this.
pub const PAD_TO: usize = 32;
/// Bytes a sealed packet adds before padding: call, seq, stream, length, tag.
pub const OVERHEAD: usize = 8 + 8 + 1 + 2 + 16;
/// Most devices in one group call.
pub const MAX_GROUP_CALL: usize = 8;
/// Packets this far behind the newest one received are dropped as stale.
const REPLAY_WINDOW: u64 = 1024;
const SUITE: Suite = Suite::ChaCha20Poly1305;

const FLAG_VIDEO: u64 = 1;

/// Why a call ended. Unknown reasons decode as [`HangupReason::Ended`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HangupReason {
    /// Either side hung up an answered call, or the caller gave up.
    Ended = 0,
    /// The callee declined.
    Declined = 1,
    /// The callee is in another call.
    Busy = 2,
    /// Nobody answered in time.
    Unanswered = 3,
    /// Media couldn't flow (no route, or it stopped arriving).
    Failed = 4,
    /// Another device of the callee's account answered (to its siblings).
    AnsweredElsewhere = 5,
    /// Another device of the callee's account declined (to its siblings).
    DeclinedElsewhere = 6,
}

impl HangupReason {
    fn from_wire(v: u64) -> Self {
        match v {
            1 => Self::Declined,
            2 => Self::Busy,
            3 => Self::Unanswered,
            4 => Self::Failed,
            5 => Self::AnsweredElsewhere,
            6 => Self::DeclinedElsewhere,
            _ => Self::Ended,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum CallMsg {
    Offer {
        call: u64,
        secret: [u8; 32],
        video: bool,
    },
    Ringing {
        call: u64,
    },
    Answer {
        call: u64,
        secret: [u8; 32],
        video: bool,
    },
    Hangup {
        call: u64,
        reason: HangupReason,
    },
    /// The sender turned its video on or off.
    Update {
        call: u64,
        video: bool,
    },
    /// Opaque signalling for the media layer (session descriptions, ICE
    /// candidates), at most [`MAX_SIGNAL`] bytes.
    Signal {
        call: u64,
        data: Vec<u8>,
    },
    /// An emoji the sender wants floated over both sides' video, at most
    /// [`MAX_REACTION`] bytes.
    Reaction {
        call: u64,
        emoji: String,
    },
}

impl std::fmt::Debug for CallMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the secrets.
        match self {
            Self::Offer { call, video, .. } => write!(f, "Offer({call:x}, video={video})"),
            Self::Ringing { call } => write!(f, "Ringing({call:x})"),
            Self::Answer { call, video, .. } => write!(f, "Answer({call:x}, video={video})"),
            Self::Hangup { call, reason } => write!(f, "Hangup({call:x}, {reason:?})"),
            Self::Update { call, video } => write!(f, "Update({call:x}, video={video})"),
            Self::Signal { call, data } => write!(f, "Signal({call:x}, {} bytes)", data.len()),
            Self::Reaction { call, .. } => write!(f, "Reaction({call:x})"),
        }
    }
}

impl CallMsg {
    pub fn call(&self) -> u64 {
        match self {
            Self::Offer { call, .. }
            | Self::Ringing { call }
            | Self::Answer { call, .. }
            | Self::Hangup { call, .. }
            | Self::Update { call, .. }
            | Self::Signal { call, .. }
            | Self::Reaction { call, .. } => *call,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let flags = |video: bool| if video { FLAG_VIDEO } else { 0 };
        let size = match self {
            Self::Signal { data, .. } => data.len() + 32,
            _ => 96,
        };
        cbor::to_vec(size, |e| {
            match self {
                Self::Offer {
                    call,
                    secret,
                    video,
                }
                | Self::Answer {
                    call,
                    secret,
                    video,
                } => {
                    let op = if matches!(self, Self::Offer { .. }) {
                        1
                    } else {
                        3
                    };
                    e.map_len(4)?.u8(0)?.u8(op)?;
                    e.u8(1)?.u64(*call)?;
                    e.u8(2)?.bytes(secret)?;
                    e.u8(3)?.u64(flags(*video))?;
                }
                Self::Ringing { call } => {
                    e.map_len(2)?.u8(0)?.u8(2)?;
                    e.u8(1)?.u64(*call)?;
                }
                Self::Hangup { call, reason } => {
                    e.map_len(3)?.u8(0)?.u8(4)?;
                    e.u8(1)?.u64(*call)?;
                    e.u8(4)?.u8(*reason as u8)?;
                }
                Self::Update { call, video } => {
                    e.map_len(3)?.u8(0)?.u8(5)?;
                    e.u8(1)?.u64(*call)?;
                    e.u8(3)?.u64(flags(*video))?;
                }
                Self::Signal { call, data } => {
                    e.map_len(3)?.u8(0)?.u8(6)?;
                    e.u8(1)?.u64(*call)?;
                    e.u8(5)?.bytes(data)?;
                }
                Self::Reaction { call, emoji } => {
                    e.map_len(3)?.u8(0)?.u8(7)?;
                    e.u8(1)?.u64(*call)?;
                    e.u8(6)?.str(emoji)?;
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut call, mut secret, mut flags, mut reason) = (None, None, None, 0, 0);
        let (mut data, mut emoji) = (None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => call = Some(d.u64()?),
                2 => secret = Some(fixed_bytes::<32>(d)?),
                3 => flags = d.u64()?,
                4 => reason = d.u64()?,
                5 => data = Some(d.bytes()?.to_vec()),
                6 => emoji = Some(d.str()?.to_owned()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let call = required(call, "call id")?;
        if call == 0 {
            return Err(Error::Malformed("call id"));
        }
        let video = flags & FLAG_VIDEO != 0;
        Ok(match required(op, "call op")? {
            1 => Self::Offer {
                call,
                secret: required(secret, "call secret")?,
                video,
            },
            2 => Self::Ringing { call },
            3 => Self::Answer {
                call,
                secret: required(secret, "call secret")?,
                video,
            },
            4 => Self::Hangup {
                call,
                reason: HangupReason::from_wire(reason),
            },
            5 => Self::Update { call, video },
            6 => {
                let data = required(data, "call signal")?;
                if data.len() > MAX_SIGNAL {
                    return Err(Error::Malformed("call signal too large"));
                }
                Self::Signal { call, data }
            }
            7 => {
                let emoji = required(emoji, "call reaction")?;
                if !valid_reaction(&emoji) {
                    return Err(Error::Malformed("call reaction"));
                }
                Self::Reaction { call, emoji }
            }
            _ => return Err(Error::Malformed("call op")),
        })
    }
}

/// Whether `emoji` can be a call reaction: not empty, at most
/// [`MAX_REACTION`] bytes, and nothing but visible characters (no
/// control characters, so it can't break the line it's drawn on).
pub fn valid_reaction(emoji: &str) -> bool {
    !emoji.is_empty()
        && emoji.len() <= MAX_REACTION
        && !emoji.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Group call signalling, in a group message (Appendix Q, *Group calls*):
///
/// ```text
/// GroupCallMsg = { 0: op, 1: call uint, ? 2: device bstr(32), ? 3: flags uint,
///                  ? 4: data bstr, ? 5: secret bstr(32) }
/// op: 1 start (device = hub, flags, secret), 2 join (flags), 3 leave,
///     4 leg (device = to, data), 5 hub (device = new hub), 6 update (flags)
/// flags: 1 = video
/// ```
#[derive(Clone, PartialEq, Eq)]
pub enum GroupCallMsg {
    /// The sender started call `call`, relayed through `hub`. `secret`
    /// keys its legs ([`MediaKeys::for_leg`]): it goes only inside the
    /// group's encryption, and isn't tied to the group's epoch, so members
    /// at another epoch still agree on it.
    Start {
        call: u64,
        hub: [u8; 32],
        video: bool,
        secret: [u8; 32],
    },
    Join {
        call: u64,
        video: bool,
    },
    Leave {
        call: u64,
    },
    /// Media-layer signalling for the leg between the sender and `to`, at
    /// most [`MAX_SIGNAL`] bytes. Other members ignore it.
    Leg {
        call: u64,
        to: [u8; 32],
        data: Vec<u8>,
    },
    /// The hub left; `hub` relays from now on.
    Hub {
        call: u64,
        hub: [u8; 32],
    },
    /// The sender turned its video on or off.
    Update {
        call: u64,
        video: bool,
    },
}

impl std::fmt::Debug for GroupCallMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the secret, nor the leg's session descriptions.
        match self {
            Self::Start { call, video, .. } => write!(f, "Start({call:x}, video={video})"),
            Self::Join { call, video } => write!(f, "Join({call:x}, video={video})"),
            Self::Leave { call } => write!(f, "Leave({call:x})"),
            Self::Leg { call, data, .. } => write!(f, "Leg({call:x}, {} bytes)", data.len()),
            Self::Hub { call, .. } => write!(f, "Hub({call:x})"),
            Self::Update { call, video } => write!(f, "Update({call:x}, video={video})"),
        }
    }
}

impl GroupCallMsg {
    pub fn call(&self) -> u64 {
        match self {
            Self::Start { call, .. }
            | Self::Join { call, .. }
            | Self::Leave { call }
            | Self::Leg { call, .. }
            | Self::Hub { call, .. }
            | Self::Update { call, .. } => *call,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let flags = |video: bool| if video { FLAG_VIDEO } else { 0 };
        let size = match self {
            Self::Leg { data, .. } => data.len() + 80,
            _ => 80,
        };
        cbor::to_vec(size, |e| {
            match self {
                Self::Start {
                    call,
                    hub,
                    video,
                    secret,
                } => {
                    e.map_len(5)?.u8(0)?.u8(1)?.u8(1)?.u64(*call)?;
                    e.u8(2)?.bytes(hub)?.u8(3)?.u64(flags(*video))?;
                    e.u8(5)?.bytes(secret)?;
                }
                Self::Join { call, video } => {
                    e.map_len(3)?.u8(0)?.u8(2)?.u8(1)?.u64(*call)?;
                    e.u8(3)?.u64(flags(*video))?;
                }
                Self::Leave { call } => {
                    e.map_len(2)?.u8(0)?.u8(3)?.u8(1)?.u64(*call)?;
                }
                Self::Leg { call, to, data } => {
                    e.map_len(4)?.u8(0)?.u8(4)?.u8(1)?.u64(*call)?;
                    e.u8(2)?.bytes(to)?.u8(4)?.bytes(data)?;
                }
                Self::Hub { call, hub } => {
                    e.map_len(3)?.u8(0)?.u8(5)?.u8(1)?.u64(*call)?;
                    e.u8(2)?.bytes(hub)?;
                }
                Self::Update { call, video } => {
                    e.map_len(3)?.u8(0)?.u8(6)?.u8(1)?.u64(*call)?;
                    e.u8(3)?.u64(flags(*video))?;
                }
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut op, mut call, mut device, mut flags, mut data) = (None, None, None, 0, None);
        let mut secret = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => op = Some(d.u8()?),
                1 => call = Some(d.u64()?),
                2 => device = Some(fixed_bytes::<32>(d)?),
                3 => flags = d.u64()?,
                4 => data = Some(d.bytes()?.to_vec()),
                5 => secret = Some(fixed_bytes::<32>(d)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let call = required(call, "call id")?;
        if call == 0 {
            return Err(Error::Malformed("call id"));
        }
        let video = flags & FLAG_VIDEO != 0;
        let device = || required(device, "group call device");
        Ok(match required(op, "group call op")? {
            1 => Self::Start {
                call,
                hub: device()?,
                video,
                secret: required(secret, "group call secret")?,
            },
            2 => Self::Join { call, video },
            3 => Self::Leave { call },
            4 => {
                let data = required(data, "group call leg")?;
                if data.len() > MAX_SIGNAL {
                    return Err(Error::Malformed("group call leg too large"));
                }
                Self::Leg {
                    call,
                    to: device()?,
                    data,
                }
            }
            5 => Self::Hub {
                call,
                hub: device()?,
            },
            6 => Self::Update { call, video },
            _ => return Err(Error::Malformed("group call op")),
        })
    }
}

/// The id of the leg between devices `a` and `b` in group call `call`:
/// the same whichever side asks, never 0, and unlike any other leg's.
pub fn leg_id(call: u64, a: &[u8; 32], b: &[u8; 32]) -> u64 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let id = u64::from_be_bytes(kdf::derive(
        label::GROUP_CALL_LEG_ID,
        &[&call.to_be_bytes(), lo, hi],
    ));
    id.max(1)
}

/// Wraps a sealed packet for the hub to pass to (or from) `device`.
pub fn wrap_relayed(device: &[u8; 32], packet: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(40 + packet.len());
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(device);
    out.extend_from_slice(packet);
    out
}

/// The device and packet inside a relayed wrapper; `None` for an
/// ordinary packet.
pub fn unwrap_relayed(packet: &[u8]) -> Option<([u8; 32], &[u8])> {
    if call_of(packet)? != 0 {
        return None;
    }
    let device: [u8; 32] = packet.get(8..40)?.try_into().ok()?;
    Some((device, &packet[40..]))
}

/// The call id a sealed packet is for, to find its keys.
pub fn call_of(packet: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(packet.get(..8)?.try_into().ok()?))
}

/// One call's media keys and sequence state, for one side.
pub struct MediaKeys {
    call: u64,
    send: Zeroizing<[u8; 32]>,
    recv: Zeroizing<[u8; 32]>,
    next_seq: u64,
    /// The newest sequence number received, and which of the
    /// [`REPLAY_WINDOW`] before it were (bit i = `newest - i`).
    newest: Option<u64>,
    seen: [u64; (REPLAY_WINDOW / 64) as usize],
}

impl MediaKeys {
    /// Keys for `call` from the offer's and the answer's secrets; `caller`
    /// says which side we are, so each direction gets its own key.
    pub fn new(call: u64, offer: &[u8; 32], answer: &[u8; 32], caller: bool) -> Self {
        let base: Zeroizing<[u8; 32]> = Zeroizing::new(kdf::derive(
            label::CALL_MEDIA,
            &[offer, answer, &call.to_be_bytes()],
        ));
        let dir = |d: &[u8]| Zeroizing::new(kdf::derive(label::CALL_DIRECTION, &[&base[..], d]));
        let (to_callee, to_caller) = (dir(b"to callee"), dir(b"to caller"));
        let (send, recv) = if caller {
            (to_callee, to_caller)
        } else {
            (to_caller, to_callee)
        };
        Self {
            call,
            send,
            recv,
            next_seq: 0,
            newest: None,
            seen: [0; (REPLAY_WINDOW / 64) as usize],
        }
    }

    /// Keys for the leg between us (`me`) and `peer` in group call
    /// `call`, from the group's exporter secret for the call (`base`).
    /// The side with the smaller key counts as the caller.
    pub fn for_leg(base: &[u8; 32], call: u64, me: &[u8; 32], peer: &[u8; 32]) -> Self {
        let (lo, hi) = if me <= peer { (me, peer) } else { (peer, me) };
        let secret: Zeroizing<[u8; 32]> = Zeroizing::new(kdf::derive(
            label::GROUP_CALL_LEG,
            &[base, &call.to_be_bytes(), lo, hi],
        ));
        Self::new(leg_id(call, me, peer), &secret, &secret, me < peer)
    }

    pub fn call(&self) -> u64 {
        self.call
    }

    /// Seals `payload` (at most [`MAX_PAYLOAD`] bytes) on `stream`, an id
    /// the media layer chooses (audio, video, …).
    pub fn seal(&mut self, stream: u8, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() > MAX_PAYLOAD {
            return Err(Error::Malformed("media payload too large"));
        }
        let seq = self.next_seq;
        self.next_seq = seq
            .checked_add(1)
            .ok_or(Error::Malformed("media sequence"))?;
        let mut pt = Vec::with_capacity(payload.len() + PAD_TO + 3);
        pt.push(stream);
        pt.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        pt.extend_from_slice(payload);
        pt.resize(pt.len().div_ceil(PAD_TO) * PAD_TO, 0);
        let head = header(self.call, seq);
        let mut out = head.to_vec();
        out.extend(SUITE.seal(&self.send, &nonce(seq), &head, &pt));
        Ok(out)
    }

    /// Opens a packet: `(stream, payload)`. Fails on tampering, a replay,
    /// another call's packet, or one too old for the replay window.
    pub fn open(&mut self, packet: &[u8]) -> Result<(u8, Vec<u8>)> {
        if packet.len() < 16 + 16 || call_of(packet) != Some(self.call) {
            return Err(Error::Malformed("media packet"));
        }
        let seq = u64::from_be_bytes(packet[8..16].try_into().expect("8 bytes"));
        if !self.fresh(seq) {
            return Err(Error::Malformed("media replay"));
        }
        let pt = SUITE.open(&self.recv, &nonce(seq), &packet[..16], &packet[16..])?;
        if pt.len() < 3 {
            return Err(Error::Malformed("media plaintext"));
        }
        let len = u16::from_be_bytes([pt[1], pt[2]]) as usize;
        let payload = pt.get(3..3 + len).ok_or(Error::Malformed("media length"))?;
        let out = (pt[0], payload.to_vec());
        self.mark(seq);
        Ok(out)
    }

    /// Whether `seq` is new and within the window.
    fn fresh(&self, seq: u64) -> bool {
        match self.newest {
            None => true,
            Some(n) if seq > n => true,
            Some(n) => {
                let back = n - seq;
                back < REPLAY_WINDOW && self.seen[(back / 64) as usize] & (1 << (back % 64)) == 0
            }
        }
    }

    fn mark(&mut self, seq: u64) {
        let Some(n) = self.newest.filter(|n| seq <= *n) else {
            // A new newest: shift the window by the gap.
            let shift = self.newest.map_or(REPLAY_WINDOW, |n| seq - n);
            self.shift(shift);
            self.newest = Some(seq);
            self.seen[0] |= 1;
            return;
        };
        let back = n - seq;
        self.seen[(back / 64) as usize] |= 1 << (back % 64);
    }

    /// Moves every bit `by` places further back (older), dropping those
    /// that leave the window.
    fn shift(&mut self, by: u64) {
        if by >= REPLAY_WINDOW {
            self.seen = [0; (REPLAY_WINDOW / 64) as usize];
            return;
        }
        let (words, bits) = ((by / 64) as usize, (by % 64) as u32);
        let n = self.seen.len();
        for i in (0..n).rev() {
            let lo = i.checked_sub(words).map_or(0, |j| self.seen[j]);
            let carry = match i.checked_sub(words + 1) {
                Some(j) if bits > 0 => self.seen[j] >> (64 - bits),
                _ => 0,
            };
            self.seen[i] = if bits > 0 { lo << bits } else { lo } | carry;
        }
    }
}

fn header(call: u64, seq: u64) -> [u8; 16] {
    let mut h = [0u8; 16];
    h[..8].copy_from_slice(&call.to_be_bytes());
    h[8..].copy_from_slice(&seq.to_be_bytes());
    h
}

fn nonce(seq: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&seq.to_be_bytes());
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signalling_round_trips_and_validates() {
        let msgs = [
            CallMsg::Offer {
                call: 7,
                secret: [1; 32],
                video: true,
            },
            CallMsg::Ringing { call: 7 },
            CallMsg::Answer {
                call: 7,
                secret: [2; 32],
                video: false,
            },
            CallMsg::Hangup {
                call: 7,
                reason: HangupReason::Busy,
            },
            CallMsg::Update {
                call: 7,
                video: true,
            },
            CallMsg::Signal {
                call: 7,
                data: b"v=0".to_vec(),
            },
            CallMsg::Reaction {
                call: 7,
                emoji: "👍🏽".into(),
            },
        ];
        for m in msgs.clone() {
            assert_eq!(CallMsg::decode(&m.encode().unwrap()).unwrap(), m);
            assert_eq!(m.call(), 7);
        }
        assert!(
            !format!("{:?}", msgs[0]).contains("1, 1"),
            "secret not logged"
        );
        // A call id of 0, an unknown op, a missing secret: refused.
        let raw = |f: &dyn Fn(
            &mut const_cbor::Encoder<'_>,
        ) -> std::result::Result<(), const_cbor::Error>| {
            CallMsg::decode(&cbor::to_vec(64, f).unwrap())
        };
        assert!(
            raw(&|e| {
                e.map_len(2)?.u8(0)?.u8(2)?.u8(1)?.u64(0)?;
                Ok(())
            })
            .is_err()
        );
        assert!(
            raw(&|e| {
                e.map_len(2)?.u8(0)?.u8(9)?.u8(1)?.u64(1)?;
                Ok(())
            })
            .is_err()
        );
        assert!(
            raw(&|e| {
                e.map_len(2)?.u8(0)?.u8(1)?.u8(1)?.u64(1)?;
                Ok(())
            })
            .is_err()
        );
        let big = CallMsg::Signal {
            call: 1,
            data: vec![0; MAX_SIGNAL + 1],
        };
        assert!(CallMsg::decode(&big.encode().unwrap()).is_err());
        // Reactions: one emoji, no line breaks or whitespace, not too long.
        for bad in ["", "a\nb", " ", &"❤".repeat(11)] {
            let m = CallMsg::Reaction {
                call: 1,
                emoji: bad.to_owned(),
            };
            assert!(CallMsg::decode(&m.encode().unwrap()).is_err(), "{bad:?}");
        }
        assert!(valid_reaction("👨‍👩‍👧‍👦") && valid_reaction("❤️"));
        // Unknown hang-up reasons are a plain end.
        let odd = raw(&|e| {
            e.map_len(3)?.u8(0)?.u8(4)?.u8(1)?.u64(1)?.u8(4)?.u64(99)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            odd,
            CallMsg::Hangup {
                call: 1,
                reason: HangupReason::Ended
            }
        );
    }

    #[test]
    fn group_call_signalling_round_trips() {
        let msgs = [
            GroupCallMsg::Start {
                call: 5,
                hub: [1; 32],
                video: true,
                secret: [4; 32],
            },
            GroupCallMsg::Join {
                call: 5,
                video: false,
            },
            GroupCallMsg::Leave { call: 5 },
            GroupCallMsg::Leg {
                call: 5,
                to: [2; 32],
                data: b"o v=0".to_vec(),
            },
            GroupCallMsg::Hub {
                call: 5,
                hub: [3; 32],
            },
            GroupCallMsg::Update {
                call: 5,
                video: true,
            },
        ];
        for m in msgs {
            assert_eq!(GroupCallMsg::decode(&m.encode().unwrap()).unwrap(), m);
            assert_eq!(m.call(), 5);
        }
        let big = GroupCallMsg::Leg {
            call: 1,
            to: [0; 32],
            data: vec![0; MAX_SIGNAL + 1],
        };
        assert!(GroupCallMsg::decode(&big.encode().unwrap()).is_err());
        assert!(GroupCallMsg::decode(&GroupCallMsg::Leave { call: 0 }.encode().unwrap()).is_err());
    }

    #[test]
    fn group_call_legs_pair_up_and_differ() {
        let (a, b, c) = ([1; 32], [2; 32], [3; 32]);
        let base = [9; 32];
        assert_eq!(leg_id(7, &a, &b), leg_id(7, &b, &a));
        assert_ne!(leg_id(7, &a, &b), leg_id(7, &a, &c));
        assert_ne!(leg_id(7, &a, &b), leg_id(8, &a, &b));
        let mut ab = MediaKeys::for_leg(&base, 7, &a, &b);
        let mut ba = MediaKeys::for_leg(&base, 7, &b, &a);
        assert_eq!(ab.call(), leg_id(7, &a, &b));
        let p = ab.seal(0, b"hi").unwrap();
        assert_eq!(ba.open(&p).unwrap(), (0, b"hi".to_vec()));
        assert_eq!(ab.open(&ba.seal(0, b"yo").unwrap()).unwrap().1, b"yo");
        // Another leg, or another call's base, can't open it.
        let mut ac = MediaKeys::for_leg(&base, 7, &c, &a);
        assert!(ac.open(&ab.seal(0, b"x").unwrap()).is_err());
        let mut other = MediaKeys::for_leg(&[8; 32], 7, &b, &a);
        assert!(other.open(&ab.seal(0, b"x").unwrap()).is_err());
        // The hub's wrapper.
        let w = wrap_relayed(&c, &p);
        assert_eq!(unwrap_relayed(&w), Some((c, &p[..])));
        assert_eq!(unwrap_relayed(&p), None);
    }

    fn pair() -> (MediaKeys, MediaKeys) {
        let (o, a) = ([3; 32], [4; 32]);
        (
            MediaKeys::new(9, &o, &a, true),
            MediaKeys::new(9, &o, &a, false),
        )
    }

    #[test]
    fn media_seals_both_ways_with_padding() {
        let (mut caller, mut callee) = pair();
        let p = caller.seal(1, b"hello").unwrap();
        assert_eq!(call_of(&p), Some(9));
        assert_eq!(p.len(), 16 + PAD_TO + 16, "padded");
        assert_eq!(callee.open(&p).unwrap(), (1, b"hello".to_vec()));
        let q = callee.seal(2, &[5; MAX_PAYLOAD]).unwrap();
        assert!(q.len() <= MAX_PAYLOAD + OVERHEAD + PAD_TO);
        assert_eq!(caller.open(&q).unwrap(), (2, vec![5; MAX_PAYLOAD]));
        assert!(caller.seal(1, &[0; MAX_PAYLOAD + 1]).is_err());
        // Directions have different keys: our own packet doesn't open here.
        let mine = caller.seal(1, b"x").unwrap();
        assert!(caller.open(&mine).is_err());
        // Nor does another call's, or a tampered one.
        let (mut other, _) = {
            let (o, a) = ([3; 32], [4; 32]);
            (MediaKeys::new(10, &o, &a, true), ())
        };
        assert!(callee.open(&other.seal(1, b"x").unwrap()).is_err());
        let mut bad = caller.seal(1, b"y").unwrap();
        *bad.last_mut().unwrap() ^= 1;
        assert!(callee.open(&bad).is_err());
        // Different secrets, different keys.
        let mut stranger = MediaKeys::new(9, &[3; 32], &[5; 32], false);
        assert!(stranger.open(&caller.seal(1, b"z").unwrap()).is_err());
    }

    #[test]
    fn replays_and_stale_packets_are_dropped_reordering_is_not() {
        let (mut tx, mut rx) = pair();
        let pkts: Vec<_> = (0..2000).map(|i| tx.seal(0, &[i as u8]).unwrap()).collect();
        assert!(rx.open(&pkts[5]).is_ok());
        assert!(rx.open(&pkts[5]).is_err(), "replay");
        assert!(rx.open(&pkts[3]).is_ok(), "late but in the window");
        assert!(rx.open(&pkts[3]).is_err());
        assert!(rx.open(&pkts[70]).is_ok());
        assert!(rx.open(&pkts[4]).is_ok(), "across a word boundary");
        assert!(rx.open(&pkts[5]).is_err(), "still remembered after a shift");
        assert!(rx.open(&pkts[70]).is_err());
        assert!(rx.open(&pkts[1500]).is_ok());
        assert!(rx.open(&pkts[400]).is_err(), "older than the window");
        assert!(rx.open(&pkts[1499]).is_ok());
        assert!(rx.open(&pkts[1500 - 1023]).is_ok(), "oldest in the window");
        assert!(rx.open(&pkts[1500 - 1023]).is_err());
        // Every packet, shuffled within small distances, opens exactly once.
        let (mut tx, mut rx) = pair();
        let pkts: Vec<_> = (0..500).map(|_| tx.seal(0, b"m").unwrap()).collect();
        let mut order: Vec<usize> = (0..500).collect();
        for c in order.chunks_mut(37) {
            c.reverse();
        }
        assert!(order.iter().all(|&i| rx.open(&pkts[i]).is_ok()));
        assert!(order.iter().all(|&i| rx.open(&pkts[i]).is_err()));
    }
}
