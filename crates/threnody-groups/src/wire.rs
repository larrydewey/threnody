//! Group control messages, carried in `AppMessage::Group` over the 1:1
//! ratchets (so MLS traffic is itself end-to-end encrypted per hop).
//!
//! ```text
//! GroupWire = { 0: kind uint, 1: group_id bstr, ? 2: payload bstr, ? 3: name tstr,
//!               ? 4: member bstr .size 32, ? 5: reference uint }
//! kind: 1 key-package request, 2 key package, 3 welcome, 4 MLS message,
//!       5 forward (an MLS message for another member),
//!       6 receipt (that member acknowledged a forwarded message),
//!       7 leave (a member asks the owner to remove it)
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
    /// "Deliver this MLS message to member `to` for me": sent to a member
    /// the sender can reach when it can't reach `to` itself.
    /// `reference` (if not 0) asks the forwarder to send a `Receipt`
    /// once `to` acknowledges.
    Forward {
        group: GroupId,
        to: [u8; 32],
        message: Vec<u8>,
        reference: u64,
    },
    /// From a forwarder: `member` acknowledged the message we forwarded
    /// with `reference`.
    Receipt {
        group: GroupId,
        member: [u8; 32],
        reference: u64,
    },
    /// To the owner: remove me (the sender) from the group.
    Leave { group: GroupId },
}

