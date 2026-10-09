//! Recording voice and video messages. GStreamer reads the microphone
//! (and for video, the camera) and writes straight to a file in the
//! identity's private media folder: Opus in Ogg for voice, VP8 and Opus in
//! WebM for video, which both apps play. A video recording also hands
//! over small mirrored RGBA frames for a preview. The microphone and
//! camera stop when the [`Recorder`] finishes or is dropped, and a
//! recording that isn't finished is deleted.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::camera;

/// Longest voice message: well under the file size limit at Opus's 32 kb/s.
pub const MAX_VOICE: Duration = Duration::from_secs(10 * 60);
/// Longest video message, so it fits in one file (8 MB).
pub const MAX_VIDEO: Duration = Duration::from_secs(60);

/// What a video message is recorded at, and the preview's size.
const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const PREVIEW_WIDTH: u32 = 320;
const PREVIEW_HEIGHT: u32 = 240;

/// `(width, height, rgba)` of one preview frame.
pub type OnFrame = Box<dyn Fn(u32, u32, &[u8]) + Send + Sync>;

pub struct Recorder {
    pipeline: gst::Pipeline,
    path: PathBuf,
    started: Instant,
    video: bool,
    done: bool,
}

/// A finished recording.
pub struct Recorded {
    pub path: PathBuf,
    pub duration_ms: u32,
}

impl Recorder {
    /// Starts recording to `path`: sound alone, or with `video` camera
    /// number `camera` too, whose frames `preview` gets.
    pub fn start(
        path: &Path,
        video: bool,
        camera: usize,
        preview: Option<OnFrame>,
    ) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let audio = "autoaudiosrc ! queue ! audioconvert ! audioresample \
                     ! audio/x-raw,channels=1,rate=48000";
        let desc = if video {
            format!(
                "{audio} ! opusenc bitrate=48000 ! queue ! mux. \
                 videoconvert name=head ! videoscale ! videorate \
                 ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=30/1 ! tee name=t \
                 t. ! queue ! videoconvert \
                   ! vp8enc deadline=1 cpu-used=8 target-bitrate=700000 keyframe-max-dist=60 \
                   ! queue ! mux. \
                 t. ! queue leaky=downstream max-size-buffers=1 ! videoflip method=horizontal-flip \
                   ! videoscale ! videoconvert \
                   ! video/x-raw,format=RGBA,width={PREVIEW_WIDTH},height={PREVIEW_HEIGHT} \
                   ! appsink name=preview sync=false max-buffers=1 drop=true \
                 webmmux name=mux ! filesink name=out"
            )
        } else {
            format!("{audio} ! opusenc bitrate=32000 ! oggmux ! filesink name=out")
        };
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_owned())?;
        pipeline
            .by_name("out")
            .ok_or("no file sink")?
            .set_property("location", path.to_str().ok_or("unusable file name")?);
        if video {
            let source = camera::source(camera)?;
            let head = pipeline.by_name("head").ok_or("no head")?;
            pipeline.add(&source).map_err(|e| e.to_string())?;
            source.link(&head).map_err(|e| e.to_string())?;
            let sink = pipeline
                .by_name("preview")
                .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
                .ok_or("no appsink")?;
            let deliver = preview.unwrap_or_else(|| Box::new(|_, _, _| {}));
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(buf) = sample.buffer()
                            && let Ok(map) = buf.map_readable()
                        {
                            deliver(PREVIEW_WIDTH, PREVIEW_HEIGHT, map.as_slice());
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        let rec = Self {
            pipeline,
            path: path.to_owned(),
            started: Instant::now(),
            video,
            done: false,
        };
        rec.pipeline.set_state(gst::State::Playing).map_err(|_| {
            if video {
                "the camera or microphone didn't start (is another app using it?)".to_owned()
            } else {
                "the microphone didn't start".to_owned()
            }
        })?;
        Ok(rec)
    }

    /// How long it has been recording.
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The longest it may record.
    pub fn limit(&self) -> Duration {
        if self.video { MAX_VIDEO } else { MAX_VOICE }
    }

    /// Stops and closes the file properly (blocking briefly while the
    /// encoders drain).
    pub fn finish(mut self) -> Result<Recorded, String> {
        let duration = self.elapsed().min(self.limit());
        self.pipeline.send_event(gst::event::Eos::new());
        let bus = self.pipeline.bus().ok_or("no bus")?;
        let msg = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(5),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        );
        let _ = self.pipeline.set_state(gst::State::Null);
        match msg.as_ref().map(|m| m.view()) {
            Some(gst::MessageView::Eos(_)) => {}
            Some(gst::MessageView::Error(e)) => return Err(e.error().to_string()),
            _ => return Err("the recording didn't finish".into()),
        }
        self.done = true;
        Ok(Recorded {
            path: self.path.clone(),
            duration_ms: u32::try_from(duration.as_millis()).unwrap_or(u32::MAX),
        })
    }

    /// An error GStreamer reported since the recording started, if any.
    pub fn error(&self) -> Option<String> {
        let bus = self.pipeline.bus()?;
        let msg = bus.pop_filtered(&[gst::MessageType::Error])?;
        match msg.view() {
            gst::MessageView::Error(e) => Some(e.error().to_string()),
            _ => None,
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if !self.done {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// `m:ss` for a duration in milliseconds.
pub fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_reads_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(4_999), "0:04");
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(600_000), "10:00");
    }

    /// Records two seconds from this machine's microphone; nothing is
    /// kept. Run by hand: `cargo test -p threnody-desktop -- --ignored
    /// voice`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_a_voice_message() {
        let dir = std::env::temp_dir().join(format!("thr-clip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voice.ogg");
        let rec = Recorder::start(&path, false, 0, None).expect("microphone");
        std::thread::sleep(Duration::from_secs(2));
        let done = rec.finish().expect("finished");
        let data = std::fs::read(&done.path).unwrap();
        assert!(data.starts_with(b"OggS"), "an Ogg file");
        assert!(done.duration_ms >= 1_900);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Records two seconds of video and sound; nothing is kept. Run by
    /// hand: `cargo test -p threnody-desktop -- --ignored video`.
    #[test]
    #[ignore = "needs a camera and microphone"]
    fn records_a_video_message() {
        let dir = std::env::temp_dir().join(format!("thr-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video.webm");
        let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f = frames.clone();
        let rec = Recorder::start(
            &path,
            true,
            0,
            Some(Box::new(move |w, h, rgba| {
                assert_eq!(rgba.len(), (w * h * 4) as usize);
                f.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            })),
        )
        .expect("camera");
        std::thread::sleep(Duration::from_secs(2));
        let done = rec.finish().expect("finished");
        let data = std::fs::read(&done.path).unwrap();
        assert!(data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]), "a WebM file");
        assert!(frames.load(std::sync::atomic::Ordering::Relaxed) >= 20);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
