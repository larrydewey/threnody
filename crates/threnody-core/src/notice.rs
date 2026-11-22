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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Where a message was until it was deleted for everyone: by its
    /// sender, or (on our own messages) by us.
    Deleted,
}

impl Notice {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(16, |e| {
            match self {
                Self::Deleted => {
                    e.map_len(1)?.u8(0)?.uint(KIND_DELETED)?;
                }
            }
            Ok(())
        })
    }

    /// The notice in `b`; `None` for a kind this version doesn't know.
    pub fn decode(b: &[u8]) -> Result<Option<Self>> {
        let mut dec = const_cbor::Decoder::new(b);
        let mut kind = None;
        cbor::read_map(&mut dec, |k, d| {
            match k {
                0 => kind = Some(d.u64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        cbor::finish(&dec)?;
        Ok(match kind.ok_or(Error::Malformed("notice kind"))? {
            KIND_DELETED => Some(Self::Deleted),
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
}
