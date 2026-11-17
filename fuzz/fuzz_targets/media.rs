#![no_main]
//! Metadata removal from photos and videos picked to send: JPEG, PNG,
//! WebP, GIF and ISO base media files, which may be anything a file
//! picker hands over.
use libfuzzer_sys::fuzz_target;
use threnody_core::media::{self, Kind, Stripped};

fuzz_target!(|data: &[u8]| {
    let kind = media::kind(data);
    if let Ok(Stripped::Image(out)) = media::strip(data.to_vec()) {
        // Video boxes are blanked where they are, so nothing moves.
        if kind == Some(Kind::Bmff) {
            assert_eq!(out.len(), data.len());
        }
    }
    let _ = media::as_sent(data.to_vec(), data.len() as u64);
    let _ = media::neutral_name("x", data);
});
