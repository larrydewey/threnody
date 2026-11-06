//! Group calls (Appendix Q, *Group calls*): up to eight people in a group,
//! a leg of media between every two, relayed by one participant (the hub)
//! for pairs without a session of their own. Signalling goes in group
//! messages; with the `calls` feature, the media runs here too.

use threnody_core::PublicIdentity;
use threnody_net::Event;

use crate::groups::hex;
use crate::{NodeEvent, Result, ThrenodyNode, fail, fp};

/// A group call, as an app shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupCallRecord {
    pub id: u64,
    /// Hex group id.
    pub group: String,
    /// The device relaying for pairs without a session.
    pub hub: String,
    /// Devices in the call other than us.
    pub participants: Vec<GroupCallParticipant>,
    /// We're in it; else it rings here.
    pub joined: bool,
    pub video: bool,
    pub started_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupCallParticipant {
    pub device: String,
    pub video: bool,
}

/// A frame of one participant's video, ready to draw.
#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct GroupVideoFrame {
    pub device: String,
    pub width: u32,
    pub height: u32,
    /// Degrees clockwise to turn it to be upright.
    pub rotation: u32,
    /// `width × height` pixels, 4 bytes each: red, green, blue, alpha.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for GroupVideoFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GroupVideoFrame({}, {}x{}, {}°)",
            self.device, self.width, self.height, self.rotation
        )
    }
}

/// A group call's media, when this build has it.
#[cfg(feature = "calls")]
pub(crate) type GroupMedia = std::sync::Mutex<Option<std::sync::Arc<threnody_media::GroupMedia>>>;
#[cfg(not(feature = "calls"))]
pub(crate) type GroupMedia = ();

/// Leg signalling the media sends, in order, to the group layer.
pub(crate) type LegSignals = tokio::sync::mpsc::UnboundedSender<(u64, PublicIdentity, Vec<u8>)>;

pub(crate) fn convert(e: &Event) -> Option<NodeEvent> {
    Some(match e {
        Event::GroupCallRinging { group, call, from } => NodeEvent::GroupCallRinging {
            group: hex(group),
            call: *call,
            from: fp(from),
        },
        Event::GroupCallMember {
            group,
            call,
            device,
            joined,
        } => NodeEvent::GroupCallMember {
            group: hex(group),
            call: *call,
            device: fp(device),
            joined: *joined,
        },
        Event::GroupCallVideo {
            group,
            call,
            device,
            video,
        } => NodeEvent::GroupCallVideo {
            group: hex(group),
            call: *call,
            device: fp(device),
            video: *video,
        },
        Event::GroupCallEnded { group, call } => NodeEvent::GroupCallEnded {
            group: hex(group),
            call: *call,
        },
        _ => return None,
    })
}

impl ThrenodyNode {
    /// What group call events mean for the media, before the app sees
    /// them.
    pub(crate) fn group_call_event(&self, e: &Event) {
        #[cfg(feature = "calls")]
        match e {
            Event::GroupCallMember {
                call,
                device,
                joined,
                ..
            } => {
                let Some(m) = self.group_media_for(*call) else {
                    return;
                };
                if *joined {
                    let device = *device;
                    self.rt.spawn(async move {
                        let _ = m.add(device).await;
                    });
                } else {
                    m.remove(device);
                }
            }
            Event::GroupLegSignal { call, from, data } => {
                self.group_leg_signal(*call, *from, data.clone())
            }
            Event::GroupCallEnded { call, .. } => {
                let mut gm = self
                    .group_media
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if gm.as_ref().is_some_and(|m| m.call() == *call) {
                    *gm = None;
                }
            }
            _ => {}
        }
        #[cfg(not(feature = "calls"))]
        let _ = e;
    }

    /// Signalling for our leg with `from`, from a session or a group
    /// message: for the media only, never the app or its logs.
    pub(crate) fn group_leg_signal(&self, call: u64, from: PublicIdentity, data: Vec<u8>) {
        #[cfg(feature = "calls")]
        if let Some(m) = self.group_media_for(call) {
            self.rt.spawn(async move { m.signal(from, data).await });
        }
        #[cfg(not(feature = "calls"))]
        let _ = (call, from, data);
    }

    #[cfg(feature = "calls")]
    fn group_media_for(&self, call: u64) -> Option<std::sync::Arc<threnody_media::GroupMedia>> {
        self.group_media
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .filter(|m| m.call() == call)
            .cloned()
    }

