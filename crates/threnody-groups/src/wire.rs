//! Group control messages, carried in `AppMessage::Group` over the 1:1
//! ratchets (so MLS traffic is itself end-to-end encrypted per hop).
//!
//! ```text
//! GroupWire = { 0: kind uint, 1: group_id bstr, ? 2: payload bstr, ? 3: name tstr }
//! kind: 1 key-package request, 2 key package, 3 welcome, 4 MLS message
//! ```

use const_cbor::Decoder;
use threnody_core::cbor::{self, finish, read_map, required};
use threnody_core::{Error, Result};

/// Group ids are 16 random bytes.
pub type GroupId = [u8; 16];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupWire {
    /// "Send me a key package so I can add you to this group."
    KeyPackageRequest { group: GroupId, name: String },
    KeyPackage {
        group: GroupId,
        key_package: Vec<u8>,
    },
    Welcome {
        group: GroupId,
        name: String,
        welcome: Vec<u8>,
    },
    /// A serialized `MlsMessageOut` (application message or commit).
    Message { group: GroupId, message: Vec<u8> },
}

impl GroupWire {
    pub fn group(&self) -> &GroupId {
        match self {
            Self::KeyPackageRequest { group, .. }
            | Self::KeyPackage { group, .. }
            | Self::Welcome { group, .. }
            | Self::Message { group, .. } => group,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let (kind, payload, name): (u8, Option<&[u8]>, Option<&str>) = match self {
            Self::KeyPackageRequest { name, .. } => (1, None, Some(name)),
            Self::KeyPackage { key_package, .. } => (2, Some(key_package), None),
            Self::Welcome { welcome, name, .. } => (3, Some(welcome), Some(name)),
            Self::Message { message, .. } => (4, Some(message), None),
        };
        let size = payload.map_or(0, <[u8]>::len) + name.map_or(0, str::len) + 48;
        cbor::to_vec(size, |e| {
            e.map_len(2 + usize::from(payload.is_some()) + usize::from(name.is_some()))?;
            e.u8(0)?.u8(kind)?;
            e.u8(1)?.bytes(self.group())?;
            if let Some(p) = payload {
                e.u8(2)?.bytes(p)?;
            }
            if let Some(n) = name {
                e.u8(3)?.str(n)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut kind, mut group, mut payload, mut name) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => group = Some(cbor::fixed_bytes::<16>(d)?),
                2 => payload = Some(d.bytes()?.to_vec()),
                3 => name = Some(d.str()?.to_owned()),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let group = required(group, "group id")?;
        Ok(match required(kind, "group message kind")? {
            1 => Self::KeyPackageRequest {
                group,
                name: name.unwrap_or_default(),
            },
            2 => Self::KeyPackage {
                group,
                key_package: required(payload, "key package")?,
            },
            3 => Self::Welcome {
                group,
                name: name.unwrap_or_default(),
                welcome: required(payload, "welcome")?,
            },
            4 => Self::Message {
                group,
                message: required(payload, "mls message")?,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let g = [7u8; 16];
        for m in [
            GroupWire::KeyPackageRequest {
                group: g,
                name: "team".into(),
            },
            GroupWire::KeyPackage {
                group: g,
                key_package: vec![1, 2],
            },
            GroupWire::Welcome {
                group: g,
                name: "team".into(),
                welcome: vec![3],
            },
            GroupWire::Message {
                group: g,
                message: vec![4, 5, 6],
            },
        ] {
            assert_eq!(GroupWire::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(GroupWire::decode(&[0xa0]).is_err());
    }
}
