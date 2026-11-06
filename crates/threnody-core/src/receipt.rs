//! End-to-end delivery and read receipts that can travel sealed through
//! mailboxes (`docs/appendix-h-offline.md`, *Receipts*), inside
//! `AppMessage::Receipt`.
//!
//! ```text
//! ReceiptMsg = { ? 0: delivered ids bstr, ? 1: read ids bstr }
//!              ; 8 bytes big-endian each, ≤ 512 per list, ≥ 1 id in all
//! ```
//!
//! The ids are the sender's history ids (the `id` its `Text` carried).
//! Read implies delivered.

use const_cbor::Decoder;

use crate::cbor::{self, finish, read_map};
use crate::error::{Error, Result};

/// Most ids one list of a receipt carries, as for `Ack` and `Read`.
pub const MAX_IDS: usize = crate::message::MAX_ACK_IDS;

/// Which of the sender's messages arrived, and which were displayed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    /// Sender-ids (the sender's history local ids) we received.
    pub delivered: Vec<u64>,
    /// Sender-ids we displayed. Read implies delivered.
    pub read: Vec<u64>,
}

fn pack(ids: &[u64]) -> Vec<u8> {
    ids.iter().flat_map(|i| i.to_be_bytes()).collect()
}

fn unpack(b: &[u8]) -> Result<Vec<u64>> {
    if !b.len().is_multiple_of(8) || b.len() / 8 > MAX_IDS {
        return Err(Error::Malformed("receipt"));
    }
    Ok(b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_be_bytes(*c))
        .collect())
}

impl Receipt {
    pub fn is_empty(&self) -> bool {
        self.delivered.is_empty() && self.read.is_empty()
    }

    /// Encodes it; an empty list's key is left out. Fails for an empty
    /// receipt or a list over [`MAX_IDS`], which the receiver would refuse.
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.is_empty() || self.delivered.len() > MAX_IDS || self.read.len() > MAX_IDS {
            return Err(Error::Malformed("receipt"));
        }
        let (d, r) = (pack(&self.delivered), pack(&self.read));
        cbor::to_vec(d.len() + r.len() + 16, |e| {
            e.map_len(usize::from(!d.is_empty()) + usize::from(!r.is_empty()))?;
            if !d.is_empty() {
                e.u8(0)?.bytes(&d)?;
            }
            if !r.is_empty() {
                e.u8(1)?.bytes(&r)?;
            }
            Ok(())
        })
    }

    /// Decodes a receipt. Unknown keys are ignored; a list whose length
    /// isn't a multiple of 8 or holds over [`MAX_IDS`] ids, or a receipt
    /// with no ids at all, is malformed.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut r = Self::default();
        read_map(&mut dec, |k, d| {
            match k {
                0 => r.delivered = unpack(d.bytes()?)?,
                1 => r.read = unpack(d.bytes()?)?,
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if r.is_empty() {
            return Err(Error::Malformed("receipt"));
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_bounds() {
        for r in [
            Receipt {
                delivered: vec![1, u64::MAX],
                read: vec![],
            },
            Receipt {
                delivered: vec![],
                read: vec![7],
            },
            Receipt {
                delivered: vec![3],
                read: vec![4, 5],
            },
            Receipt {
                delivered: (0..MAX_IDS as u64).collect(),
                read: (0..MAX_IDS as u64).collect(),
            },
        ] {
            assert_eq!(Receipt::decode(&r.encode().unwrap()).unwrap(), r);
        }
        // No ids at all.
        assert!(Receipt::default().encode().is_err());
        let empty = cbor::to_vec(8, |e| {
            e.map_len(0)?;
            Ok(())
        })
        .unwrap();
        assert!(Receipt::decode(&empty).is_err());
        // A length that isn't a multiple of 8.
        let odd = cbor::to_vec(16, |e| {
            e.map_len(1)?.u8(0)?.bytes(&[1, 2, 3])?;
            Ok(())
        })
        .unwrap();
        assert!(Receipt::decode(&odd).is_err());
        // 513 ids in either list.
        let over = pack(&(0..=MAX_IDS as u64).collect::<Vec<_>>());
        for key in [0u8, 1] {
            let b = cbor::to_vec(over.len() + 16, |e| {
                e.map_len(1)?.u8(key)?.bytes(&over)?;
                Ok(())
            })
            .unwrap();
            assert!(Receipt::decode(&b).is_err());
        }
        let too_many = Receipt {
            delivered: vec![],
            read: (0..=MAX_IDS as u64).collect(),
        };
        assert!(too_many.encode().is_err());
        // An unknown key is ignored.
        let extra = cbor::to_vec(32, |e| {
            e.map_len(2)?.u8(1)?.bytes(&9u64.to_be_bytes())?;
            e.u8(7)?.str("later")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            Receipt::decode(&extra).unwrap(),
            Receipt {
                delivered: vec![],
                read: vec![9],
            }
        );
    }
}
