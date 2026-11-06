//! What our own devices tell each other about files and ticks
//! (`docs/appendix-j-devices.md`, *Files* and *Ticks*). Codecs only; the
//! logic is in `threnody-net`.
//!
//! ```text
//! FileRef = { 0: author device (32), 1: message id uint (0 = none),
//!             2: name tstr, 3: size uint, 4: at_ms uint }
//! Want    = { 0: request uint, 1: FileRef, 2: offset uint, ? 3: content hash (32) }
//! Part    = { 0: request uint, 1: offset uint, 2: total uint,
//!             3: content hash (32), 4: data bstr (<= 262144) }
//! Missing = { 0: request uint }
//! Ticks   = { 0: 1, 1: [* Tick] }                      ; at most 128
//! Tick    = { 0: message id uint, 1: author device (32), ? 2: true (delivered),
//!             ? 3: read_ms uint, ? 4: recipients uint,
//!             ? 5: bstr (n x 32 delivered devices, n <= 1024) }
//! ```
//!
//! The content hash is BLAKE3 over the whole file as it was sent. Unknown
//! keys are skipped everywhere, so every map can grow.

use const_cbor::Decoder;

use crate::cbor::{self, finish, fixed_bytes, read_map, required};
use crate::error::{Error, Result};
use crate::message::MAX_FILE;

/// Bytes per [`FilePart`].
pub const PART: usize = 256 * 1024;
/// Parts a source answers one [`FileWant`] with.
pub const WINDOW: usize = 4;
/// Devices kept in one [`Tick`]'s `delivered_to`.
pub const MAX_TICK_DEVICES: usize = 1024;
/// Ticks in one [`Ticks`] message.
pub const TICKS_PER_MESSAGE: usize = 128;
/// Longest file name a [`FileRef`] carries, in bytes.
pub const MAX_NAME: usize = 255;
const TICKS_VERSION: u64 = 1;

/// Names a file in history, as both devices recorded it: by its author
/// device and the sender's id for the message, or (for messages without
/// one) by name, size and time.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileRef {
    pub device: [u8; 32],
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub at_ms: u64,
}

/// Asks one of our devices for a file's contents from `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWant {
    /// Random; the parts and a `FileMissing` answering it carry it back.
    pub request: u64,
    pub file: FileRef,
    pub offset: u64,
    /// The content hash the requester already has parts of, if any.
    pub hash: Option<[u8; 32]>,
}

/// Up to [`PART`] bytes of a file, from `offset`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePart {
    pub request: u64,
    pub offset: u64,
    pub total: u64,
    pub hash: [u8; 32],
    pub data: Vec<u8>,
}

/// The device asked doesn't have the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMissing {
    pub request: u64,
}

/// What one device knows of an outgoing message's ticks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The sender's id for the message.
    pub id: u64,
    /// The device that sent it.
    pub device: [u8; 32],
    pub delivered: bool,
    pub read_ms: u64,
    /// For a group message: how many members it went to, and the devices
    /// that acknowledged it.
    pub recipients: u32,
    pub delivered_to: Vec<[u8; 32]>,
}

/// Ticks for one conversation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ticks(pub Vec<Tick>);

/// `s` cut to at most `max` bytes, at a char boundary.
fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

impl FileRef {
    fn write(
        &self,
        e: &mut const_cbor::Encoder<'_>,
    ) -> core::result::Result<(), const_cbor::Error> {
        e.map_len(5)?;
        e.u8(0)?.bytes(&self.device)?;
        e.u8(1)?.u64(self.id)?;
        e.u8(2)?.str(&self.name)?;
        e.u8(3)?.u64(self.size)?;
        e.u8(4)?.u64(self.at_ms)?;
        Ok(())
    }

