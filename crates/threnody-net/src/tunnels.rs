//! Tunnel peers kept across restarts and in step with mutual approval
//! (Appendix D, *Persistence*).
//!
//! While tunnels are on, the peer set (identity, WireGuard key, endpoint,
//! the session-exported PSK and its age) is kept in the encrypted
//! `tunnels` state and comes back when tunnels next turn on, so a tunnel
//! works again before, or without, a new session. Only peers of mutually
//! approved contacts whose PSK is younger than `KEEP_FOR_MS` are kept.
//! Turning tunnels off deletes the file; personas never write it.
//!
//! Locks: `tunnel_io` is outermost, held around turning tunnels off and
//! each read or write of the file, and re-checks `tunnel` under it, so a
//! save racing with "tunnels off" can't bring the file back. Of `tunnel`,
//! `tunnel_peers` and `contacts`, none is held while taking another, but
//! for `tunnel` read under `tunnel_peers` when an offer is recorded
//! (never the other way round).

use threnody_core::tunnel::{self, STATE, SavedPeer, overlay_addr};
use threnody_core::{AppMessage, PublicIdentity, now_ms};

use crate::node::{Node, Secret, Shared, TunnelPeer, lock};

/// Endpoint-only changes from the userspace device are saved at most
/// this often (ms); in between they wait for the next save of any kind.
#[cfg(feature = "boringtun")]
const SEEN_SAVE_EVERY_MS: u64 = 30_000;

impl Shared {
    /// Sets (or clears) our tunnel port and key, returning what it was.
    /// Turning tunnels off forgets every peer and deletes the saved ones.
    pub(crate) fn set_tunnel(&self, new: Option<(u16, [u8; 32])>) -> Option<(u16, [u8; 32])> {
        let _io = lock(&self.tunnel_io);
        let was = std::mem::replace(&mut *lock(&self.tunnel), new);
        if new.is_none() {
            lock(&self.tunnel_peers).clear();
            self.remove_saved_tunnels();
        }
        was
    }

    /// Saves every fresh peer of a mutually approved contact; removes the
    /// file when none are left. Does nothing when tunnels are off, or for
    /// a persona.
    pub(crate) fn save_tunnels(&self) {
        self.save_tunnels_at(now_ms());
    }

    fn save_tunnels_at(&self, now: u64) {
        if self.persona {
            return;
        }
        let _io = lock(&self.tunnel_io);
        if lock(&self.tunnel).is_none() {
            return;
        }
        let peers: Vec<(PublicIdentity, TunnelPeer)> = lock(&self.tunnel_peers)
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let saved: Vec<SavedPeer> = peers
            .into_iter()
            .filter(|(p, _)| self.mutual(p))
            .map(|(peer, tp)| SavedPeer {
                peer,
                wg_public: tp.wg_public,
                endpoint: tp.seen.unwrap_or(tp.endpoint),
                psk: tp.psk.0,
                made_ms: tp.made_ms,
            })
            .filter(|s| s.fresh(now))
            .collect();
        if saved.is_empty() {
            self.remove_saved_tunnels();
        } else {
            self.save_state(STATE, tunnel::encode_saved(&saved));
        }
    }

    /// Merges saved peers into `tunnel_peers` (those already there win),
    /// skipping stale ones, contacts no longer mutually approved and keys
    /// that are ours or another peer's, then saves back what it kept. A
    /// file that can't be read is deleted: a lost tunnel comes back with
    /// the next session. Does nothing when tunnels are off, or for a
    /// persona.
    pub(crate) fn restore_tunnels(&self) {
        if self.persona {
            return;
        }
        {
            let _io = lock(&self.tunnel_io);
            let Some((_, ours)) = *lock(&self.tunnel) else {
                return;
            };
            let saved = self
                .home
                .load_state(&self.identity, STATE)
                .and_then(|b| b.map(|b| tunnel::decode_saved(&b)).transpose());
            let saved = match saved {
                Ok(Some(s)) => s,
                Ok(None) => return,
                Err(e) => {
                    eprintln!("threnody: forgetting saved tunnels: {e}");
                    self.remove_saved_tunnels();
                    return;
                }
            };
            let now = now_ms();
            let keep: Vec<SavedPeer> = saved
                .into_iter()
                .filter(|s| s.fresh(now) && s.wg_public != ours && self.mutual(&s.peer))
                .collect();
            let mut peers = lock(&self.tunnel_peers);
            for s in keep {
                if peers.contains_key(&s.peer) || peers.values().any(|p| p.wg_public == s.wg_public)
                {
                    continue;
                }
                peers.insert(
                    s.peer,
                    TunnelPeer {
                        wg_public: s.wg_public,
                        endpoint: s.endpoint,
                        overlay: overlay_addr(&s.peer),
                        psk: Secret(s.psk),
                        made_ms: s.made_ms,
                        seen: None,
                    },
                );
            }
        }
        // Writes back only what was kept: stale and revoked peers go.
        self.save_tunnels();
    }

