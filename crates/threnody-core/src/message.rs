//! Application messages carried inside the ratchet (spec §6.1, §6.3), and the
//! length-hiding padding applied before encryption (spec §9, layer 1).
//!
//! ```text
//! AppMessage = { 0: kind uint, ? 1: sent_ms uint, ? 2: body, * uint => any }
//! ```

use const_cbor::Decoder;

use crate::cbor::{self, finish, read_map, required};
use crate::error::{Error, Result};

/// Smallest padded plaintext size.
pub const PAD_MIN: usize = 256;
/// Above this, padding rounds up to a multiple of this size instead of the
/// next power of two.
pub const PAD_STEP_MAX: usize = 64 * 1024;
/// Largest file a single message may carry.
pub const MAX_FILE: usize = 8 * 1024 * 1024;
/// `File` flag (key 6): the sender marked it sensitive.
pub const FILE_SENSITIVE: u64 = 1;
/// `Hello` feature bit: this side acknowledges `Tracked` messages with
/// `Ack` and accepts them (so the other side may send both).
pub const FEATURE_ACKS: u64 = 1;
/// `Hello` feature bit: this side understands `Delete` (so it can be sent).
pub const FEATURE_DELETE: u64 = 2;
/// `Hello` feature bit: this side understands `Edit`.
pub const FEATURE_EDIT: u64 = 4;
/// Every feature this implementation has.
/// The peer understands `AppMessage::Identity` (profiles, reveals).
pub const FEATURE_IDENTITY: u64 = 8;
/// The peer understands `AppMessage::React`.
pub const FEATURE_REACT: u64 = 16;
/// The peer understands `AppMessage::Observed`.
pub const FEATURE_OBSERVED: u64 = 32;
/// The peer understands `AppMessage::Paths` and keeps the lease it implies.
pub const FEATURE_PATHS: u64 = 64;
pub const FEATURES: u64 = FEATURE_ACKS
    | FEATURE_DELETE
    | FEATURE_EDIT
    | FEATURE_IDENTITY
    | FEATURE_REACT
    | FEATURE_OBSERVED
    | FEATURE_PATHS;
/// `React` flag (key 6): take the reaction away.
const REACT_REMOVE: u64 = 1;
/// Most ids one `Ack` carries.
pub const MAX_ACK_IDS: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppMessage {
    /// The first message each side sends: opens the responder's sending
    /// chain, and says which optional features this side supports
    /// (`FEATURE_*` bits; 0 encodes exactly as before features existed).
    Hello { features: u64 },
    Text {
        sent_ms: u64,
        body: String,
        /// Disappearing-message timer (spec §6.1): both sides delete the
        /// message this many seconds after sending.
        expires_in_s: Option<u32>,
        /// The sender's id for this message, so it can later be deleted
        /// for everyone (0 = none).
        id: u64,
    },
    File {
        sent_ms: u64,
        name: String,
        data: Vec<u8>,
        /// As for `Text`.
        id: u64,
        /// The sender marked it sensitive: show it covered until opened.
        sensitive: bool,
        /// Text sent with it, as with a photo (empty if none).
        caption: String,
        /// Files sent together share a random album id (0 = alone); the
        /// caption comes with the first.
        album: u64,
    },
    /// Our current mesh/tunnel approval of the peer (spec §5.2). Sent at
    /// session start and whenever it changes.
    Approval { approved: bool },
    /// Cover traffic: indistinguishable on the wire, discarded on receipt.
    Cover,
    /// Offer a WireGuard tunnel (spec §8). Only sent and honoured between
    /// mutually approved peers; the preshared key comes from the session.
    TunnelOffer { wg_public: [u8; 32], port: u16 },
    /// An opaque group-layer message (MLS, see `threnody-groups`).
    Group(Vec<u8>),
    /// An opaque relay-circuit message (see `threnody-net::relay`).
    Relay(Vec<u8>),
    /// Our prekey bundle for the peer (`prekey::PrekeyBundle`, Appendix H).
    Prekeys(Vec<u8>),
    /// An opaque mailbox message (see `threnody-net::mailbox`).
    Mailbox(Vec<u8>),
    /// An opaque onion-circuit message (see `threnody-net::onion`).
    Onion(Vec<u8>),
    /// An opaque account / device-linking message (see `threnody-net::account`).
    Account(Vec<u8>),
    /// An opaque Wi-Fi Direct link message (see `threnody-net::direct`).
    Direct(Vec<u8>),
    /// A profile or a revealed identity ([`crate::persona::IdentityMsg`]).
    Identity(Vec<u8>),
    /// `inner`, which the receiver acknowledges by `id` and delivers once
    /// even if it arrives again. Only sent to peers whose `Hello` has
    /// [`FEATURE_ACKS`]; never nested.
    Tracked { id: u64, inner: Box<AppMessage> },
    /// Acknowledges `Tracked` messages by id.
    Ack(Vec<u64>),
    /// Delete these messages (by the sender's `id`s). From a peer: its own
    /// messages, "delete for everyone". From one of our devices: any
    /// messages in `conversation`, which it deleted. Only sent to peers
    /// whose `Hello` has [`FEATURE_DELETE`].
    Delete {
        conversation: Vec<u8>,
        ids: Vec<u64>,
    },
    /// Replace the text of message `id` (the sender's own; or, from one of
    /// our devices, in `conversation`). Only sent to peers whose `Hello`
    /// has [`FEATURE_EDIT`].
    Edit {
        conversation: Vec<u8>,
        id: u64,
        body: String,
    },
    /// Add (or with `add` false, take away) the sender's `emoji` on message
    /// `id`, whoever sent it (or, from one of our devices, in
    /// `conversation`). Only sent to peers whose `Hello` has
    /// [`FEATURE_REACT`].
    React {
        conversation: Vec<u8>,
        id: u64,
        emoji: String,
        add: bool,
    },
    /// The address and port the sender sees us at (Appendix N): our
    /// outside address, for rendezvous. Only to a peer whose `Hello` has
    /// [`FEATURE_OBSERVED`].
    Observed { addr: std::net::SocketAddr },
    /// Our paths and heartbeat (`rendezvous::Paths`), for repairing a lost
    /// session fast. Only to a mutually approved peer whose `Hello` has
    /// [`FEATURE_PATHS`].
    Paths(Vec<u8>),
}

