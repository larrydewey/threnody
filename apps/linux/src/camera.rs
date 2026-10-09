//! The camera, for video calls. GStreamer reads it and hands over two
//! streams of frames, on its own threads: 640×480 I420 for the call, and a
//! small mirrored RGBA preview for our own window. Nothing is kept: frames
//! go to the call and the screen, and the camera stops (light off) when
//! the [`Camera`] is dropped.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// What the call is sent; WebRTC adapts it to the bandwidth.
pub const WIDTH: u32 = 640;
pub const HEIGHT: u32 = 480;
/// Our own picture, in the corner.
pub const PREVIEW_WIDTH: u32 = 320;
pub const PREVIEW_HEIGHT: u32 = 240;

pub struct Camera {
    pipeline: gst::Pipeline,
}

/// `(width, height, bytes)` of one frame.
type OnFrame = Box<dyn Fn(u32, u32, &[u8]) + Send + Sync>;

impl Camera {
    /// Starts the default camera. `send` gets I420 frames for the call,
    /// `preview` RGBA ones (mirrored, as people expect to see themselves).
    pub fn start(send: OnFrame, preview: OnFrame) -> Result<Self, String> {
        gst::init().map_err(|e| e.to_string())?;
        let desc = format!(
            "v4l2src ! videoconvert ! videoscale ! videorate \
             ! video/x-raw,width={WIDTH},height={HEIGHT},framerate=30/1 ! tee name=t \
             t. ! queue leaky=downstream max-size-buffers=1 ! videoconvert \
               ! video/x-raw,format=I420 ! appsink name=send sync=false max-buffers=1 drop=true \
             t. ! queue leaky=downstream max-size-buffers=1 ! videoflip method=horizontal-flip \
               ! videoscale ! videoconvert \
               ! video/x-raw,format=RGBA,width={PREVIEW_WIDTH},height={PREVIEW_HEIGHT} \
               ! appsink name=preview sync=false max-buffers=1 drop=true"
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| e.to_string())?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "not a pipeline".to_owned())?;
        for (name, deliver) in [("send", send), ("preview", preview)] {
            let sink = pipeline
                .by_name(name)
                .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
                .ok_or("no appsink")?;
            sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        let size = sample.caps().and_then(|c| c.structure(0)).and_then(|st| {
                            Some((st.get::<i32>("width").ok()?, st.get::<i32>("height").ok()?))
                        });
                        if let (Some(buf), Some((w, h))) = (sample.buffer(), size)
                            && let Ok(map) = buf.map_readable()
                        {
                            deliver(w as u32, h as u32, map.as_slice());
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| "the camera didn't start (is another app using it?)".to_owned())?;
        Ok(Self { pipeline })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Opens this machine's camera for two seconds and counts the frames;
    /// none are kept. Run by hand: `cargo test -p threnody-desktop --
    /// --ignored camera`.
    #[test]
    #[ignore = "needs a camera"]
    fn camera_delivers_both_streams() {
        let (sent, shown) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (s, p) = (sent.clone(), shown.clone());
        let cam = Camera::start(
            Box::new(move |w, h, i420| {
                assert_eq!((w, h), (WIDTH, HEIGHT));
                assert!(i420.len() >= (w * h * 3 / 2) as usize);
                s.fetch_add(1, Ordering::Relaxed);
            }),
            Box::new(move |w, h, rgba| {
                assert_eq!((w, h), (PREVIEW_WIDTH, PREVIEW_HEIGHT));
                assert_eq!(rgba.len(), (w * h * 4) as usize);
                p.fetch_add(1, Ordering::Relaxed);
            }),
        )
        .expect("camera");
        std::thread::sleep(std::time::Duration::from_secs(2));
        drop(cam);
        let (sent, shown) = (sent.load(Ordering::Relaxed), shown.load(Ordering::Relaxed));
        eprintln!("{sent} frames for the call, {shown} previews in 2 s");
        assert!(sent >= 20 && shown >= 20);
    }
}
