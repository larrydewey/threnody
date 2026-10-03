//! Thin helpers over `const-cbor` for growable output and typed map decoding.
//!
//! `const-cbor` encodes into caller-provided fixed buffers. Protocol messages
//! have well-known upper bounds, so [`to_vec`] starts from an estimate and
//! retries with a larger buffer only if the estimate was too small.

use const_cbor::{Decoder, Encoder};

use crate::error::{Error, Result};

/// Largest single encoded message the core will produce.
pub const MAX_ENCODED: usize = 16 * 1024 * 1024;

/// Encodes with `f` into a fresh `Vec`, growing the scratch buffer on
/// `OUT_OF_SPACE` until it fits or [`MAX_ENCODED`] is reached.
pub fn to_vec(
    estimate: usize,
    f: impl Fn(&mut Encoder<'_>) -> core::result::Result<(), const_cbor::Error>,
) -> Result<Vec<u8>> {
    let mut cap = estimate.clamp(64, MAX_ENCODED);
    loop {
        let mut buf = vec![0u8; cap];
        let mut enc = Encoder::new(&mut buf);
        match f(&mut enc) {
            Ok(()) => {
                let n = enc.written();
                buf.truncate(n);
                return Ok(buf);
            }
            Err(const_cbor::Error::OUT_OF_SPACE) if cap < MAX_ENCODED => {
                cap = (cap * 2).min(MAX_ENCODED);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Decodes a fixed-size byte string.
pub fn fixed_bytes<const N: usize>(dec: &mut Decoder<'_>) -> Result<[u8; N]> {
    dec.bytes()?
        .try_into()
        .map_err(|_| Error::Malformed("byte string has wrong length"))
}

/// Iterates the pairs of a CBOR map whose keys are unsigned integers,
/// handing each key to `field`. `field` returns `false` for keys it does not
/// recognise; their values are skipped (forward compatibility, spec §6.3).
pub fn read_map<'a>(
    dec: &mut Decoder<'a>,
    mut field: impl FnMut(u64, &mut Decoder<'a>) -> Result<bool>,
) -> Result<()> {
    let pairs = dec.map_len()?;
    let mut seen: u128 = 0;
    for _ in 0..pairs {
        let key = dec.u64()?;
        if key < 128 {
            let bit = 1u128 << key;
            if seen & bit != 0 {
                return Err(Error::Malformed("duplicate map key"));
            }
            seen |= bit;
        }
        if !field(key, dec)? {
            dec.skip()?;
        }
    }
    Ok(())
}

/// Fails unless `dec` consumed its whole input.
pub fn finish(dec: &Decoder<'_>) -> Result<()> {
    if dec.is_empty() {
        Ok(())
    } else {
        Err(Error::Malformed("trailing bytes"))
    }
}

/// Unwraps a field that a decoder loop should have filled.
pub fn required<T>(v: Option<T>, what: &'static str) -> Result<T> {
    v.ok_or(Error::Malformed(what))
}
