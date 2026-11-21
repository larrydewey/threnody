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
            // Edits applied range by range, as a large video is read,
            // give the same bytes.
            let mut r = std::io::Cursor::new(data);
            let edits = media::video_edits(&mut r, data.len() as u64).unwrap();
            let step = 1 + data.first().copied().unwrap_or(0) as usize;
            for (i, part) in data.chunks(step).enumerate() {
                let mut p = part.to_vec();
                edits.apply((i * step) as u64, &mut p);
                assert_eq!(p, &out[i * step..i * step + p.len()]);
            }
        }
    }
    let _ = media::as_sent(data.to_vec(), data.len() as u64);
    let _ = media::neutral_name("x", data);
});