mod kind {
    pub const HELLO: u64 = 0;
    pub const TEXT: u64 = 1;
    pub const FILE: u64 = 2;
    pub const APPROVAL: u64 = 3;
    pub const COVER: u64 = 4;
    pub const TUNNEL_OFFER: u64 = 5;
    pub const GROUP: u64 = 6;
    pub const RELAY: u64 = 7;
    pub const PREKEYS: u64 = 8;
    pub const MAILBOX: u64 = 9;
    pub const ONION: u64 = 10;
    pub const ACCOUNT: u64 = 11;
    pub const DIRECT: u64 = 12;
    pub const TRACKED: u64 = 13;
    pub const ACK: u64 = 14;
    pub const DELETE: u64 = 15;
    pub const EDIT: u64 = 16;
    pub const IDENTITY: u64 = 17;
    pub const REACT: u64 = 18;
    pub const OBSERVED: u64 = 19;
    pub const PATHS: u64 = 20;
}

impl AppMessage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        // These wrap bytes built first, so the (retried) encoder below
        // never re-encodes them.
        match self {
            Self::Tracked { id, inner } => {
                let inner = inner.encode()?;
                return cbor::to_vec(inner.len() + 32, |e| {
                    e.map_len(3)?.u8(0)?.uint(kind::TRACKED)?;
                    e.u8(2)?.bytes(&inner)?;
                    e.u8(5)?.uint(*id)?;
                    Ok(())
                });
            }
            Self::Ack(ids) => {
                let packed: Vec<u8> = ids.iter().flat_map(|i| i.to_be_bytes()).collect();
                return cbor::to_vec(packed.len() + 16, |e| {
                    e.map_len(2)?.u8(0)?.uint(kind::ACK)?;
                    e.u8(2)?.bytes(&packed)?;
                    Ok(())
                });
            }
            Self::Edit {
                conversation,
                id,
                body,
            } => {
                return cbor::to_vec(body.len() + conversation.len() + 32, |e| {
                    e.map_len(4)?.u8(0)?.uint(kind::EDIT)?;
                    e.u8(2)?.str(body)?;
                    e.u8(3)?.bytes(conversation)?;
                    e.u8(5)?.uint(*id)?;
                    Ok(())
                });
            }
            Self::React {
                conversation,
                id,
                emoji,
                add,
            } => {
                return cbor::to_vec(emoji.len() + conversation.len() + 40, |e| {
                    e.map_len(4 + usize::from(!*add))?
                        .u8(0)?
                        .uint(kind::REACT)?;
                    e.u8(2)?.str(emoji)?;
                    e.u8(3)?.bytes(conversation)?;
                    e.u8(5)?.uint(*id)?;
                    if !*add {
                        e.u8(6)?.uint(REACT_REMOVE)?;
                    }
                    Ok(())
                });
            }
            Self::Delete { conversation, ids } => {
                let packed: Vec<u8> = ids.iter().flat_map(|i| i.to_be_bytes()).collect();
                return cbor::to_vec(packed.len() + conversation.len() + 24, |e| {
                    e.map_len(3)?.u8(0)?.uint(kind::DELETE)?;
                    e.u8(2)?.bytes(&packed)?;
                    e.u8(3)?.bytes(conversation)?;
                    Ok(())
                });
            }
            _ => {}
        }
        cbor::to_vec(self.size_hint(), |e| {
            match self {
                Self::Hello { features } => {
                    e.map_len(1 + usize::from(*features != 0))?
                        .u8(0)?
                        .uint(kind::HELLO)?;
                    if *features != 0 {
                        e.u8(5)?.uint(*features)?;
                    }
                }
                Self::Tracked { .. }
                | Self::Ack(_)
                | Self::Delete { .. }
                | Self::Edit { .. }
                | Self::React { .. } => {
                    unreachable!("encoded above")
                }
                Self::Cover => {
                    e.map_len(1)?.u8(0)?.uint(kind::COVER)?;
                }
                Self::Text {
                    sent_ms,
                    body,
                    expires_in_s,
                    id,
                } => {
                    e.map_len(3 + usize::from(expires_in_s.is_some()) + usize::from(*id != 0))?
                        .u8(0)?
                        .uint(kind::TEXT)?;
                    e.u8(1)?.uint(*sent_ms)?;
                    e.u8(2)?.str(body)?;
                    if let Some(x) = expires_in_s {
                        e.u8(4)?.u32(*x)?;
                    }
                    if *id != 0 {
                        e.u8(5)?.uint(*id)?;
                    }
                }
                Self::File {
                    sent_ms,
                    name,
                    data,
                    id,
                    sensitive,
                    caption,
                    album,
                } => {
                    e.map_len(
                        4 + usize::from(*id != 0)
                            + usize::from(*sensitive)
                            + usize::from(!caption.is_empty())
                            + usize::from(*album != 0),
                    )?
                    .u8(0)?
                    .uint(kind::FILE)?;
                    e.u8(1)?.uint(*sent_ms)?;
                    e.u8(2)?.bytes(data)?;
                    e.u8(3)?.str(name)?;
                    if *id != 0 {
                        e.u8(5)?.uint(*id)?;
                    }
                    if *sensitive {
                        e.u8(6)?.uint(FILE_SENSITIVE)?;
                    }
                    if !caption.is_empty() {
                        e.u8(7)?.str(caption)?;
                    }
                    if *album != 0 {
                        e.u8(8)?.uint(*album)?;
                    }
                }
                Self::TunnelOffer { wg_public, port } => {
                    e.map_len(3)?.u8(0)?.uint(kind::TUNNEL_OFFER)?;
                    e.u8(2)?.bytes(wg_public)?;
                    e.u8(3)?.u16(*port)?;
                }
                Self::Group(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::GROUP)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Relay(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::RELAY)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Prekeys(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::PREKEYS)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Mailbox(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::MAILBOX)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Onion(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::ONION)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Account(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::ACCOUNT)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Direct(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::DIRECT)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Identity(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::IDENTITY)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Paths(payload) => {
                    e.map_len(2)?.u8(0)?.uint(kind::PATHS)?;
                    e.u8(2)?.bytes(payload)?;
                }
                Self::Approval { approved } => {
                    e.map_len(2)?.u8(0)?.uint(kind::APPROVAL)?;
                    e.u8(2)?.bool(*approved)?;
                }
                Self::Observed { addr } => {
                    e.map_len(3)?.u8(0)?.uint(kind::OBSERVED)?;
                    match addr.ip() {
                        std::net::IpAddr::V4(a) => e.u8(2)?.bytes(&a.octets())?,
                        std::net::IpAddr::V6(a) => e.u8(2)?.bytes(&a.octets())?,
                    };
                    e.u8(3)?.u16(addr.port())?;
                }
            }
            Ok(())
        })
    }

    /// Roughly how many bytes this message encodes to.
    pub fn encoded_len_hint(&self) -> usize {
        self.size_hint()
    }

    fn size_hint(&self) -> usize {
        match self {
            Self::Text { body, .. } => body.len() + 32,
            Self::File {
                name,
                data,
                caption,
                ..
            } => name.len() + data.len() + caption.len() + 64,
            Self::Group(p)
            | Self::Relay(p)
            | Self::Prekeys(p)
            | Self::Mailbox(p)
            | Self::Onion(p)
            | Self::Paths(p)
            | Self::Identity(p) => p.len() + 16,
            Self::Tracked { inner, .. } => inner.size_hint() + 32,
            Self::Ack(ids) => ids.len() * 8 + 16,
            _ => 16,
        }
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        Self::decode_at(b, false)
    }

    /// `inside` is true for the message inside a `Tracked`, which may not
    /// be a wrapper itself. That is checked before decoding any deeper, so
    /// nesting can't recurse.
    fn decode_at(b: &[u8], inside: bool) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut k, mut ts, mut text, mut bytes, mut flag, mut name, mut port, mut expiry) =
            (None, None, None, None, None, None, None, None);
        let mut five = None;
        let mut conv = None;
        let mut flags = 0;
        let (mut caption, mut album) = (None, 0);
        read_map(&mut dec, |key, d| {
            match key {
                0 => k = Some(d.u64()?),
                1 => ts = Some(d.u64()?),
                2 => match d.peek_major() {
                    Some(const_cbor::Major::Text) => text = Some(d.str()?.to_owned()),
                    Some(const_cbor::Major::Bytes) => bytes = Some(d.bytes()?.to_vec()),
                    _ => flag = Some(d.bool()?),
                },
                3 => match d.peek_major() {
                    Some(const_cbor::Major::Text) => name = Some(d.str()?.to_owned()),
                    Some(const_cbor::Major::Bytes) => conv = Some(d.bytes()?.to_vec()),
                    _ => port = Some(d.u16()?),
                },
                4 => expiry = Some(d.u32()?),
                5 => five = Some(d.u64()?),
                6 => flags = d.u64()?,
                7 => caption = Some(d.str()?.to_owned()),
                8 => album = d.u64()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if inside && matches!(k, Some(kind::TRACKED | kind::ACK)) {
            return Err(Error::Malformed("nested tracked message"));
        }
        Ok(match required(k, "message kind")? {
            kind::HELLO => Self::Hello {
                features: five.unwrap_or(0),
            },
            kind::TRACKED => {
                let inner = Self::decode_at(&required(bytes, "tracked message")?, true)?;
                Self::Tracked {
                    id: required(five, "message id")?,
                    inner: Box::new(inner),
                }
            }
            kind::EDIT => Self::Edit {
                conversation: required(conv, "conversation")?,
                id: required(five, "message id")?,
                body: required(text, "text body")?,
            },
            kind::REACT => {
                let emoji = required(text, "emoji")?;
                if !crate::history::valid_emoji(&emoji) {
                    return Err(Error::Malformed("emoji"));
                }
                Self::React {
                    conversation: required(conv, "conversation")?,
                    id: required(five, "message id")?,
                    emoji,
                    add: flags & REACT_REMOVE == 0,
                }
            }
            kind::DELETE => {
                let packed = required(bytes, "deleted ids")?;
                if packed.len() % 8 != 0 || packed.len() / 8 > MAX_ACK_IDS {
                    return Err(Error::Malformed("delete"));
                }
                Self::Delete {
                    conversation: required(conv, "conversation")?,
                    ids: packed
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_be_bytes(*c))
                        .collect(),
                }
            }
            kind::ACK => {
                let packed = required(bytes, "acknowledged ids")?;
                if packed.len() % 8 != 0 || packed.len() / 8 > MAX_ACK_IDS {
                    return Err(Error::Malformed("ack"));
                }
                Self::Ack(
                    packed
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u64::from_be_bytes(*c))
                        .collect(),
                )
            }
            kind::COVER => Self::Cover,
            kind::TEXT => Self::Text {
                sent_ms: ts.unwrap_or(0),
                body: required(text, "text body")?,
                expires_in_s: expiry,
                id: five.unwrap_or(0),
            },
            kind::FILE => {
                let data = required(bytes, "file data")?;
                if data.len() > MAX_FILE {
                    return Err(Error::Malformed("file too large"));
                }
                Self::File {
                    sent_ms: ts.unwrap_or(0),
                    name: required(name, "file name")?,
                    data,
                    id: five.unwrap_or(0),
                    sensitive: flags & FILE_SENSITIVE != 0,
                    caption: caption.unwrap_or_default(),
                    album,
                }
            }
            kind::GROUP
            | kind::RELAY
            | kind::PREKEYS
            | kind::MAILBOX
            | kind::ONION
            | kind::ACCOUNT
            | kind::DIRECT
            | kind::PATHS
            | kind::IDENTITY => {
                let p = required(bytes, "payload")?;
                if p.len() > MAX_FILE + 4096 {
                    return Err(Error::Malformed("message too large"));
                }
                match k {
                    Some(kind::GROUP) => Self::Group(p),
                    Some(kind::RELAY) => Self::Relay(p),
                    Some(kind::PREKEYS) => Self::Prekeys(p),
                    Some(kind::ONION) => Self::Onion(p),
                    Some(kind::ACCOUNT) => Self::Account(p),
                    Some(kind::DIRECT) => Self::Direct(p),
                    Some(kind::IDENTITY) => Self::Identity(p),
                    Some(kind::PATHS) => Self::Paths(p),
                    _ => Self::Mailbox(p),
                }
            }
            kind::TUNNEL_OFFER => Self::TunnelOffer {
                wg_public: required(bytes, "wireguard key")?
                    .try_into()
                    .map_err(|_| Error::Malformed("wireguard key length"))?,
                port: required(port, "wireguard port")?,
            },
            kind::APPROVAL => Self::Approval {
                approved: required(flag, "approval flag")?,
            },
            kind::OBSERVED => {
                let b = required(bytes, "observed address")?;
                let ip = match <[u8; 4]>::try_from(b.as_slice()) {
                    Ok(v4) => std::net::IpAddr::from(v4),
                    Err(_) => std::net::IpAddr::from(
                        <[u8; 16]>::try_from(b.as_slice())
                            .map_err(|_| Error::Malformed("observed address"))?,
                    ),
                };
                Self::Observed {
                    addr: std::net::SocketAddr::new(ip, required(port, "observed port")?),
                }
            }
            other => return Err(Error::UnexpectedType(other)),
        })
    }
}

