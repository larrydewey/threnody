//! Voice calls (Appendix Q). Signalling always works; audio needs the
//! `calls` feature (WebRTC), which drives the platform's microphone and
//! speaker itself, so apps only show the call and its controls.

use std::sync::Arc;

use threnody_core::call::HangupReason;
use threnody_net::CallPhase;

use crate::{NodeEvent, Result, ThrenodyNode, fail, fp};

/// The current call, as an app shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CallRecord {
    pub id: u64,
    pub peer: String,
    pub outgoing: bool,
    /// "calling", "ringing", "incoming" or "active".
    pub phase: String,
    pub video: bool,
    pub peer_video: bool,
    pub started_ms: u64,
    /// When it was answered (0 = not yet).
    pub answered_ms: u64,
}

/// Words for why a call ended, for `NodeEvent::CallEnded`.
pub(crate) fn reason(r: HangupReason) -> String {
    match r {
        HangupReason::Ended => "ended",
        HangupReason::Declined => "declined",
        HangupReason::Busy => "busy",
        HangupReason::Unanswered => "unanswered",
        HangupReason::Failed => "failed",
        HangupReason::AnsweredElsewhere => "answered elsewhere",
        HangupReason::DeclinedElsewhere => "declined elsewhere",
    }
    .to_owned()
}

/// The media driver, when this build has one.
#[cfg(feature = "calls")]
pub(crate) type Media = threnody_media::Driver;
/// No call audio in this build.
#[cfg(not(feature = "calls"))]
pub(crate) struct Media;

/// Starts the media driver; its state changes are queued as events.
#[cfg(feature = "calls")]
pub(crate) fn media(
    node: &threnody_net::Node,
    queued: Arc<std::sync::Mutex<std::collections::VecDeque<NodeEvent>>>,
) -> Media {
    use threnody_media::MediaState;
    threnody_media::Driver::new(
        node.clone(),
        Arc::new(move |call, s| {
            let state = match s {
                MediaState::Connected => "connected",
                MediaState::Interrupted => "interrupted",
                MediaState::Failed => "failed",
            };
            queued
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(NodeEvent::CallMedia {
                    call,
                    state: state.to_owned(),
                });
        }),
    )
}

#[cfg(not(feature = "calls"))]
pub(crate) fn media(
    _: &threnody_net::Node,
    _: Arc<std::sync::Mutex<std::collections::VecDeque<NodeEvent>>>,
) -> Media {
    Media
}

impl ThrenodyNode {
    /// Call events the media needs to see, before the app sees them.
    pub(crate) fn call_event(&self, e: &threnody_net::Event) {
        #[cfg(feature = "calls")]
        match e {
            threnody_net::Event::CallStarted { call, .. } => {
                // The callee started its media when it answered.
                if self
                    .node
                    .call()
                    .is_some_and(|c| c.call == *call && c.outgoing)
                {
                    self.media.start(*call);
                }
            }
            threnody_net::Event::CallSignal { call, data, .. } => {
                self.media.signal(*call, data.clone());
            }
            threnody_net::Event::CallEnded { .. } => self.media.end(),
            _ => {}
        }
        #[cfg(not(feature = "calls"))]
        let _ = e;
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Whether this build carries call audio (else calls ring and can be
    /// answered, but stay silent).
    pub fn calls_supported(&self) -> bool {
        cfg!(feature = "calls")
    }

    /// Calls `peer` (connected, and with an app that takes calls). Events
    /// follow: `CallRinging`, then `CallStarted` or `CallEnded`.
    pub fn start_call(&self, peer: String, video: bool) -> Result<u64> {
        let p = self.resolve(&peer)?;
        // The call's watchdog runs on the node's runtime.
        let _guard = self.rt.enter();
        self.node.start_call(&p, video).map_err(fail)
    }

    /// Answers the incoming call `id`; its audio starts at once.
    pub fn answer_call(&self, id: u64, video: bool) -> Result<()> {
        let _guard = self.rt.enter();
        self.node.answer_call(id, video).map_err(fail)?;
        #[cfg(feature = "calls")]
        self.media.start(id);
        Ok(())
    }

    /// Hangs up, declines (when it's ringing here) or cancels call `id`.
    pub fn hangup_call(&self, id: u64) {
        self.node.hangup_call(id);
    }

    /// Mutes or unmutes our microphone in the current call.
    pub fn set_call_muted(&self, muted: bool) {
        #[cfg(feature = "calls")]
        self.media.set_muted(muted);
        #[cfg(not(feature = "calls"))]
        let _ = muted;
    }

    /// The current call, if any.
    pub fn current_call(&self) -> Option<CallRecord> {
        self.node.call().map(|c| CallRecord {
            id: c.call,
            peer: fp(&c.peer),
            outgoing: c.outgoing,
            phase: match c.phase {
                CallPhase::Calling => "calling",
                CallPhase::Ringing => "ringing",
                CallPhase::Incoming => "incoming",
                CallPhase::Active => "active",
            }
            .to_owned(),
            video: c.video,
            peer_video: c.peer_video,
            started_ms: c.started_ms,
            answered_ms: c.answered_ms,
        })
    }
}
