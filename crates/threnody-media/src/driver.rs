//! Runs a node's call media for an application: starts it when a call is
//! answered, hands it the peer's signalling (holding what arrives before
//! the media exists), and ends it with the call.
//!
//! The application forwards three things: that a call started
//! ([`Driver::start`]: the callee right after answering, the caller on
//! `Event::CallStarted`), each `Event::CallSignal` ([`Driver::signal`]),
//! and `Event::CallEnded` ([`Driver::end`]).

use std::sync::Arc;

use threnody_net::Node;
use tokio::sync::mpsc;

use crate::CallMedia;
use crate::rtc::{AudioIn, PeerConnectionState};

enum Cmd {
    Start(u64),
    Signal(u64, Vec<u8>),
    Mute(bool),
    End,
}

/// How the call's media is doing, for the user interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaState {
    /// Audio flows both ways.
    Connected,
    /// It stopped flowing; WebRTC is trying to recover.
    Interrupted,
    /// It couldn't start, or couldn't recover: the call was hung up.
    Failed,
}

pub type OnState = Arc<dyn Fn(u64, MediaState) + Send + Sync>;

pub struct Driver {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Driver {
    /// Starts the driver's task on the current Tokio runtime.
    pub fn new(node: Node, on_state: OnState) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(node, rx, on_state));
        Self { tx }
    }

    pub fn start(&self, call: u64) {
        let _ = self.tx.send(Cmd::Start(call));
    }

    pub fn signal(&self, call: u64, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Signal(call, data));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.tx.send(Cmd::Mute(muted));
    }

    pub fn end(&self) {
        let _ = self.tx.send(Cmd::End);
    }
}

async fn run(node: Node, mut rx: mpsc::UnboundedReceiver<Cmd>, on_state: OnState) {
    let mut media: Option<(u64, CallMedia)> = None;
    let mut early: Vec<(u64, Vec<u8>)> = Vec::new();
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Start(call) => {
                let report = on_state.clone();
                let failed = node.clone();
                let started = CallMedia::start(
                    &node,
                    call,
                    AudioIn::Device,
                    |_| {},
                    move |s| match s {
                        PeerConnectionState::Connected => report(call, MediaState::Connected),
                        PeerConnectionState::Disconnected => report(call, MediaState::Interrupted),
                        PeerConnectionState::Failed => {
                            report(call, MediaState::Failed);
                            failed.fail_call(call);
                        }
                        _ => {}
                    },
                )
                .await;
                match started {
                    Ok(m) => {
                        for (_, d) in early.drain(..).filter(|(c, _)| *c == call) {
                            let _ = m.on_signal(&d).await;
                        }
                        media = Some((call, m));
                    }
                    Err(_) => {
                        on_state(call, MediaState::Failed);
                        node.fail_call(call);
                    }
                }
            }
            Cmd::Signal(call, d) => match &media {
                Some((c, m)) if *c == call => {
                    let _ = m.on_signal(&d).await;
                }
                _ => early.push((call, d)),
            },
            Cmd::Mute(muted) => {
                if let Some((_, m)) = &media {
                    m.set_muted(muted);
                }
            }
            Cmd::End => {
                media = None;
                early.clear();
            }
        }
    }
}