    /// Writes the saved peers back without those whose PSK aged past
    /// `KEEP_FOR_MS` by `now`, so the bound holds on a node that runs for
    /// weeks without a tunnel change (the history sweep calls this each
    /// minute). Reads the file only while a peer in memory is that old: the
    /// file holds no peer that memory doesn't.
    pub(crate) fn expire_saved_tunnels(&self, now: u64) {
        if self.persona || lock(&self.tunnel).is_none() {
            return;
        }
        if !lock(&self.tunnel_peers)
            .values()
            .any(|tp| !tunnel::fresh(tp.made_ms, now))
        {
            return;
        }
        let stale = {
            let _io = lock(&self.tunnel_io);
            match self.home.load_state(&self.identity, STATE) {
                Ok(Some(b)) => {
                    tunnel::decode_saved(&b).map_or(true, |s| s.iter().any(|p| !p.fresh(now)))
                }
                Ok(None) => false,
                Err(_) => true,
            }
        };
        if stale {
            self.save_tunnels_at(now);
        }
    }

    /// Overwrites and deletes the saved peers. The caller holds `tunnel_io`.
    fn remove_saved_tunnels(&self) {
        if let Err(e) = self.home.secure_remove_state(STATE) {
            eprintln!("threnody: failed to delete saved tunnels: {e}");
        }
    }

    /// Takes down every tunnel peer that is no longer mutually approved
    /// (blocked, deleted, revoked on another of our devices, or a device
    /// that left its account): `tunnel_down` for each, which saves. Paths
    /// that end approval without an `Approval` message call this.
    pub(crate) fn prune_tunnels(&self) {
        let peers: Vec<PublicIdentity> = lock(&self.tunnel_peers).keys().copied().collect();
        for p in peers.iter().filter(|p| !self.mutual(p)) {
            self.tunnel_down(p);
        }
    }
}

impl Node {
    /// Sends our `TunnelOffer` on every direct session with a mutually
    /// approved peer (tunnels on): after the port changes, tunnels turn on
    /// or the network changes. The peer takes a repeat as an endpoint
    /// update.
    pub(crate) fn reoffer_tunnels(&self) {
        let Some((port, wg_public)) = *lock(&self.shared.tunnel) else {
            return;
        };
        for s in self.sessions() {
            if s.via.is_none() && self.shared.mutual(&s.peer) {
                let _ = self.send(&s.peer, AppMessage::TunnelOffer { wg_public, port });
            }
        }
    }

    /// The userspace device took a packet that `peer`'s key (`wg_public`)
    /// authenticates, over UDP from `from`: remember where the peer is, so
    /// the saved endpoint is the last one that worked. No event: the key
    /// and PSK are unchanged, and kernel WireGuard roams by itself.
    #[cfg(feature = "boringtun")]
    pub(crate) fn tunnel_endpoint_seen(
        &self,
        peer: &PublicIdentity,
        wg_public: &[u8; 32],
        from: std::net::SocketAddr,
    ) {
        use std::sync::atomic::Ordering;
        let changed = {
            let mut peers = lock(&self.shared.tunnel_peers);
            let Some(tp) = peers.get_mut(peer).filter(|tp| tp.wg_public == *wg_public) else {
                return;
            };
            let seen = (from != tp.endpoint).then_some(from);
            std::mem::replace(&mut tp.seen, seen) != seen
        };
        let now = now_ms();
        let last = self.shared.tunnel_seen_saved_ms.load(Ordering::Relaxed);
        if changed
            && now.saturating_sub(last) >= SEEN_SAVE_EVERY_MS
            && self
                .shared
                .tunnel_seen_saved_ms
                .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.shared.save_tunnels();
        }
    }
}

impl Node {
    /// Tests only: what the history sweep does each minute, at `now_ms`
    /// (see `Shared::expire_saved_tunnels`).
    #[doc(hidden)]
    pub fn expire_saved_tunnels_at(&self, now_ms: u64) {
        self.shared.expire_saved_tunnels(now_ms);
    }
}
