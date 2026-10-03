//! Top-level wire envelope (spec §6.3, §11).
//!
//! ```text
//! Envelope = { 0: major uint, 1: minor uint, 2: type uint, 3: body bstr, * uint => any }
//! ```
//!
//! Every frame on every transport is exactly one `Envelope`. Unknown keys are
//! ignored; an unknown *major* version is rejected outright because all
//! envelope types are security-critical.

use const_cbor::Decoder;

use crate::cbor::{self, finish, read_map, required};
use crate::error::{Error, Result};

pub const VERSION_MAJOR: u64 = 1;
pub const VERSION_MINOR: u64 = 0;

/// Envelope type codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    HandshakeInit = 1,
    HandshakeResp = 2,
    HandshakeFinish = 3,
    Ratchet = 4,
}

impl MsgType {
    fn from_wire(v: u64) -> Result<Self> {
        Ok(match v {
            1 => Self::HandshakeInit,
            2 => Self::HandshakeResp,
            3 => Self::HandshakeFinish,
            4 => Self::Ratchet,
            other => return Err(Error::UnexpectedType(other)),
        })
    }
}

pub fn encode_envelope(ty: MsgType, body: &[u8]) -> Result<Vec<u8>> {
    cbor::to_vec(body.len() + 16, |e| {
        e.map_len(4)?;
        e.u8(0)?.uint(VERSION_MAJOR)?;
        e.u8(1)?.uint(VERSION_MINOR)?;
        e.u8(2)?.u8(ty as u8)?;
        e.u8(3)?.bytes(body)?;
        Ok(())
    })
}

/// Parses an envelope, returning its type and borrowed body.
pub fn decode_envelope(frame: &[u8]) -> Result<(MsgType, &[u8])> {
    let mut dec = Decoder::new(frame);
    let (mut major, mut ty, mut body) = (None, None, None);
    read_map(&mut dec, |k, d| {
        match k {
            0 => major = Some(d.u64()?),
            1 => {
                d.u64()?;
            }
            2 => ty = Some(d.u64()?),
            3 => body = Some(d.bytes()?),
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    finish(&dec)?;
    let major = required(major, "envelope version")?;
    if major != VERSION_MAJOR {
        return Err(Error::UnsupportedVersion(major));
    }
    let ty = MsgType::from_wire(required(ty, "envelope type")?)?;
    Ok((ty, required(body, "envelope body")?))
}

/// Decodes an envelope and checks it has the expected type.
pub fn expect(frame: &[u8], want: MsgType) -> Result<&[u8]> {
    let (ty, body) = decode_envelope(frame)?;
    if ty != want {
        return Err(Error::UnexpectedType(ty as u64));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let f = encode_envelope(MsgType::Ratchet, b"xyz").unwrap();
        assert_eq!(
            decode_envelope(&f).unwrap(),
            (MsgType::Ratchet, &b"xyz"[..])
        );
    }

    #[test]
    fn rejects_future_major_and_ignores_unknown_keys() {
        // {0: 2, 2: 4, 3: h''}
        assert!(matches!(
            decode_envelope(&[0xa3, 0x00, 0x02, 0x02, 0x04, 0x03, 0x40]),
            Err(Error::UnsupportedVersion(2))
        ));
        // {0: 1, 2: 4, 3: h'', 9: "x"}
        let f = [0xa4, 0x00, 0x01, 0x02, 0x04, 0x03, 0x40, 0x09, 0x61, b'x'];
        assert_eq!(decode_envelope(&f).unwrap().0, MsgType::Ratchet);
    }

    #[test]
    fn rejects_duplicate_keys_and_trailing_bytes() {
        let dup = [0xa3, 0x00, 0x01, 0x00, 0x01, 0x03, 0x40];
        assert!(decode_envelope(&dup).is_err());
        let mut f = encode_envelope(MsgType::Ratchet, b"").unwrap();
        f.push(0);
        assert!(decode_envelope(&f).is_err());
    }
}
