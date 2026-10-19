//! Waking a sleeping app with UnifiedPush (Appendix R).

use std::time::Duration;

use threnody_core::push::{PushTarget, WebPushKeys};
use threnody_net::push::DEFAULT_DUMMIES_PER_DAY;

use crate::{Result, ThrenodyNode, fail};

/// Where contacts wake this device: a UnifiedPush endpoint, with the Web
/// Push keys (RFC 8291) the distributor gave, decoded from base64url.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PushEndpoint {
    pub url: String,
    /// P-256 public key, uncompressed (65 bytes).
    pub public_key: Option<Vec<u8>>,
    /// Authentication secret (16 bytes).
    pub auth: Option<Vec<u8>>,
}

/// What a wake-up sync found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct WakeReport {
    /// Mutually approved contacts.
    pub contacts: u32,
    /// Of them, connected when the sync ended.
    pub connected: u32,
}

#[uniffi::export]
impl ThrenodyNode {
    /// Lets mutually approved contacts wake this device at `endpoint` when
    /// they hold a message for it; `None` withdraws it. Off unless set: a
    /// push server then sees when this device is woken, and the addresses
    /// of the contacts that wake it (Appendix R). Refused for anonymous
    /// identities.
    pub fn set_push_endpoint(&self, endpoint: Option<PushEndpoint>) -> Result<()> {
        let target = match endpoint {
            None => None,
            Some(e) => {
                let keys = match (e.public_key, e.auth) {
                    (Some(p), Some(a)) => Some(WebPushKeys {
                        p256dh: p
                            .try_into()
                            .map_err(|_| fail("push public key must be 65 bytes"))?,
                        auth: a
                            .try_into()
                            .map_err(|_| fail("push auth secret must be 16 bytes"))?,
                    }),
                    (None, None) => None,
                    _ => return Err(fail("push keys come in pairs")),
                };
                Some(PushTarget {
                    url: e.url,
                    keys,
                    dummies_per_day: DEFAULT_DUMMIES_PER_DAY,
                    expires_ms: 0,
                })
            }
        };
        let _guard = self.rt.enter();
        self.node.set_push_target(target).map_err(fail)
    }

    /// The endpoint contacts may wake us at, if any.
    pub fn push_endpoint(&self) -> Option<String> {
        self.node.push_target().map(|t| t.url)
    }

    /// How many contacts gave us an endpoint to wake them at.
    pub fn push_wakeable_contacts(&self) -> u32 {
        u32::try_from(self.node.push_targets()).unwrap_or(u32::MAX)
    }

    /// Woken by a push: dials every mutually approved contact and returns
    /// once what they held has had time to arrive (messages come as the
    /// usual events), or after `timeout_ms`. Blocks; call it from a
    /// background thread with the node listening.
    pub fn wake_sync(&self, timeout_ms: u32) -> WakeReport {
        let r = self.rt.block_on(
            self.node
                .wake_sync(Duration::from_millis(u64::from(timeout_ms))),
        );
        WakeReport {
            contacts: u32::try_from(r.contacts).unwrap_or(u32::MAX),
            connected: u32::try_from(r.connected).unwrap_or(u32::MAX),
        }
    }
}