impl GroupWire {
    pub fn group(&self) -> &GroupId {
        match self {
            Self::KeyPackageRequest { group, .. }
            | Self::KeyPackage { group, .. }
            | Self::Welcome { group, .. }
            | Self::Message { group, .. }
            | Self::Forward { group, .. }
            | Self::Receipt { group, .. }
            | Self::Leave { group } => group,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        #[allow(clippy::type_complexity)]
        let (kind, payload, name, to, reference): (
            u8,
            Option<&[u8]>,
            Option<&str>,
            Option<&[u8; 32]>,
            u64,
        ) = match self {
            Self::KeyPackageRequest { name, .. } => (1, None, Some(name), None, 0),
            Self::KeyPackage { key_package, .. } => (2, Some(key_package), None, None, 0),
            Self::Welcome { welcome, name, .. } => (3, Some(welcome), Some(name), None, 0),
            Self::Message { message, .. } => (4, Some(message), None, None, 0),
            Self::Forward {
                to,
                message,
                reference,
                ..
            } => (5, Some(message), None, Some(to), *reference),
            Self::Receipt {
                member, reference, ..
            } => (6, None, None, Some(member), *reference),
            Self::Leave { .. } => (7, None, None, None, 0),
        };
        let size = payload.map_or(0, <[u8]>::len) + name.map_or(0, str::len) + 48;
        cbor::to_vec(size, |e| {
            e.map_len(
                2 + usize::from(payload.is_some())
                    + usize::from(name.is_some())
                    + usize::from(to.is_some())
                    + usize::from(reference != 0),
            )?;
            e.u8(0)?.u8(kind)?;
            e.u8(1)?.bytes(self.group())?;
            if let Some(p) = payload {
                e.u8(2)?.bytes(p)?;
            }
            if let Some(n) = name {
                e.u8(3)?.str(n)?;
            }
            if let Some(t) = to {
                e.u8(4)?.bytes(t)?;
            }
            if reference != 0 {
                e.u8(5)?.u64(reference)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut kind, mut group, mut payload, mut name, mut to) = (None, None, None, None, None);
        let mut reference = 0;
        read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => group = Some(cbor::fixed_bytes::<16>(d)?),
                2 => payload = Some(d.bytes()?.to_vec()),
                3 => name = Some(d.str()?.to_owned()),
                4 => to = Some(cbor::fixed_bytes::<32>(d)?),
                5 => reference = d.u64()?,
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
            5 => Self::Forward {
                group,
                to: required(to, "forward target")?,
                message: required(payload, "mls message")?,
                reference,
            },
            6 => Self::Receipt {
                group,
                member: required(to, "receipt member")?,
                reference,
            },
            7 => Self::Leave { group },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

/// What a member sends inside an MLS application message.
///
/// ```text
/// Content = text (UTF-8, as first sent)
///         / 0xFF || { 0: kind uint, ? 1: text or name tstr, ? 2: data bstr,
///                     ? 3: flags uint, ? 4: caption tstr, ? 5: album uint }
/// kind: 1 text, 2 file; flags: 1 sensitive
/// ```
///
/// 0xFF never starts UTF-8, so plain text stays as it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    File {
        name: String,
        data: Vec<u8>,
        sensitive: bool,
        caption: String,
        album: u64,
    },
}

const TAGGED: u8 = 0xFF;

impl Content {
    pub fn encode(&self) -> Result<Vec<u8>> {
        match self {
            Self::Text(t) => Ok(t.as_bytes().to_vec()),
            Self::File {
                name,
                data,
                sensitive,
                caption,
                album,
            } => {
                let size = name.len() + data.len() + caption.len() + 40;
                let body = cbor::to_vec(size, |e| {
                    e.map_len(
                        3 + usize::from(*sensitive)
                            + usize::from(!caption.is_empty())
                            + usize::from(*album != 0),
                    )?;
                    e.u8(0)?.u8(2)?;
                    e.u8(1)?.str(name)?;
                    e.u8(2)?.bytes(data)?;
                    if *sensitive {
                        e.u8(3)?.u8(1)?;
                    }
                    if !caption.is_empty() {
                        e.u8(4)?.str(caption)?;
                    }
                    if *album != 0 {
                        e.u8(5)?.u64(*album)?;
                    }
                    Ok(())
                })?;
                let mut out = Vec::with_capacity(body.len() + 1);
                out.push(TAGGED);
                out.extend_from_slice(&body);
                Ok(out)
            }
        }
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let Some((&TAGGED, rest)) = b.split_first() else {
            return Ok(Self::Text(String::from_utf8_lossy(b).into_owned()));
        };
        let mut dec = Decoder::new(rest);
        let (mut kind, mut text, mut data) = (None, None, None);
        let (mut flags, mut caption, mut album) = (0, String::new(), 0);
        read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u8()?),
                1 => text = Some(d.str()?.to_owned()),
                2 => data = Some(d.bytes()?.to_vec()),
                3 => flags = d.u64()?,
                4 => caption = d.str()?.to_owned(),
                5 => album = d.u64()?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(match required(kind, "content kind")? {
            1 => Self::Text(text.unwrap_or_default()),
            2 => Self::File {
                name: required(text, "file name")?,
                data: required(data, "file data")?,
                sensitive: flags & 1 != 0,
                caption,
                album,
            },
            other => return Err(Error::UnexpectedType(u64::from(other))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_round_trips_and_plain_text_still_reads() {
        for c in [
            Content::Text("hello".into()),
            Content::Text(String::new()),
            Content::File {
                name: "a.txt".into(),
                data: vec![0xFF; 3000],
                sensitive: false,
                caption: String::new(),
                album: 0,
            },
            Content::File {
                name: "IMG.jpg".into(),
                data: vec![1; 10],
                sensitive: true,
                caption: "look".into(),
                album: 5,
            },
        ] {
            assert_eq!(Content::decode(&c.encode().unwrap()).unwrap(), c);
        }
        assert_eq!(
            Content::decode("hi ✓".as_bytes()).unwrap(),
            Content::Text("hi ✓".into())
        );
        // A tagged text, as a later sender might write it.
        let tagged = [&[TAGGED][..], &[0xa2, 0, 1, 1, 0x62, b'o', b'k']].concat();
        assert_eq!(
            Content::decode(&tagged).unwrap(),
            Content::Text("ok".into())
        );
        assert!(Content::decode(&[TAGGED, 0xa1, 0, 9]).is_err());
        assert!(Content::decode(&[TAGGED]).is_err());
    }

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
            GroupWire::Forward {
                group: g,
                to: [9; 32],
                message: vec![7],
                reference: 0,
            },
            GroupWire::Forward {
                group: g,
                to: [9; 32],
                message: vec![7],
                reference: u64::MAX,
            },
            GroupWire::Receipt {
                group: g,
                member: [8; 32],
                reference: 3,
            },
            GroupWire::Leave { group: g },
        ] {
            assert_eq!(GroupWire::decode(&m.encode().unwrap()).unwrap(), m);
        }
        assert!(GroupWire::decode(&[0xa0]).is_err());
        // A forward must name its target.
        let mut no_target = GroupWire::Message {
            group: g,
            message: vec![1],
        }
        .encode()
        .unwrap();
        no_target[2] = 5; // { 0: 5, ... }
        assert!(GroupWire::decode(&no_target).is_err());
    }
}