    /// Starts the media for group call `call` (just joined or started),
    /// with a leg to everyone already in it.
    fn start_group_media(&self, call: u64, video: bool) {
        #[cfg(feature = "calls")]
        {
            let signals = self.leg_signals.clone();
            let queued = self.queued.clone();
            let m = std::sync::Arc::new(threnody_media::GroupMedia::new(
                self.node.clone(),
                call,
                self.media.devices(),
                video,
                std::sync::Arc::new(move |peer, data| {
                    let _ = signals.send((call, peer, data));
                }),
                std::sync::Arc::new(move || {
                    queued
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push_back(NodeEvent::CallMedia {
                            call,
                            state: "connected".to_owned(),
                        });
                }),
            ));
            *self
                .group_media
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(m.clone());
            let peers: Vec<_> = self
                .node
                .group_call()
                .map(|c| c.participants.into_iter().map(|(p, _)| p).collect())
                .unwrap_or_default();
            self.rt.spawn(async move {
                for p in peers {
                    let _ = m.add(p).await;
                }
            });
        }
        #[cfg(not(feature = "calls"))]
        let _ = (call, video);
    }
}

#[uniffi::export]
impl ThrenodyNode {
    /// Starts a call in `group` (hex id), ringing every other member;
    /// we relay for pairs without a session. Returns its id.
    pub fn start_group_call(&self, group: String, video: bool) -> Result<u64> {
        let g = self.group_id(&group)?;
        let _guard = self.rt.enter();
        let call = self
            .group_node()
            .start_call(&self.node, &g, video)
            .map_err(fail)?;
        self.start_group_media(call, video);
        Ok(call)
    }

    /// Joins the group call ringing here (`GroupCallRinging`).
    pub fn join_group_call(&self, call: u64, video: bool) -> Result<()> {
        let _guard = self.rt.enter();
        self.group_node()
            .join_call(&self.node, call, video)
            .map_err(fail)?;
        self.start_group_media(call, video);
        Ok(())
    }

    /// Leaves the group call, or stops it ringing here.
    pub fn leave_group_call(&self) {
        let Some(c) = self.node.group_call() else {
            return;
        };
        let _guard = self.rt.enter();
        self.group_node().leave_call(&self.node, c.call);
        #[cfg(feature = "calls")]
        self.group_media
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    /// Turns our video in the group call on or off.
    pub fn set_group_call_video(&self, on: bool) -> Result<()> {
        let c = self
            .node
            .group_call()
            .filter(|c| c.joined)
            .ok_or_else(|| fail("not in a group call"))?;
        let _guard = self.rt.enter();
        self.group_node()
            .set_call_video(&self.node, c.call, on)
            .map_err(fail)?;
        #[cfg(feature = "calls")]
        if let Some(m) = self.group_media_for(c.call) {
            m.set_video(on);
        }
        Ok(())
    }

    /// The group call we're in or that rings here, if any.
    pub fn current_group_call(&self) -> Option<GroupCallRecord> {
        self.node.group_call().map(|c| GroupCallRecord {
            id: c.call,
            group: hex(&c.group),
            hub: fp(&c.hub),
            participants: c
                .participants
                .iter()
                .map(|(d, video)| GroupCallParticipant {
                    device: fp(d),
                    video: *video,
                })
                .collect(),
            joined: c.joined,
            video: c.video,
            started_ms: c.started_ms,
        })
    }

    /// The newest frame from some participant's video, waiting up to
    /// `timeout_ms`; each is returned once, and participants take turns.
    pub fn next_group_video_frame(&self, timeout_ms: u32) -> Option<GroupVideoFrame> {
        #[cfg(feature = "calls")]
        {
            let m = self
                .node
                .group_call()
                .and_then(|c| self.group_media_for(c.call))?;
            let wait = std::time::Duration::from_millis(u64::from(timeout_ms));
            let (device, f) = self.rt.block_on(m.next_frame(wait))?;
            let f = threnody_media::driver::pixels(&f);
            Some(GroupVideoFrame {
                device: fp(&device),
                width: f.width,
                height: f.height,
                rotation: f.rotation,
                rgba: f.rgba,
            })
        }
        #[cfg(not(feature = "calls"))]
        {
            let _ = timeout_ms;
            None
        }
    }
}
