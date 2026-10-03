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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppMessage {
    /// Opens the responder's sending chain; carries no user content.
    Hello,
    Text {
        sent_ms: u64,
        body: String,
    },
    File {
        sent_ms: u64,
        name: String,
        data: Vec<u8>,
    },
    /// Our current mesh/tunnel approval of the peer (spec §5.2). Sent at
    /// session start and whenever it changes.
    Approval {
        approved: bool,
    },
    /// Cover traffic: indistinguishable on the wire, discarded on receipt.
    Cover,
    /// Offer a WireGuard tunnel (spec §8). Only sent and honoured between
    /// mutually approved peers; the preshared key comes from the session.
    TunnelOffer {
        wg_public: [u8; 32],
        port: u16,
    },
}

mod kind {
    pub const HELLO: u64 = 0;
    pub const TEXT: u64 = 1;
    pub const FILE: u64 = 2;
    pub const APPROVAL: u64 = 3;
    pub const COVER: u64 = 4;
    pub const TUNNEL_OFFER: u64 = 5;
}

impl AppMessage {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(self.size_hint(), |e| {
            match self {
                Self::Hello => {
                    e.map_len(1)?.u8(0)?.uint(kind::HELLO)?;
                }
                Self::Cover => {
                    e.map_len(1)?.u8(0)?.uint(kind::COVER)?;
                }
                Self::Text { sent_ms, body } => {
                    e.map_len(3)?.u8(0)?.uint(kind::TEXT)?;
                    e.u8(1)?.uint(*sent_ms)?;
                    e.u8(2)?.str(body)?;
                }
                Self::File {
                    sent_ms,
                    name,
                    data,
                } => {
                    e.map_len(4)?.u8(0)?.uint(kind::FILE)?;
                    e.u8(1)?.uint(*sent_ms)?;
                    e.u8(2)?.bytes(data)?;
                    e.u8(3)?.str(name)?;
                }
                Self::TunnelOffer { wg_public, port } => {
                    e.map_len(3)?.u8(0)?.uint(kind::TUNNEL_OFFER)?;
                    e.u8(2)?.bytes(wg_public)?;
                    e.u8(3)?.u16(*port)?;
                }
                Self::Approval { approved } => {
                    e.map_len(2)?.u8(0)?.uint(kind::APPROVAL)?;
                    e.u8(2)?.bool(*approved)?;
                }
            }
            Ok(())
        })
    }

    fn size_hint(&self) -> usize {
        match self {
            Self::Text { body, .. } => body.len() + 32,
            Self::File { name, data, .. } => name.len() + data.len() + 48,
            _ => 16,
        }
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut k, mut ts, mut text, mut bytes, mut flag, mut name, mut port) =
            (None, None, None, None, None, None, None);
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
                    _ => port = Some(d.u16()?),
                },
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(k, "message kind")? {
            kind::HELLO => Self::Hello,
            kind::COVER => Self::Cover,
            kind::TEXT => Self::Text {
                sent_ms: ts.unwrap_or(0),
                body: required(text, "text body")?,
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
            AppMessage::Hello,
            AppMessage::Cover,
            AppMessage::Text {
                sent_ms: 42,
                body: "héllo".into(),
            },
            AppMessage::File {
                sent_ms: 1,
                name: "a.txt".into(),
                data: vec![0, 1, 2],
            },
            AppMessage::Approval { approved: true },
            AppMessage::TunnelOffer {
                wg_public: [7; 32],
                port: 51820,
            },
        ] {
            assert_eq!(AppMessage::decode(&m.encode().unwrap()).unwrap(), m);
        }
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