/// Pads with ISO/IEC 7816-4 (`0x80` then zeros) up to the next bucket:
/// powers of two from [`PAD_MIN`], then multiples of [`PAD_STEP_MAX`].
pub fn pad(mut msg: Vec<u8>) -> Vec<u8> {
    let target = padded_len(msg.len() + 1);
    msg.push(0x80);
    msg.resize(target, 0);
    msg
}

pub fn padded_len(n: usize) -> usize {
    if n <= PAD_MIN {
        PAD_MIN
    } else if n <= PAD_STEP_MAX {
        n.next_power_of_two()
    } else {
        n.div_ceil(PAD_STEP_MAX) * PAD_STEP_MAX
    }
}

pub fn unpad(mut msg: Vec<u8>) -> Result<Vec<u8>> {
    let end = msg
        .iter()
        .rposition(|&b| b != 0)
        .ok_or(Error::Malformed("padding"))?;
    if msg[end] != 0x80 {
        return Err(Error::Malformed("padding"));
    }
    msg.truncate(end);
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        for m in [
            AppMessage::Hello { features: 0 },
            AppMessage::Observed {
                addr: "203.0.113.9:7450".parse().unwrap(),
            },
            AppMessage::Paths(vec![1, 2, 3]),
            AppMessage::Observed {
                addr: "[2001:db8::7]:61000".parse().unwrap(),
            },
            AppMessage::Hello {
                features: FEATURE_ACKS,
            },
            AppMessage::Tracked {
                id: u64::MAX,
                inner: Box::new(AppMessage::Group(vec![1])),
            },
            AppMessage::Tracked {
                id: 7,
                inner: Box::new(AppMessage::File {
                    sent_ms: 1,
                    name: "big".into(),
                    data: vec![3; MAX_FILE],
                    id: 0,
                    sensitive: false,
                    caption: String::new(),
                    album: 0,
                }),
            },
            AppMessage::Ack(vec![]),
            AppMessage::Ack(vec![1, u64::MAX]),
            AppMessage::Delete {
                conversation: vec![0; 33],
                ids: vec![7, u64::MAX],
            },
            AppMessage::Edit {
                conversation: vec![1; 17],
                id: 9,
                body: "better wording".into(),
            },
            AppMessage::Cover,
            AppMessage::Text {
                sent_ms: 42,
                body: "héllo".into(),
                expires_in_s: None,
                id: 0,
            },
            AppMessage::Text {
                sent_ms: 43,
                body: "gone soon".into(),
                expires_in_s: Some(30),
                id: 0,
            },
            AppMessage::File {
                sent_ms: 1,
                name: "a.txt".into(),
                data: vec![0, 1, 2],
                id: 0,
                sensitive: false,
                caption: String::new(),
                album: 0,
            },
            AppMessage::File {
                sent_ms: 2,
                name: "IMG_1.jpg".into(),
                data: vec![0xFF, 0xD8],
                id: 9,
                sensitive: true,
                caption: "from the summit".into(),
                album: u64::MAX,
            },
            AppMessage::React {
                conversation: vec![1; 33],
                id: 7,
                emoji: "👍🏽".into(),
                add: true,
            },
            AppMessage::React {
                conversation: vec![2; 17],
                id: u64::MAX,
                emoji: "❤️".into(),
                add: false,
            },
            AppMessage::Approval { approved: true },
            AppMessage::TunnelOffer {
                wg_public: [7; 32],
                port: 51820,
            },
            AppMessage::Group(vec![1, 2, 3]),
            AppMessage::Relay(vec![4, 5]),
            AppMessage::Prekeys(vec![6]),
            AppMessage::Mailbox(vec![7]),
            AppMessage::Onion(vec![8]),
            AppMessage::Account(vec![9]),
            AppMessage::Direct(vec![10]),
        ] {
            assert_eq!(AppMessage::decode(&m.encode().unwrap()).unwrap(), m);
        }
    }

    #[test]
    fn hello_without_features_is_unchanged_and_tracking_is_bounded() {
        // Old peers sent (and expect) exactly this.
        assert_eq!(
            AppMessage::Hello { features: 0 }.encode().unwrap(),
            [0xa1, 0, 0]
        );
        let ack = |n: usize| AppMessage::Ack((0..n as u64).collect()).encode().unwrap();
        assert!(AppMessage::decode(&ack(MAX_ACK_IDS)).is_ok());
        assert!(AppMessage::decode(&ack(MAX_ACK_IDS + 1)).is_err());
        let nested = AppMessage::Tracked {
            id: 1,
            inner: Box::new(AppMessage::Tracked {
                id: 2,
                inner: Box::new(AppMessage::Cover),
            }),
        };
        assert!(AppMessage::decode(&nested.encode().unwrap()).is_err());
        let acked = AppMessage::Tracked {
            id: 1,
            inner: Box::new(AppMessage::Ack(vec![1])),
        };
        assert!(AppMessage::decode(&acked.encode().unwrap()).is_err());
        // Deep nesting is refused at the second level, without recursing.
        let mut deep = AppMessage::Cover;
        for i in 0..10_000 {
            deep = AppMessage::Tracked {
                id: i,
                inner: Box::new(deep),
            };
        }
        let deep = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || deep.encode().unwrap())
            .unwrap()
            .join()
            .unwrap();
        let r = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || AppMessage::decode(&deep).is_err())
            .unwrap()
            .join()
            .unwrap();
        assert!(r);
        // A tracked message needs its id.
        let mut no_id = acked.encode().unwrap();
        no_id[0] = 0xa2;
        assert!(AppMessage::decode(&no_id).is_err());
    }

    #[test]
    fn padding_hides_length_within_bucket() {
        let a = pad(b"hi".to_vec());
        let b = pad(vec![7u8; 200]);
        assert_eq!(a.len(), PAD_MIN);
        assert_eq!(b.len(), PAD_MIN);
        assert_eq!(unpad(a).unwrap(), b"hi");
        assert_eq!(pad(vec![1; 300]).len(), 512);
        assert_eq!(padded_len(PAD_STEP_MAX + 1), 2 * PAD_STEP_MAX);
        assert!(unpad(vec![0; 10]).is_err());
        assert_eq!(unpad(pad(vec![0x80, 0])).unwrap(), vec![0x80, 0]);
    }
}