    fn read(d: &mut Decoder<'_>) -> Result<Self> {
        let (mut device, mut id, mut name, mut size, mut at_ms) = (None, None, None, None, None);
        read_map(d, |k, d| {
            match k {
                0 => device = Some(fixed_bytes::<32>(d)?),
                1 => id = Some(d.u64()?),
                2 => name = Some(truncate(d.str()?.to_owned(), MAX_NAME)),
                3 => size = Some(d.u64()?),
                4 => at_ms = Some(d.u64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        Ok(Self {
            device: required(device, "file device")?,
            id: required(id, "file id")?,
            name: required(name, "file name")?,
            size: required(size, "file size")?,
            at_ms: required(at_ms, "file time")?,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(self.name.len() + 96, |e| self.write(e))
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let r = Self::read(&mut dec)?;
        finish(&dec)?;
        Ok(r)
    }
}

impl FileWant {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(self.file.name.len() + 160, |e| {
            e.map_len(3 + usize::from(self.hash.is_some()))?;
            e.u8(0)?.u64(self.request)?;
            e.u8(1)?;
            self.file.write(e)?;
            e.u8(2)?.u64(self.offset)?;
            if let Some(h) = &self.hash {
                e.u8(3)?.bytes(h)?;
            }
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut request, mut file, mut offset, mut hash) = (None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => request = Some(d.u64()?),
                1 => file = Some(FileRef::read(d)?),
                2 => offset = Some(d.u64()?),
                3 => hash = Some(fixed_bytes::<32>(d)?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            request: required(request, "request")?,
            file: required(file, "file")?,
            offset: required(offset, "offset")?,
            hash,
        })
    }
}

impl FilePart {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(self.data.len() + 96, |e| {
            e.map_len(5)?;
            e.u8(0)?.u64(self.request)?;
            e.u8(1)?.u64(self.offset)?;
            e.u8(2)?.u64(self.total)?;
            e.u8(3)?.bytes(&self.hash)?;
            e.u8(4)?.bytes(&self.data)?;
            Ok(())
        })
    }

    /// Decodes a part. Data over [`PART`], a total over `MAX_FILE`, or
    /// data reaching past the total are malformed.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut request, mut offset, mut total, mut hash, mut data) =
            (None, None, None, None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => request = Some(d.u64()?),
                1 => offset = Some(d.u64()?),
                2 => total = Some(d.u64()?),
                3 => hash = Some(fixed_bytes::<32>(d)?),
                4 => {
                    let b = d.bytes()?;
                    if b.len() > PART {
                        return Err(Error::Malformed("file part too large"));
                    }
                    data = Some(b.to_vec());
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        let p = Self {
            request: required(request, "request")?,
            offset: required(offset, "offset")?,
            total: required(total, "total")?,
            hash: required(hash, "hash")?,
            data: required(data, "data")?,
        };
        if p.total > MAX_FILE as u64 {
            return Err(Error::Malformed("file too large"));
        }
        if p.offset
            .checked_add(p.data.len() as u64)
            .is_none_or(|end| end > p.total)
        {
            return Err(Error::Malformed("file part past its end"));
        }
        Ok(p)
    }
}

impl FileMissing {
    pub fn encode(&self) -> Result<Vec<u8>> {
        cbor::to_vec(16, |e| {
            e.map_len(1)?.u8(0)?.u64(self.request)?;
            Ok(())
        })
    }

    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let mut request = None;
        read_map(&mut dec, |k, d| {
            match k {
                0 => request = Some(d.u64()?),
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        Ok(Self {
            request: required(request, "request")?,
        })
    }
}

impl Tick {
    fn write(
        &self,
        e: &mut const_cbor::Encoder<'_>,
    ) -> core::result::Result<(), const_cbor::Error> {
        let devices = &self.delivered_to[..self.delivered_to.len().min(MAX_TICK_DEVICES)];
        e.map_len(
            2 + usize::from(self.delivered)
                + usize::from(self.read_ms != 0)
                + usize::from(self.recipients != 0)
                + usize::from(!devices.is_empty()),
        )?;
        e.u8(0)?.u64(self.id)?;
        e.u8(1)?.bytes(&self.device)?;
        if self.delivered {
            e.u8(2)?.bool(true)?;
        }
        if self.read_ms != 0 {
            e.u8(3)?.u64(self.read_ms)?;
        }
        if self.recipients != 0 {
            e.u8(4)?.u32(self.recipients)?;
        }
        if !devices.is_empty() {
            e.u8(5)?.bytes(&devices.concat())?;
        }
        Ok(())
    }

    fn read(d: &mut Decoder<'_>) -> Result<Self> {
        let (mut id, mut device) = (None, None);
        let mut t = Self::default();
        read_map(d, |k, d| {
            match k {
                0 => id = Some(d.u64()?),
                1 => device = Some(fixed_bytes::<32>(d)?),
                2 => t.delivered = d.bool()?,
                3 => t.read_ms = d.u64()?,
                4 => t.recipients = d.u32()?,
                5 => {
                    let b = d.bytes()?;
                    if !b.len().is_multiple_of(32) {
                        return Err(Error::Malformed("delivered devices"));
                    }
                    let all = b.as_chunks::<32>().0;
                    t.delivered_to = all[..all.len().min(MAX_TICK_DEVICES)].to_vec();
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        t.id = required(id, "tick id")?;
        t.device = required(device, "tick device")?;
        Ok(t)
    }
}

impl Ticks {
    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.0.len() > TICKS_PER_MESSAGE {
            return Err(Error::Malformed("too many ticks"));
        }
        let size: usize = self
            .0
            .iter()
            .map(|t| 80 + 32 * t.delivered_to.len().min(MAX_TICK_DEVICES))
            .sum();
        cbor::to_vec(size + 16, |e| {
            e.map_len(2)?.u8(0)?.u64(TICKS_VERSION)?;
            e.u8(1)?.array_len(self.0.len())?;
            for t in &self.0 {
                t.write(e)?;
            }
            Ok(())
        })
    }

    /// Decodes ticks; more than [`TICKS_PER_MESSAGE`] are malformed.
    pub fn decode(b: &[u8]) -> Result<Self> {
        let mut dec = Decoder::new(b);
        let (mut ver, mut ticks) = (None, None);
        read_map(&mut dec, |k, d| {
            match k {
                0 => ver = Some(d.u64()?),
                1 => {
                    let n = d.array_len()?;
                    if n > TICKS_PER_MESSAGE {
                        return Err(Error::Malformed("too many ticks"));
                    }
                    let mut v = Vec::with_capacity(n);
                    for _ in 0..n {
                        v.push(Tick::read(d)?);
                    }
                    ticks = Some(v);
                }
                _ => return Ok(false),
            }
            Ok(true)
        })?;
        finish(&dec)?;
        if ver != Some(TICKS_VERSION) {
            return Err(Error::Malformed("ticks version"));
        }
        Ok(Self(required(ticks, "ticks")?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> FileRef {
        FileRef {
            device: [3; 32],
            id: 77,
            name: "IMG_1.png".into(),
            size: 1234,
            at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn file_ref_round_trips_and_skips_unknown_keys() {
        let r = file();
        assert_eq!(FileRef::decode(&r.encode().unwrap()).unwrap(), r);
        // The same map with an extra key 9.
        let extra = cbor::to_vec(256, |e| {
            e.map_len(6)?;
            e.u8(0)?.bytes(&r.device)?;
            e.u8(1)?.u64(r.id)?;
            e.u8(2)?.str(&r.name)?;
            e.u8(9)?.str("future")?;
            e.u8(3)?.u64(r.size)?;
            e.u8(4)?.u64(r.at_ms)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(FileRef::decode(&extra).unwrap(), r);
    }

    #[test]
    fn want_part_missing_round_trip() {
        for hash in [None, Some([5; 32])] {
            let w = FileWant {
                request: 9,
                file: file(),
                offset: 4 * PART as u64,
                hash,
            };
            assert_eq!(FileWant::decode(&w.encode().unwrap()).unwrap(), w);
        }
        for data in [Vec::new(), vec![1, 2, 3], vec![7; PART]] {
            let p = FilePart {
                request: 9,
                offset: 0,
                total: data.len() as u64,
                hash: [1; 32],
                data,
            };
            assert_eq!(FilePart::decode(&p.encode().unwrap()).unwrap(), p);
        }
        let m = FileMissing { request: u64::MAX };
        assert_eq!(FileMissing::decode(&m.encode().unwrap()).unwrap(), m);
    }

    #[test]
    fn part_limits_are_enforced() {
        let ok = FilePart {
            request: 1,
            offset: 10,
            total: 20,
            hash: [0; 32],
            data: vec![0; 10],
        };
        assert!(FilePart::decode(&ok.encode().unwrap()).is_ok());
        let too_long = FilePart {
            total: MAX_FILE as u64,
            data: vec![0; PART + 1],
            ..ok.clone()
        };
        assert!(FilePart::decode(&too_long.encode().unwrap()).is_err());
        let too_big = FilePart {
            total: MAX_FILE as u64 + 1,
            ..ok.clone()
        };
        assert!(FilePart::decode(&too_big.encode().unwrap()).is_err());
        let past_end = FilePart {
            offset: 11,
            ..ok.clone()
        };
        assert!(FilePart::decode(&past_end.encode().unwrap()).is_err());
        let overflow = FilePart {
            offset: u64::MAX,
            ..ok.clone()
        };
        assert!(FilePart::decode(&overflow.encode().unwrap()).is_err());
        let short_hash = cbor::to_vec(128, |e| {
            e.map_len(5)?;
            e.u8(0)?.u64(1)?;
            e.u8(1)?.u64(0)?;
            e.u8(2)?.u64(1)?;
            e.u8(3)?.bytes(&[0; 31])?;
            e.u8(4)?.bytes(&[0])?;
            Ok(())
        })
        .unwrap();
        assert!(FilePart::decode(&short_hash).is_err());
    }

    #[test]
    fn ticks_round_trip_and_cap_devices() {
        let t = Tick {
            id: 5,
            device: [2; 32],
            delivered: true,
            read_ms: 99,
            recipients: 3,
            delivered_to: vec![[4; 32], [6; 32]],
        };
        let plain = Tick {
            id: 6,
            device: [2; 32],
            ..Tick::default()
        };
        let ts = Ticks(vec![t.clone(), plain]);
        assert_eq!(Ticks::decode(&ts.encode().unwrap()).unwrap(), ts);

        // 1500 devices on the wire come back as 1024.
        let wide = |devices: &[u8]| {
            cbor::to_vec(devices.len() + 128, |e| {
                e.map_len(2)?.u8(0)?.u64(1)?;
                e.u8(1)?.array_len(1)?;
                e.map_len(3)?;
                e.u8(0)?.u64(1)?;
                e.u8(1)?.bytes(&[2; 32])?;
                e.u8(5)?.bytes(devices)?;
                Ok(())
            })
            .unwrap()
        };
        let many = Ticks::decode(&wide(&vec![9; 32 * 1500])).unwrap();
        assert_eq!(many.0[0].delivered_to.len(), MAX_TICK_DEVICES);
        assert!(Ticks::decode(&wide(&[9; 33])).is_err());

        let over = Ticks(vec![t; TICKS_PER_MESSAGE + 1]);
        assert!(over.encode().is_err());
        let over = cbor::to_vec(1 << 20, |e| {
            e.map_len(2)?.u8(0)?.u64(1)?;
            e.u8(1)?.array_len(TICKS_PER_MESSAGE + 1)?;
            for _ in 0..=TICKS_PER_MESSAGE {
                e.map_len(2)?.u8(0)?.u64(1)?.u8(1)?.bytes(&[2; 32])?;
            }
            Ok(())
        })
        .unwrap();
        assert!(Ticks::decode(&over).is_err());

        // A 300-byte name is cut to 255 bytes at a char boundary.
        let long = FileRef {
            name: "é".repeat(150),
            ..file()
        };
        let back = FileRef::decode(&long.encode().unwrap()).unwrap();
        assert_eq!(back.name.len(), 254);
        assert!(back.name.chars().all(|c| c == 'é'));
    }
}
