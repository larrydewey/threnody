//! The ringback tone: what a caller hears while the call rings at the
//! other end. Without it, a call that rang sounded the same as one that
//! went nowhere.
//!
//! A 425 Hz tone, one second on and three off (the ITU's recommended
//! cadence), through the desktop's default output with GStreamer.

use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::glib;

/// Seconds of tone, then of silence, in each cycle.
const ON_S: u32 = 1;
const CYCLE_S: u32 = 4;

pub struct Ringback {
    pipeline: gst::Element,
    cadence: Option<glib::SourceId>,
}

impl Ringback {
    /// Starts ringing; `None` when there's no audio output to ring on.
    pub fn start() -> Option<Self> {
        gst::init().ok()?;
        let pipeline = gst::parse::launch(
            "audiotestsrc wave=sine freq=425 volume=0.12 is-live=true ! audioconvert ! autoaudiosink",
        )
        .ok()?;
        pipeline.set_state(gst::State::Playing).ok()?;
        let p = pipeline.clone();
        let mut second = 0u32;
        let cadence = glib::timeout_add_seconds_local(1, move || {
            second = (second + 1) % CYCLE_S;
            let state = if second < ON_S {
                gst::State::Playing
            } else {
                gst::State::Paused
            };
            let _ = p.set_state(state);
            glib::ControlFlow::Continue
        });
        Some(Self {
            pipeline,
            cadence: Some(cadence),
        })
    }
}

impl Drop for Ringback {
    fn drop(&mut self) {
        if let Some(c) = self.cadence.take() {
            c.remove();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
