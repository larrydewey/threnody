#![no_main]
//! Offers and transfer messages for large files (Appendix U): from any
//! contact or group member, and pieces from any holder of a file.
use libfuzzer_sys::fuzz_target;
use threnody_core::transfer::{FileKey, Offer, TransferMsg, open_piece};

fuzz_target!(|data: &[u8]| {
    if let Ok(o) = Offer::decode(data) {
        // Anything we accept must re-encode and decode to the same value.
        assert_eq!(Offer::decode(&o.encode().unwrap()).unwrap(), o);
    }
    if let Ok(m) = TransferMsg::decode(data) {
        assert_eq!(TransferMsg::decode(&m.encode().unwrap()).unwrap(), m);
    }
    let key = FileKey::from_bytes([7; 32]);
    let size = data.len().saturating_sub(40) as u64;
    let _ = open_piece(&key, size, 0, data);
});
