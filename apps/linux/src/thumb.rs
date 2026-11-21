//! Thumbnails for files offered rather than sent (Appendix U): a small
//! JPEG of a photo or of a video's first frame, shown in the contact's
//! chat before they download the file.

use std::path::Path;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gtk::gdk::gdk_pixbuf::{Colorspace, InterpType, Pixbuf};
use gtk::{gio, glib};

/// Longest side of a thumbnail, in pixels (as Android makes them).
const SIDE: i32 = 320;
/// How long a video may take to give its first frame.
const FRAME_WAIT: u64 = 5;

const VIDEO: &[&str] = &["mp4", "m4v", "mov", "3gp", "webm", "mkv"];

pub fn is_video(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, e)| VIDEO.iter().any(|v| e.eq_ignore_ascii_case(v)))
}

/// A JPEG of the photo or video at `path` within what an offer carries;
/// None if it is neither, or can't be made that small. Encoded afresh
/// from pixels, so none of the original's metadata comes with it (the
/// node strips it again all the same). Blocking: run it off the main
/// thread.
pub fn offer_thumbnail(path: &Path, name: &str) -> Option<Vec<u8>> {
    let pixbuf = if crate::core::is_image(name) {
        let (w, h) = crate::viewer::checked_size(path).ok()?;
        Pixbuf::from_file_at_scale(path, SIDE.min(w), SIDE.min(h), true)
            .ok()?
            .apply_embedded_orientation()?
    } else if is_video(name) {
        first_frame(path)?
    } else {
        return None;
    };
    let limit = threnody_ffi::max_offer_thumbnail() as usize;
    ["70", "55", "40", "30"].into_iter().find_map(|quality| {
        pixbuf
            .save_to_bufferv("jpeg", &[("quality", quality)])
            .ok()
            .filter(|j| j.len() <= limit)
    })
}

/// The first frame of the video at `path`, at most [`SIDE`] on its long
/// side. Only the video stream is decoded (any sound is left alone), and
/// only up to that frame.
fn first_frame(path: &Path) -> Option<Pixbuf> {
    gst::init().ok()?;
    let uri = gio::prelude::FileExt::uri(&gio::File::for_path(path));
    let convert = gst::ElementFactory::make("videoconvert").build().ok()?;
    let sink = gst_app::AppSink::builder()
        .caps(
            &gst::Caps::builder("video/x-raw")
                .field("format", "RGB")
                .build(),
        )
        .max_buffers(1)
        .build();
    let video = gst::Bin::new();
    video.add_many([&convert, sink.upcast_ref()]).ok()?;
    convert.link(&sink).ok()?;
    let head = gst::GhostPad::with_target(&convert.static_pad("sink")?).ok()?;
    video.add_pad(&head).ok()?;
    let player = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .property("video-sink", &video)
        .build()
        .ok()?;
    // Video only: a sound track is never decoded, nor left unlinked.
    player.set_property_from_str("flags", "video");
    let frame = (|| {
        player.set_state(gst::State::Paused).ok()?;
        let sample = sink.try_pull_preroll(gst::ClockTime::from_seconds(FRAME_WAIT))?;
        let s = sample.caps()?.structure(0)?;
        let (w, h) = (s.get::<i32>("width").ok()?, s.get::<i32>("height").ok()?);
        let map = sample.buffer()?.map_readable().ok()?;
        // GStreamer's RGB rows are padded to 4 bytes.
        let stride = (w as usize * 3).next_multiple_of(4);
        if w <= 0 || h <= 0 || map.len() < stride * h as usize {
            return None;
        }
        let full = Pixbuf::from_bytes(
            &glib::Bytes::from(map.as_slice()),
            Colorspace::Rgb,
            false,
            8,
            w,
            h,
            stride as i32,
        );
        let scale = f64::from(SIDE) / f64::from(w.max(h));
        if scale >= 1.0 {
            return Some(full);
        }
        full.scale_simple(
            ((f64::from(w) * scale) as i32).max(1),
            ((f64::from(h) * scale) as i32).max(1),
            InterpType::Bilinear,
        )
    })();
    let _ = player.set_state(gst::State::Null);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test, removed when it ends.
    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(what: &str) -> Self {
            let d =
                std::env::temp_dir().join(format!("threnody-thumb-{what}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn photos_become_small_jpegs_and_other_files_none() {
        let dir = Dir::new("photo");
        let png = dir.path().join("big.png");
        let p = Pixbuf::new(Colorspace::Rgb, false, 8, 2000, 1000).unwrap();
        p.fill(0x3366_99ff);
        p.savev(&png, "png", &[]).unwrap();
        let j = offer_thumbnail(&png, "big.png").unwrap();
        assert!(j.starts_with(&[0xFF, 0xD8, 0xFF]));
        assert!(j.len() <= threnody_ffi::max_offer_thumbnail() as usize);
        let back = Pixbuf::from_read(std::io::Cursor::new(j)).unwrap();
        assert_eq!((back.width(), back.height()), (320, 160));

        let bin = dir.path().join("data.bin");
        std::fs::write(&bin, [7u8; 100]).unwrap();
        assert_eq!(offer_thumbnail(&bin, "data.bin"), None);
        // A "video" that isn't one gives up, rather than hanging.
        let fake = dir.path().join("fake.mp4");
        std::fs::write(&fake, [7u8; 100]).unwrap();
        assert_eq!(offer_thumbnail(&fake, "fake.mp4"), None);
    }

    #[test]
    fn a_video_with_sound_gives_its_first_frame() {
        gst::init().unwrap();
        let dir = Dir::new("video");
        let out = dir.path().join("clip.webm");
        // Skipped where the encoders aren't installed.
        let Ok(p) = gst::parse::launch(&format!(
            "videotestsrc num-buffers=10 ! video/x-raw,width=640,height=480 \
             ! vp8enc ! webmmux name=mux ! filesink location={} \
             audiotestsrc num-buffers=10 ! audioconvert ! opusenc ! mux.",
            out.display()
        )) else {
            return;
        };
        p.set_state(gst::State::Playing).unwrap();
        let bus = p.bus().unwrap();
        bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        p.set_state(gst::State::Null).unwrap();
        let j = offer_thumbnail(&out, "clip.webm").unwrap();
        let back = Pixbuf::from_read(std::io::Cursor::new(j)).unwrap();
        assert_eq!((back.width(), back.height()), (320, 240));
    }
}
