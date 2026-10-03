#![no_main]
use libfuzzer_sys::fuzz_target;
use threnody_core::AppMessage;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = AppMessage::decode(data) {
        // Anything we accept must re-encode and decode to the same value.
        let again = AppMessage::decode(&m.encode().unwrap()).unwrap();
        assert_eq!(m, again);
    }
    let _ = threnody_core::message::unpad(data.to_vec());
});
